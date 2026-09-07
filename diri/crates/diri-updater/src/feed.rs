//! The update feed diri fetches from the releases host.
//!
//! JSON rather than a Sparkle appcast: nothing in diri parses XML, and the
//! Swift app's appcast carries Sparkle-specific EdDSA signatures that this
//! updater does not use (see `crate::codesign` for the trust model).
//!
//! ```json
//! {
//!   "feed_version": 1,
//!   "releases": [
//!     {
//!       "version": "0.2.0",
//!       "url": "https://github.com/cristicretu/diri/releases/download/v0.2.0/diri-0.2.0-universal.zip",
//!       "size": 48210944,
//!       "sha256": "5f2e…",
//!       "minimum_system_version": "15.0",
//!       "published": "2026-07-26",
//!       "notes_url": "https://github.com/cristicretu/diri/releases/download/v0.2.0/diri-0.2.0.html"
//!     }
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::net::MAX_ARCHIVE_BYTES;
use crate::version::Version;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Release {
    pub version: String,
    /// Zip of the notarized, stapled `diri.app` — not the DMG. See
    /// `crate::install` for why updates ship as an archive.
    pub url: String,
    /// Expected byte count, used for the download progress bar and as a cheap
    /// truncation check. Zero means "unknown".
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub minimum_system_version: Option<String>,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
}

impl Release {
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }

    /// `None` when the release omits a floor, which reads as "runs anywhere".
    fn minimum_system(&self) -> Option<Version> {
        self.minimum_system_version
            .as_deref()
            .and_then(Version::parse)
    }

    fn has_bounded_integrity_metadata(&self) -> bool {
        self.size > 0
            && self.size <= MAX_ARCHIVE_BYTES
            && self.sha256.as_deref().is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Feed {
    #[serde(default)]
    pub feed_version: u32,
    #[serde(default)]
    pub releases: Vec<Release>,
}

/// What the installed build knows about itself when judging the feed.
#[derive(Clone, Copy, Debug)]
pub struct Eligibility<'a> {
    pub current: Version,
    /// macOS version of this machine, from `sw_vers -productVersion`.
    pub system: Version,
    /// Version string the user chose to skip, if any.
    pub skipped: Option<&'a str>,
}

impl Feed {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The highest release worth offering, or `None` when the running build is
    /// already current.
    ///
    /// Entries that fail to parse are skipped rather than fatal: one bad hand-
    /// edited row in the feed must not strand every install on its old build.
    /// Only strictly-newer versions qualify, which is also what blocks a
    /// tampered feed from walking users back onto an older signed build.
    pub fn newest_eligible(&self, against: Eligibility<'_>) -> Option<&Release> {
        self.releases
            .iter()
            .filter(|release| {
                let Some(version) = release.parsed_version() else {
                    return false;
                };
                if !version.is_newer_than(against.current) {
                    return false;
                }
                if against.skipped.is_some_and(|skipped| {
                    Version::parse(skipped).is_some_and(|skipped| skipped == version)
                }) {
                    return false;
                }
                if release
                    .minimum_system()
                    .is_some_and(|minimum| minimum > against.system)
                {
                    return false;
                }
                !release.url.is_empty() && release.has_bounded_integrity_metadata()
            })
            .max_by_key(|release| release.parsed_version().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: Version = Version::new(0, 4, 2);
    const SYSTEM: Version = Version::new(15, 5, 0);

    fn eligibility() -> Eligibility<'static> {
        Eligibility {
            current: CURRENT,
            system: SYSTEM,
            skipped: None,
        }
    }

    fn release(version: &str) -> Release {
        Release {
            version: version.to_owned(),
            url: format!("https://example.test/diri-{version}.zip"),
            size: 1,
            sha256: Some("0".repeat(64)),
            ..Release::default()
        }
    }

    #[test]
    fn parses_a_minimal_feed() {
        let feed = Feed::parse(
            r#"{"feed_version":1,"releases":[{"version":"0.5.0","url":"https://example.test/a.zip"}]}"#,
        )
        .expect("feed parses");
        assert_eq!(feed.feed_version, 1);
        assert_eq!(feed.releases[0].version, "0.5.0");
        assert_eq!(feed.releases[0].size, 0);
        assert!(feed.releases[0].sha256.is_none());
    }

    /// Byte-for-byte shape of what `diri/scripts/release.sh` writes. If the
    /// script's JSON and this parser ever drift, every install silently stops
    /// updating — so the document itself is the fixture.
    #[test]
    fn parses_the_feed_the_release_script_generates() {
        let feed = Feed::parse(
            r#"{
  "feed_version": 1,
  "releases": [
    {
      "version": "0.2.0",
      "url": "https://github.com/cristicretu/diri/releases/download/v0.2.0/diri-0.2.0-universal.zip",
      "size": 48210944,
      "sha256": "3f786850e387550fdab836ed7e6dc881de23001b4f6d1f4e1a1e0c2a7b3c4d5e",
      "minimum_system_version": "15.0",
      "published": "2026-07-26",
      "notes_url": "https://github.com/cristicretu/diri/releases/download/v0.2.0/diri-0.2.0.html"
    },
    {
      "version": "0.1.0",
      "url": "https://github.com/cristicretu/diri/releases/download/v0.2.0/diri-0.1.0-universal.zip",
      "size": 48102400,
      "sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
      "minimum_system_version": "15.0",
      "published": "2026-07-20"
    }
  ]
}
"#,
        )
        .expect("the release script's feed parses");

        let offered = feed
            .newest_eligible(Eligibility {
                current: Version::new(0, 1, 0),
                system: SYSTEM,
                skipped: None,
            })
            .expect("0.2.0 is newer than 0.1.0");
        assert_eq!(offered.version, "0.2.0");
        assert_eq!(offered.size, 48_210_944);
        assert!(offered.sha256.is_some());
        assert!(offered.notes_url.is_some());
        assert!(offered.url.ends_with("diri-0.2.0-universal.zip"));

        // The same feed offers nothing once 0.2.0 is installed.
        assert!(
            feed.newest_eligible(Eligibility {
                current: Version::new(0, 2, 0),
                system: SYSTEM,
                skipped: None,
            })
            .is_none()
        );
    }

