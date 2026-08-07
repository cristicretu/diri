//! diri's self-updater.
//!
//! Replaces what Sparkle does for the Swift app, without Sparkle: diri is a
//! Rust binary, and the Swift app's appcast carries EdDSA signatures tied to a
//! keypair this app has no way to use. The releases host is shared — the same
//! password-gated Cloudflare Worker — but diri reads a JSON feed under
//! `/diri/` and trusts Apple's Developer ID + notarization rather than a
//! project-managed key. See [`codesign`] for why.
//!
//! The flow, each step gated on the previous one:
//!
//! 1. [`Updater::check`] — fetch the feed, pick the newest eligible release.
//! 2. [`Updater::download`] — fetch the zip, match its size and sha256.
//! 3. [`Updater::stage`] — unpack it, verify the signature against our own.
//! 4. [`Updater::install`] — hand off to the swap helper, then quit.
//!
//! Every call blocks; the app runs them on a worker thread.

pub mod bundle;
pub mod codesign;
pub mod error;
pub mod feed;
pub mod install;
pub mod net;
pub mod version;

use std::path::{Path, PathBuf};

pub use error::{Result, UpdateError};
pub use feed::{Eligibility, Feed, Release};
pub use version::Version;

use codesign::SignatureInfo;
use net::Http;

pub(crate) const AGENT: &str = env!("CARGO_PKG_VERSION");

/// Host serving both the feed and the archives. Downloads are pinned to it.
///
/// Only the URL the feed names is checked against this; curl still follows the
/// redirect GitHub issues to its asset CDN, which is what release downloads do.
pub const RELEASES_HOST: &str = "github.com";
/// The feed is published as an asset on every release, so `latest` is a stable
/// URL that always resolves to the newest one.
pub const DEFAULT_FEED_URL: &str =
    "https://github.com/cristicretu/diri/releases/latest/download/appcast.json";

/// Set to `1` to let an unsigned local build run the whole flow. Only useful
/// for exercising the updater against a test feed; the signature check still
/// runs, so the download must still be a real notarized bundle.
pub const ALLOW_UNSIGNED_ENV: &str = "DIRI_UPDATER_ALLOW_UNSIGNED";
/// Overrides the feed URL, for staging a release before it goes live.
pub const FEED_URL_ENV: &str = "DIRI_UPDATE_FEED";

#[derive(Clone, Debug)]
pub struct UpdaterConfig {
    pub feed_url: String,
    pub current_version: Version,
    /// The `.app` that will be replaced.
    pub bundle: PathBuf,
    /// Scratch space for downloads and staged bundles.
    pub cache_dir: PathBuf,
    /// Signature of the running build, which every download is pinned to.
    pub installed_signature: SignatureInfo,
}

impl UpdaterConfig {
    /// Builds the configuration for the running app, or explains why this
    /// build cannot update itself.
    ///
    /// `current_version` comes from the caller rather than the bundle's
    /// Info.plist so the app and the updater agree on one source of truth —
    /// `CARGO_PKG_VERSION`, which is also what cargo-packager stamps into the
    /// plist at package time.
    pub fn for_running_app(current_version: &str) -> Result<Self> {
        let bundle = bundle::running_bundle().ok_or_else(|| {
            UpdateError::NotUpdatable("diri is not running from an app bundle".to_owned())
        })?;
        let current = Version::parse(current_version).ok_or_else(|| {
            UpdateError::NotUpdatable(format!("unparseable app version {current_version:?}"))
        })?;
        let installed_signature = codesign::signature_of(&bundle).unwrap_or_default();
        let allow_unsigned = std::env::var_os(ALLOW_UNSIGNED_ENV).is_some_and(|value| value == "1");
        if !installed_signature.is_developer_id() && !allow_unsigned {
            return Err(UpdateError::NotUpdatable(
                "this build is not signed with a Developer ID".to_owned(),
            ));
        }

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| UpdateError::NotUpdatable("HOME is unset".to_owned()))?;
        Ok(Self {
            feed_url: std::env::var(FEED_URL_ENV).unwrap_or_else(|_| DEFAULT_FEED_URL.to_owned()),
            current_version: current,
            bundle,
            cache_dir: home.join("Library/Caches/diri/updates"),
            installed_signature,
        })
    }
}

/// A downloaded, unpacked, signature-checked bundle waiting to be swapped in.
#[derive(Clone, Debug)]
pub struct StagedUpdate {
    pub release: Release,
    pub app: PathBuf,
    /// Directory holding the staged app and the generated install script.
    pub directory: PathBuf,
}

pub struct Updater {
    config: UpdaterConfig,
    http: Http,
}

