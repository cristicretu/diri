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
//!
//! Both fetches go to GitHub, and to the update [`mirror`] when GitHub's route
//! fails or (for the feed) is slow to answer. The mirror is held to the same
//! checks; see that module for why it cannot serve a different build.
//! 3. [`Updater::stage`] — unpack it, verify the signature against our own.
//! 4. [`Updater::install`] — hand off to the swap helper, then quit.
//!
//! Every call blocks; the app runs them on a worker thread.

pub mod bundle;
pub mod codesign;
pub mod error;
pub mod feed;
pub mod install;
pub mod mirror;
pub mod net;
pub mod version;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod windows_identity;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use error::{NetworkFailure, Result, UpdateError};
pub use feed::{Eligibility, Feed, Release};
pub use mirror::{Mirror, Source, SourceReport};
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
/// The nightly channel's feed: an asset on the rolling `nightly` prerelease,
/// which GitHub's `latest` alias never resolves to, so stable installs (and
/// builds that predate channels) cannot see it.
pub const NIGHTLY_FEED_URL: &str =
    "https://github.com/cristicretu/diri/releases/download/nightly/appcast.json";
/// Canonical release metadata. The update feed deliberately stays small and
/// archive-focused; GitHub owns the human-written release body shown by the
/// app's What's New page.
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/cristicretu/diri/releases/latest";
const MAX_RELEASE_METADATA_BYTES: usize = 512 * 1024;

/// Set to `1` to let an unsigned local build run the whole flow. Only useful
/// for exercising the updater against a test feed; the signature check still
/// runs, so the download must still be a real notarized bundle.
pub const ALLOW_UNSIGNED_ENV: &str = "DIRI_UPDATER_ALLOW_UNSIGNED";
/// Overrides the feed URL, for staging a release before it goes live. A
/// staging feed is not mirrored, so setting it also turns the mirror off.
pub const FEED_URL_ENV: &str = "DIRI_UPDATE_FEED";
/// Overrides the mirror host (`host[:port]`), or `off` to use GitHub only.
pub const MIRROR_ENV: &str = "DIRI_UPDATE_MIRROR";

/// Which feed the updater follows.
///
/// Stable reads the release feed (with the mirror as a fallback route).
/// Nightly reads the nightly feed, GitHub only: `main` built every night,
/// versioned `X.Y.Z-nightly.N` so a promoted `X.Y.Z` still reads as newer.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Nightly,
}

impl UpdateChannel {
    /// The channel a build belongs to when the user has not picked one: a
    /// nightly build keeps following nightlies, anything else stays stable.
    pub fn for_version(version: &str) -> Self {
        if Version::parse(version).is_some_and(Version::is_nightly) {
            Self::Nightly
        } else {
            Self::Stable
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
        }
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq)]
pub struct ReleaseNotes {
    pub tag_name: String,
    pub name: Option<String>,
    #[serde(default)]
    pub body: String,
    pub published_at: Option<String>,
}

/// Fetches the latest public GitHub release and its Markdown notes.
///
/// This is separate from update eligibility: an up-to-date install should
/// still be able to read the notes for the version it is already running.
pub fn fetch_latest_release_notes() -> Result<ReleaseNotes> {
    let body = Http::new().fetch_text(LATEST_RELEASE_URL)?;
    parse_release_notes(&body)
}

fn parse_release_notes(body: &str) -> Result<ReleaseNotes> {
    if body.len() > MAX_RELEASE_METADATA_BYTES {
        return Err(UpdateError::Feed(
            "latest release metadata is unexpectedly large".to_owned(),
        ));
    }
    let release: ReleaseNotes =
        serde_json::from_str(body).map_err(|error| UpdateError::Feed(error.to_string()))?;
    if release.tag_name.trim().is_empty() || release.body.trim().is_empty() {
        return Err(UpdateError::Feed(
            "latest release has no version or release notes".to_owned(),
        ));
    }
    Ok(release)
}