    #[test]
    fn picks_the_highest_newer_release() {
        let feed = Feed {
            feed_version: 1,
            releases: vec![release("0.4.3"), release("0.6.0"), release("0.5.1")],
        };
        assert_eq!(
            feed.newest_eligible(eligibility())
                .map(|r| r.version.as_str()),
            Some("0.6.0")
        );
    }

    #[test]
    fn never_offers_the_current_or_an_older_build() {
        let feed = Feed {
            feed_version: 1,
            releases: vec![release("0.4.2"), release("0.4.1"), release("0.1.0")],
        };
        assert!(feed.newest_eligible(eligibility()).is_none());
    }

    #[test]
    fn honors_the_skipped_version_but_still_offers_later_ones() {
        let feed = Feed {
            feed_version: 1,
            releases: vec![release("0.5.0"), release("0.6.0")],
        };
        let skipping = Eligibility {
            skipped: Some("0.6.0"),
            ..eligibility()
        };
        assert_eq!(
            feed.newest_eligible(skipping).map(|r| r.version.as_str()),
            Some("0.5.0")
        );
    }

    #[test]
    fn skips_releases_that_need_a_newer_macos() {
        let mut too_new = release("0.7.0");
        too_new.minimum_system_version = Some("26.0".to_owned());
        let feed = Feed {
            feed_version: 1,
            releases: vec![too_new, release("0.5.0")],
        };
        assert_eq!(
            feed.newest_eligible(eligibility())
                .map(|r| r.version.as_str()),
            Some("0.5.0")
        );
    }

    #[test]
    fn a_malformed_row_does_not_hide_the_good_ones() {
        let feed = Feed {
            feed_version: 1,
            releases: vec![
                release("nightly"),
                Release {
                    version: "0.9.0".to_owned(),
                    url: String::new(),
                    ..Release::default()
                },
                release("0.5.0"),
            ],
        };
        assert_eq!(
            feed.newest_eligible(eligibility())
                .map(|r| r.version.as_str()),
            Some("0.5.0")
        );
    }

    #[test]
    fn skips_releases_without_bounded_size_and_checksum_metadata() {
        let mut missing_size = release("0.7.0");
        missing_size.size = 0;
        let mut huge = release("0.8.0");
        huge.size = MAX_ARCHIVE_BYTES + 1;
        let mut bad_digest = release("0.9.0");
        bad_digest.sha256 = Some("not-a-digest".into());
        let feed = Feed {
            feed_version: 1,
            releases: vec![missing_size, huge, bad_digest],
        };
        assert!(feed.newest_eligible(eligibility()).is_none());
    }
}
