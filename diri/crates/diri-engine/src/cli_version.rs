//! Agent CLI versions, for features that only some releases support.
//!
//! The first user is the Engine-served MCP endpoint: Claude Code and Codex
//! are pointed at it only from a release verified to speak it, and keep the
//! stdio `dirijor-mcp` otherwise. An older CLI handed a config it cannot read
//! may refuse to start or silently lose every `dirijor` tool, so an unknown
//! version always means "not supported".
//!
//! `<cli> --version` is never run on the spawn path. [`CliVersions::get`]
//! answers from a cache keyed by the resolved executable (symlinks followed)
//! and its size and modification time, and on a miss starts one background
//! probe and answers "unknown". A CLI that updates itself changes its file or
//! its symlink target, which misses the cache and probes again. Nothing is
//! polled; the Engine warms the cache once at startup.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How long a `--version` probe may take before it is killed and the CLI
/// counts as unknown.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on what a probe reads; a version line is tiny.
const PROBE_OUTPUT_LIMIT: u64 = 4096;

/// A `major.minor.patch` release. A pre-release (`0.160.0-alpha.2`) sorts
/// below its release, so a minimum is never met by an unfinished build of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `true` for a final release, `false` for a pre-release.
    pub release: bool,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            release: true,
        }
    }

    /// The first `X.Y.Z` in a `--version` line: `2.1.289 (Claude Code)`,
    /// `codex-cli 0.160.0`, `v1.2.3`.
    pub fn parse(output: &str) -> Option<Self> {
        output.split_whitespace().find_map(|word| {
            let word = word.trim_start_matches('v');
            let (core, suffix) = match word.find(['-', '+']) {
                Some(index) => (&word[..index], &word[index..]),
                None => (word, ""),
            };
            let mut parts = core.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            if parts.next().is_some() {
                return None;
            }
            Some(Self {
                major,
                minor,
                patch,
                // Build metadata (`+abc`) is still a release.
                release: !suffix.starts_with('-'),
            })
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.release {
            f.write_str("-pre")?;
        }
        Ok(())
    }
}

/// Identifies one build of an executable on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
enum Entry {
    Probing(Stamp),
    Known(Stamp, Option<Version>),
}

#[derive(Clone, Default)]
pub struct CliVersions {
    cache: Arc<Mutex<HashMap<PathBuf, Entry>>>,
}

impl CliVersions {
    /// The cached version of `executable`, or `None` while unknown. A miss
    /// starts a background probe and never blocks the caller.
    pub fn get(&self, executable: &Path) -> Option<Version> {
        let (path, stamp) = identify(executable)?;
        let mut cache = self.cache.lock().ok()?;
        match cache.get(&path) {
            Some(Entry::Known(known, version)) if *known == stamp => return *version,
            Some(Entry::Probing(probing)) if *probing == stamp => return None,
            _ => {}
        }
        cache.insert(path.clone(), Entry::Probing(stamp.clone()));
        drop(cache);
        let versions = self.clone();
        let spawned = std::thread::Builder::new()
            .name("diri-cli-version".into())
            .spawn(move || versions.record(path, stamp));
        if spawned.is_err() {
            // Leave it unknown; the next spawn tries again.
            if let Ok(mut cache) = self.cache.lock() {
                cache.retain(|_, entry| !matches!(entry, Entry::Probing(_)));
            }
        }
        None
    }

    /// Probes `executable` now, on this thread, and caches the answer. For
    /// the startup warm-up and tests; never call it on the spawn path.
    pub fn probe_now(&self, executable: &Path) -> Option<Version> {
        let (path, stamp) = identify(executable)?;
        self.record(path, stamp)
    }

    fn record(&self, path: PathBuf, stamp: Stamp) -> Option<Version> {
        let version = probe(&path);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(path, Entry::Known(stamp, version));
        }
        version
    }
}

fn identify(executable: &Path) -> Option<(PathBuf, Stamp)> {
    let path = std::fs::canonicalize(executable).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    metadata.is_file().then(|| {
        (
            path,
            Stamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            },
        )
    })
}