impl Updater {
    pub fn new(config: UpdaterConfig) -> Self {
        let http = Http::new();
        Self { config, http }
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    /// Fetches the feed and returns the release worth offering, if any.
    pub fn check(&self, skipped: Option<&str>) -> Result<Option<Release>> {
        let body = self.http.fetch_text(&self.config.feed_url)?;
        let feed = Feed::parse(&body).map_err(|error| UpdateError::Feed(error.to_string()))?;
        Ok(feed
            .newest_eligible(Eligibility {
                current: self.config.current_version,
                system: bundle::system_version(),
                skipped,
            })
            .cloned())
    }

    /// Downloads the release archive, verifying size and checksum.
    ///
    /// Checks the install location *first*: discovering that `/Applications`
    /// is read-only after pulling 50 MB wastes the user's bandwidth and their
    /// attention.
    pub fn download(&self, release: &Release, on_progress: impl FnMut(f32)) -> Result<PathBuf> {
        bundle::ensure_writable(&self.config.bundle)?;
        net::validated_download_url(&release.url, RELEASES_HOST)?;

        let directory = self.release_dir(release);
        std::fs::create_dir_all(&directory)?;
        let archive = directory.join("diri.zip");
        self.http
            .download(&release.url, &archive, release.size, on_progress)?;
        if let Some(expected) = &release.sha256 {
            net::verify_sha256(&archive, expected)?;
        }
        Ok(archive)
    }

    /// Unpacks the archive and refuses it unless it is a notarized build from
    /// the same developer as the running app.
    pub fn stage(&self, release: &Release, archive: &Path) -> Result<StagedUpdate> {
        let directory = self.release_dir(release);
        let app = install::unpack(archive, &directory.join("staged"))?;
        codesign::verify_matches_installed(&app, &self.config.installed_signature)?;
        // A tampered feed could advertise 0.9.0 and serve the 0.1.0 archive.
        // The signature check would pass — it is a real diri build — so the
        // unpacked bundle has to be held to the version that was promised.
        verify_staged_version(&app, release)?;
        // Reclaim the download now that its contents are unpacked.
        let _ = std::fs::remove_file(archive);
        Ok(StagedUpdate {
            release: release.clone(),
            app,
            directory,
        })
    }

    /// Starts the swap helper. The caller must quit the app immediately after
    /// this returns.
    pub fn install(&self, staged: &StagedUpdate) -> Result<()> {
        bundle::ensure_writable(&self.config.bundle)?;
        install::launch_installer(&staged.app, &self.config.bundle, &staged.directory)
    }

    /// Removes staging directories left behind by earlier updates. Cheap, and
    /// called at launch so a failed install does not leak a bundle-sized
    /// directory into the cache forever.
    pub fn clean_cache(&self) {
        let Ok(entries) = std::fs::read_dir(&self.config.cache_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry
                .file_name()
                .to_str()
                .and_then(Version::parse)
                .is_some_and(|version| !version.is_newer_than(self.config.current_version));
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn release_dir(&self, release: &Release) -> PathBuf {
        // Feed-controlled string in a path component: keep it to the shape a
        // version actually has.
        let name = release
            .parsed_version()
            .map(|version| version.to_string())
            .unwrap_or_else(|| "pending".to_owned());
        self.config.cache_dir.join(name)
    }
}

/// Reads `CFBundleShortVersionString` out of a staged bundle's Info.plist.
fn verify_staged_version(app: &Path, release: &Release) -> Result<()> {
    let output = std::process::Command::new("/usr/bin/defaults")
        .arg("read")
        .arg(app.join("Contents/Info.plist"))
        .arg("CFBundleShortVersionString")
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::Integrity(
            "the staged bundle has no CFBundleShortVersionString".to_owned(),
        ));
    }
    let found = String::from_utf8_lossy(&output.stdout);
    let found = Version::parse(found.trim())
        .ok_or_else(|| UpdateError::Integrity(format!("unparseable staged version {found:?}")))?;
    let promised = release.parsed_version().ok_or_else(|| {
        UpdateError::Feed(format!("unparseable release version {:?}", release.version))
    })?;
    if found != promised {
        return Err(UpdateError::Integrity(format!(
            "the feed promised {promised} but the archive contains {found}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> UpdaterConfig {
        UpdaterConfig {
            feed_url: DEFAULT_FEED_URL.to_owned(),
            current_version: Version::new(0, 4, 2),
            bundle: PathBuf::from("/Applications/diri.app"),
            cache_dir: PathBuf::from("/tmp/diri-updates"),
            installed_signature: SignatureInfo::default(),
        }
    }

    #[test]
    fn a_bare_test_binary_is_not_updatable() {
        // The test harness is not a .app, which is the same situation as
        // `cargo run` — the updater must decline rather than guess.
        let error = UpdaterConfig::for_running_app("0.1.0")
            .expect_err("a loose binary has nothing to update");
        assert!(matches!(error, UpdateError::NotUpdatable(_)));
        assert_eq!(error.user_facing(), "Updates are off for this build");
    }

    /// Live check against the published feed: proves the curl config, the
    /// GitHub `latest` redirect, and the feed's shape all still line up.
    /// Ignored by default so offline runs and CI stay green; run it after
    /// publishing with `cargo test -p diri-updater -- --ignored`.
    #[test]
    #[ignore = "requires network access to the releases host"]
    fn the_published_feed_is_reachable_and_parses() {
        let http = Http::new();
        let body = http
            .fetch_text(DEFAULT_FEED_URL)
            .expect("the releases host serves the feed");
        let feed = Feed::parse(&body).expect("the published feed parses");
        assert!(
            !feed.releases.is_empty(),
            "the published feed lists no releases"
        );
        for release in &feed.releases {
            assert!(release.parsed_version().is_some(), "{release:?}");
            net::validated_download_url(&release.url, RELEASES_HOST).expect("pinned host");
        }
    }

    /// Live end-to-end of the half the feed test does not reach: actually pull
    /// the newest release's zip and put it through the real integrity and
    /// signature checks.
    ///
    /// This is the path GitHub's redirect runs through. `github.com` hands an
    /// asset request to `release-assets.githubusercontent.com`, so a curl
    /// config that failed to follow redirects — or a host pin applied to the
    /// post-redirect URL — would break every download while the feed kept
    /// parsing fine. Only downloading catches that.
    ///
    /// Ignored by default: it needs the network and pulls ~18 MB.
    #[test]
    #[ignore = "requires network access and downloads a release"]
    fn the_published_release_downloads_and_verifies() {
        let http = Http::new();
        let feed =
            Feed::parse(&http.fetch_text(DEFAULT_FEED_URL).expect("feed")).expect("feed parses");
        let release = feed
            .releases
            .iter()
            .max_by_key(|release| release.parsed_version().unwrap_or_default())
            .expect("the feed lists a release")
            .clone();

        let directory = tempfile::tempdir().expect("temp dir");
        let updater = Updater::new(UpdaterConfig {
            cache_dir: directory.path().to_path_buf(),
            // Pin to the Developer ID the releases are actually signed with, so
            // this asserts the published artifact is ours — not merely that
            // some notarized app came down the wire.
            installed_signature: SignatureInfo {
                identifier: Some("com.dirijor.diri".to_owned()),
                team_identifier: Some("A56RVNJ69X".to_owned()),
                authorities: vec![
                    "Developer ID Application: CRISTIAN EMANUEL CRETU (A56RVNJ69X)".to_owned(),
                ],
            },
            // A version below the release so `download` has something to fetch.
            current_version: Version::new(0, 0, 1),
            ..config()
        });

        let archive = updater
            .download(&release, |_| {})
            .expect("the release zip downloads and matches its sha256");
        let staged = updater
            .stage(&release, &archive)
            .expect("the download passes signature, Gatekeeper, and version checks");
        assert_eq!(staged.app.file_name().expect("a bundle"), "diri.app");
        assert_eq!(staged.release.version, release.version);
    }

    #[test]
    fn the_default_feed_lives_on_the_pinned_host() {
        assert!(DEFAULT_FEED_URL.starts_with(&format!("https://{RELEASES_HOST}/")));
    }

    #[test]
    fn staging_directories_are_named_by_normalized_version() {
        let updater = Updater::new(config());
        let release = Release {
            version: "0.5".to_owned(),
            ..Release::default()
        };
        assert_eq!(
            updater.release_dir(&release),
            PathBuf::from("/tmp/diri-updates/0.5.0")
        );
    }

    #[test]
    fn a_feed_version_that_is_not_a_version_cannot_escape_the_cache_directory() {
        let updater = Updater::new(config());
        let release = Release {
            version: "../../../Applications".to_owned(),
            ..Release::default()
        };
        assert_eq!(
            updater.release_dir(&release),
            PathBuf::from("/tmp/diri-updates/pending")
        );
    }

    #[test]
    fn cache_cleanup_keeps_directories_for_newer_versions() {
        let directory = tempfile::tempdir().expect("temp dir");
        let updater = Updater::new(UpdaterConfig {
            cache_dir: directory.path().to_path_buf(),
            ..config()
        });
        for name in ["0.3.0", "0.4.2", "0.5.0", "notes"] {
            std::fs::create_dir(directory.path().join(name)).expect("create");
        }
        updater.clean_cache();

        assert!(!directory.path().join("0.3.0").exists());
        assert!(
            !directory.path().join("0.4.2").exists(),
            "the current version is not pending"
        );
        assert!(directory.path().join("0.5.0").exists());
        assert!(
            directory.path().join("notes").exists(),
            "unrecognized entries are left alone"
        );
    }
}