#[derive(Clone, Debug)]
pub struct UpdaterConfig {
    /// The stable channel's feed.
    pub feed_url: String,
    /// The nightly channel's feed. Never mirrored.
    pub nightly_feed_url: String,
    /// Host every archive URL in the feed must name; [`RELEASES_HOST`] outside
    /// tests.
    pub releases_host: String,
    /// Second route to the feed and archives when GitHub is unreachable.
    pub mirror: Option<Mirror>,
    pub current_version: Version,
    /// The `.app` that will be replaced.
    pub bundle: PathBuf,
    /// Scratch space for downloads and staged bundles.
    pub cache_dir: PathBuf,
    /// Signature of the running build, which every download is pinned to.
    pub installed_signature: SignatureInfo,
}

impl UpdaterConfig {
    #[cfg(windows)]
    pub fn for_running_app(current_version: &str) -> Result<Self> {
        windows::running_config(current_version)
    }

    /// Builds the configuration for the running app, or explains why this
    /// build cannot update itself.
    ///
    /// `current_version` comes from the caller rather than the bundle's
    /// Info.plist so the app and the updater agree on one source of truth —
    /// `CARGO_PKG_VERSION`, which is also what cargo-packager stamps into the
    /// plist at package time.
    #[cfg(not(windows))]
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