/// Runs `<executable> --version` with no stdin and a deadline.
fn probe(executable: &Path) -> Option<Version> {
    let spawn = || {
        Command::new(executable)
            .arg("--version")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    // A file still open for writing somewhere (an updater mid-write, or any
    // process forked while it was) cannot be executed yet. The answer is
    // cached for this stamp, so wait that out briefly rather than recording
    // the CLI as unknown until it next changes.
    let mut attempts = 0;
    let mut child = loop {
        match spawn() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            spawned => break spawned.ok()?,
        }
    };
    let mut stdout = child.stdout.take()?;
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = String::new();
        let _ = (&mut stdout)
            .take(PROBE_OUTPUT_LIMIT)
            .read_to_string(&mut output);
        let _ = done.send(output);
    });
    let output = finished.recv_timeout(PROBE_TIMEOUT).ok();
    if output.is_none() {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    Version::parse(output?.lines().next()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_cli_format() {
        assert_eq!(
            Version::parse("2.1.289 (Claude Code)"),
            Some(Version::new(2, 1, 289))
        );
        assert_eq!(
            Version::parse("codex-cli 0.160.0"),
            Some(Version::new(0, 160, 0))
        );
        assert_eq!(Version::parse("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(
            Version::parse("codex-cli 0.161.0+build.7"),
            Some(Version::new(0, 161, 0))
        );
        let pre = Version::parse("codex-cli 0.160.0-alpha.2").unwrap();
        assert!(!pre.release);
        assert!(pre < Version::new(0, 160, 0));
        assert!(pre > Version::new(0, 159, 9));
        for unknown in [
            "",
            "codex-cli",
            "Claude Code",
            "2.1",
            "1.2.3.4",
            "version x.y.z",
        ] {
            assert_eq!(Version::parse(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn versions_order_numerically() {
        assert!(Version::new(2, 1, 289) > Version::new(2, 1, 99));
        assert!(Version::new(0, 160, 0) > Version::new(0, 99, 99));
        assert!(Version::new(3, 0, 0) > Version::new(2, 99, 999));
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_miss_answers_unknown_without_blocking_then_the_probe_fills_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cli = script(
            dir.path(),
            "claude",
            "sleep 0.3; echo '2.1.290 (Claude Code)'",
        );
        let versions = CliVersions::default();
        let started = std::time::Instant::now();
        assert_eq!(versions.get(&cli), None);
        assert!(started.elapsed() < Duration::from_millis(200));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while versions.get(&cli).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(versions.get(&cli), Some(Version::new(2, 1, 290)));
    }

    #[cfg(unix)]
    #[test]
    fn a_self_update_is_noticed_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let old = script(dir.path(), "claude-old", "echo '2.1.100 (Claude Code)'");
        let new = script(dir.path(), "claude-new", "echo '2.1.300 (Claude Code)'");
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        let versions = CliVersions::default();
        assert_eq!(versions.probe_now(&link), Some(Version::new(2, 1, 100)));
        assert_eq!(versions.get(&link), Some(Version::new(2, 1, 100)));

        // The updater repoints the symlink: a different file, probed afresh.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        assert_eq!(versions.get(&link), None, "the old answer is not reused");
        assert_eq!(versions.probe_now(&link), Some(Version::new(2, 1, 300)));

        // Rewritten in place: size and mtime change.
        std::fs::write(&new, "#!/bin/sh\necho 'codex-cli 0.170.0 and more'\n").unwrap();
        assert_eq!(versions.get(&link), None);
        assert_eq!(versions.probe_now(&link), Some(Version::new(0, 170, 0)));
    }

    #[cfg(unix)]
    #[test]
    fn failing_silent_or_missing_clis_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let versions = CliVersions::default();
        let failing = script(dir.path(), "a", "echo '9.9.9'; exit 3");
        assert_eq!(versions.probe_now(&failing), None);
        let silent = script(dir.path(), "b", "true");
        assert_eq!(versions.probe_now(&silent), None);
        assert_eq!(versions.probe_now(&dir.path().join("missing")), None);
        assert_eq!(versions.get(&dir.path().join("missing")), None);
    }
}