        let home = diri_platform::home_dir()
            .map(|p| p.into_os_string())
            .map(PathBuf::from)
            .ok_or_else(|| UpdateError::NotUpdatable("HOME is unset".to_owned()))?;
        let feed_override = std::env::var(FEED_URL_ENV).ok();
        let mirror = match std::env::var(MIRROR_ENV) {
            _ if feed_override.is_some() => None,
            Ok(value) if value == "off" => None,
            Ok(host) => Some(Mirror::new(&host).ok_or_else(|| {
                UpdateError::NotUpdatable(format!("{MIRROR_ENV} is not a host: {host:?}"))
            })?),
            Err(_) => Some(Mirror::default()),
        };
        Ok(Self {
            // An explicit override is a staging feed and wins on both channels.
            nightly_feed_url: feed_override
                .clone()
                .unwrap_or_else(|| NIGHTLY_FEED_URL.to_owned()),
            feed_url: feed_override.unwrap_or_else(|| DEFAULT_FEED_URL.to_owned()),
            releases_host: RELEASES_HOST.to_owned(),
            mirror,
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
    hedge_after: Duration,
    /// How the last network step was served, until telemetry takes it.
    last_source: Mutex<Option<SourceReport>>,
    /// The last feed came from the mirror, so GitHub was just unreachable:
    /// try the mirror first for the archive too.
    prefer_mirror: AtomicBool,
    /// Following [`UpdateChannel::Nightly`]; switchable while running.
    nightly: AtomicBool,
}

impl Updater {
    pub fn new(config: UpdaterConfig) -> Self {
        Self::with_http(config, Http::new(), mirror::HEDGE_AFTER)
    }

    fn with_http(config: UpdaterConfig, http: Http, hedge_after: Duration) -> Self {
        Self {
            config,
            http,
            hedge_after,
            last_source: Mutex::new(None),
            prefer_mirror: AtomicBool::new(false),
            nightly: AtomicBool::new(false),
        }
    }

    pub fn channel(&self) -> UpdateChannel {
        if self.nightly.load(Ordering::Relaxed) {
            UpdateChannel::Nightly
        } else {
            UpdateChannel::Stable
        }
    }

    /// Switches the feed later checks read. Anything already found or staged
    /// is the caller's to drop.
    pub fn set_channel(&self, channel: UpdateChannel) {
        self.nightly
            .store(channel == UpdateChannel::Nightly, Ordering::Relaxed);
        self.prefer_mirror.store(false, Ordering::Relaxed);
    }

    /// The feed URL and fallback mirror for the current channel.
    fn feed_route(&self) -> (&str, Option<&Mirror>) {
        match self.channel() {
            UpdateChannel::Stable => (&self.config.feed_url, self.config.mirror.as_ref()),
            UpdateChannel::Nightly => (&self.config.nightly_feed_url, None),
        }
    }

    /// The mirror archive downloads may fall back to on this channel.
    fn archive_mirror(&self) -> Option<&Mirror> {
        self.feed_route().1
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    /// Which route served the most recent feed fetch or download, and why
    /// the other was skipped. Taken, so a step that never reached the network
    /// is not credited with an earlier step's route.
    pub fn take_source(&self) -> Option<SourceReport> {
        self.last_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn note_source(&self, report: SourceReport) {
        *self
            .last_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
    }

    /// Reads the feed from GitHub, or from the mirror when GitHub's route
    /// fails or has not answered within the hedge delay.
    fn fetch_feed(&self) -> Result<Feed> {
        let channel = self.channel();
        let (feed_url, feed_mirror) = self.feed_route();
        let github = {
            let http = self.http.clone();
            let url = feed_url.to_owned();
            // With a mirror to fall back to, the mirror is the retry: a
            // second try at a blocked GitHub would only delay it.
            let retry = feed_mirror.is_none();
            move || {
                if retry {
                    http.fetch_text(&url)
                } else {
                    http.fetch_text_once(&url)
                }
            }
        };
        let mirror = feed_mirror.map(|mirror| {
            let http = self.http.clone();
            let url = mirror.feed_url();
            move || http.fetch_text(&url)
        });
        let (body, report) = mirror::race(github, mirror, self.hedge_after);
        self.note_source(report);
        let body = body?;
        self.prefer_mirror
            .store(report.source == Source::Mirror, Ordering::Relaxed);
        Feed::parse(&body)
            .map(|feed| feed.for_channel(channel))
            .map_err(|error| UpdateError::Feed(error.to_string()))
    }

    /// Fetches the feed and returns the release worth offering, if any.
    pub fn check(&self, skipped: Option<&str>) -> Result<Option<Release>> {
        let feed = self.fetch_feed()?;
        Ok(feed
            .newest_eligible(Eligibility {
                current: self.config.current_version,
                system: bundle::system_version(),
                skipped,
            })
            .cloned())
    }

    /// Every release the feed offers that this machine could install, newest
    /// first, including the running version and older ones. Powers the
    /// explicit version picker; [`Updater::check`] stays newer-only.
    pub fn available_releases(&self) -> Result<Vec<Release>> {
        let feed = self.fetch_feed()?;
        Ok(feed
            .installable(bundle::system_version())
            .into_iter()
            .cloned()
            .collect())
    }

    /// The feed's installable release with exactly `version`, if any. The
    /// version is re-read from the feed rather than trusted from the caller so
    /// the download URL and checksum always come from the pinned host.
    pub fn release(&self, version: &str) -> Result<Option<Release>> {
        let Some(wanted) = Version::parse(version) else {
            return Ok(None);
        };
        let feed = self.fetch_feed()?;
        Ok(feed.find(wanted, bundle::system_version()).cloned())
    }

    /// Downloads the release archive, verifying size and checksum.
    ///
    /// Checks the install location *first*: discovering that `/Applications`
    /// is read-only after pulling 50 MB wastes the user's bandwidth and their
    /// attention.
    ///
    /// GitHub goes first unless the feed just came from the mirror; a route
    /// failure on one tries the other. Whichever serves the bytes, they must
    /// match the feed's size and SHA-256 — a mismatch is final, not a reason
    /// to shop for other bytes.
    pub fn download(&self, release: &Release, mut on_progress: impl FnMut(f32)) -> Result<PathBuf> {
        bundle::ensure_writable(&self.config.bundle)?;
        net::validated_download_url(&release.url, &self.config.releases_host)?;
        let expected = release.sha256.as_deref().ok_or_else(|| {
            UpdateError::Feed("release is missing its SHA-256 checksum".to_owned())
        })?;

        let mut routes = vec![(Source::GitHub, release.url.clone())];
        if let Some(mirror) = self.archive_mirror()
            && let Some(url) = mirror.archive_url(&release.url, &self.config.releases_host)
        {
            net::validated_download_url(&url, &mirror.host)?;
            if self.prefer_mirror.load(Ordering::Relaxed) {
                routes.insert(0, (Source::Mirror, url));
            } else {
                routes.push((Source::Mirror, url));
            }
        }

        let directory = self.release_dir(release);
        std::fs::create_dir_all(&directory)?;
        let archive = directory.join("diri.zip");
        let mut report = SourceReport::github();
        let mut github_error = None;
        let mut served = None;
        for (index, (source, url)) in routes.iter().enumerate() {
            match self
                .http
                .download(url, &archive, release.size, &mut on_progress)
            {
                Ok(()) => {
                    served = Some(*source);
                    break;
                }
                Err(error) => {
                    report.note_failure(*source, &error);
                    let last = index + 1 == routes.len();
                    if last || !error.is_route_failure() {
                        // Surface GitHub's error when it had one: it is the
                        // canonical host, and a route failure on both says
                        // GitHub is unreachable.
                        let (source, error) = match github_error {
                            Some(github) if error.is_route_failure() => (Source::GitHub, github),
                            _ => (*source, error),
                        };
                        report.source = source;
                        self.note_source(report);
                        return Err(error);
                    }
                    if *source == Source::GitHub {
                        github_error = Some(error);
                    }
                }
            }
        }
        let source = served.unwrap_or(Source::GitHub);
        report.source = source;
        if source == Source::Mirror && report.github_error.is_none() {
            report.github_error = Some("skipped");
        }
        self.note_source(report);
        if let Err(error) = net::verify_sha256(&archive, expected) {
            let _ = std::fs::remove_file(&archive);
            return Err(error);
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
    pub fn install(&self, staged: &StagedUpdate, relaunch: bool) -> Result<()> {
        bundle::ensure_writable(&self.config.bundle)?;
        install::launch_installer(
            &staged.app,
            &self.config.bundle,
            &staged.directory,
            relaunch,
        )
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
#[cfg(not(windows))]
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
    staged_version_matches(&String::from_utf8_lossy(&output.stdout), release)
}

/// Holds the staged bundle to exactly the promised version, nightly stamp
/// included: one nightly's feed row must not install another night's build.
#[cfg(any(not(windows), test))]
fn staged_version_matches(found: &str, release: &Release) -> Result<()> {
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

#[cfg(windows)]
fn verify_staged_version(app: &Path, release: &Release) -> Result<()> {
    windows::verify_version(app, release)
}

// Real curl against local HTTPS servers via the system `openssl`.
#[cfg(all(test, target_os = "macos"))]
mod fallback_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> UpdaterConfig {
        UpdaterConfig {
            feed_url: DEFAULT_FEED_URL.to_owned(),
            nightly_feed_url: NIGHTLY_FEED_URL.to_owned(),
            releases_host: RELEASES_HOST.to_owned(),
            mirror: Some(Mirror::default()),
            current_version: Version::new(0, 4, 2),
            bundle: PathBuf::from("/Applications/diri.app"),
            cache_dir: PathBuf::from("/tmp/diri-updates"),
            installed_signature: SignatureInfo::default(),
        }
    }

    #[test]
    fn latest_release_metadata_keeps_the_canonical_markdown_body() {
        let release = parse_release_notes(
            r###"{
                "tag_name": "v0.6.0",
                "name": "diri 0.6.0",
                "body": "## Highlights\n\n- Faster sessions",
                "published_at": "2026-09-05T12:17:55Z"
            }"###,
        )
        .expect("release metadata");

        assert_eq!(release.tag_name, "v0.6.0");
        assert_eq!(release.body, "## Highlights\n\n- Faster sessions");
        assert_eq!(
            release.published_at.as_deref(),
            Some("2026-09-05T12:17:55Z")
        );
    }

    #[test]
    fn latest_release_metadata_requires_notes() {
        let error = parse_release_notes(r#"{"tag_name":"v0.6.0","body":""}"#)
            .expect_err("empty notes must not render as a successful release");
        assert!(matches!(error, UpdateError::Feed(_)));
    }

    #[test]
    #[ignore = "requires network access to GitHub's releases API"]
    fn the_latest_release_notes_are_reachable() {
        let release = fetch_latest_release_notes().expect("latest release notes");
        assert!(release.tag_name.starts_with('v'));
        assert!(!release.body.trim().is_empty());
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

    /// A release asset that is not there must read as "missing", not as
    /// "couldn't reach the host" — GitHub serves it over HTTP/2, where curl
    /// reports the 404 under exit 56.
    #[test]
    #[ignore = "requires network access to the releases host"]
    fn a_missing_release_asset_is_reported_as_not_found() {
        let error = Http::new()
            .fetch_text("https://github.com/cristicretu/diri/releases/latest/download/missing.json")
            .expect_err("no such asset");
        assert!(
            matches!(
                error,
                UpdateError::Network {
                    failure: NetworkFailure::NotFound,
                    ..
                }
            ),
            "{error}"
        );
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
        for executable in ["dirijord-rs", "diri-holder"] {
            let path = staged.app.join("Contents/Resources/bin").join(executable);
            let _metadata = std::fs::metadata(&path).unwrap_or_else(|error| {
                panic!("published bundle is missing {}: {error}", path.display())
            });
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_ne!(
                    _metadata.permissions().mode() & 0o111,
                    0,
                    "published helper is not executable: {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn the_default_feed_lives_on_the_pinned_host() {
        assert!(DEFAULT_FEED_URL.starts_with(&format!("https://{RELEASES_HOST}/")));
        assert!(NIGHTLY_FEED_URL.starts_with(&format!("https://{RELEASES_HOST}/")));
    }

    #[test]
    fn the_nightly_channel_reads_its_own_feed_without_the_mirror() {
        let updater = Updater::new(config());
        assert_eq!(updater.channel(), UpdateChannel::Stable);
        let (url, mirror) = updater.feed_route();
        assert_eq!(url, DEFAULT_FEED_URL);
        assert!(mirror.is_some());

        updater.set_channel(UpdateChannel::Nightly);
        assert_eq!(updater.channel(), UpdateChannel::Nightly);
        let (url, mirror) = updater.feed_route();
        assert_eq!(url, NIGHTLY_FEED_URL);
        assert!(mirror.is_none());
        assert!(updater.archive_mirror().is_none());

        updater.set_channel(UpdateChannel::Stable);
        assert_eq!(updater.feed_route().0, DEFAULT_FEED_URL);
    }

    #[test]
    fn a_build_defaults_to_the_channel_its_version_came_from() {
        assert_eq!(UpdateChannel::for_version("0.9.3"), UpdateChannel::Stable);
        assert_eq!(
            UpdateChannel::for_version("0.9.4-nightly.202610070417"),
            UpdateChannel::Nightly
        );
        assert_eq!(
            UpdateChannel::for_version("0.9.4-beta.1"),
            UpdateChannel::Stable
        );
    }

    #[test]
    fn a_staged_nightly_must_match_the_promised_stamp() {
        let release = Release {
            version: "0.9.4-nightly.202610070417".to_owned(),
            ..Release::default()
        };
        staged_version_matches("0.9.4-nightly.202610070417\n", &release)
            .expect("the promised nightly");
        for wrong in ["0.9.4-nightly.202610060417", "0.9.4", "0.9.3"] {
            let error = staged_version_matches(wrong, &release).expect_err(wrong);
            assert!(matches!(error, UpdateError::Integrity(_)), "{wrong}");
        }
        let stable = Release {
            version: "0.9.4".to_owned(),
            ..Release::default()
        };
        assert!(staged_version_matches("0.9.4-nightly.202610070417", &stable).is_err());
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
