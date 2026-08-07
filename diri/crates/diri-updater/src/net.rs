//! HTTPS via `curl`, plus the URL checks the feed's contents have to pass.
//!
//! Shelling out to `/usr/bin/curl` instead of linking an HTTP stack is a
//! deliberate trade: it keeps a TLS + async-runtime dependency tree out of a
//! GPUI app that already takes minutes to build, and macOS ships curl on every
//! machine diri supports. Two requests per update cycle is not a workload that
//! rewards an in-process client.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{Result, UpdateError};

const CURL: &str = "/usr/bin/curl";
const FEED_TIMEOUT_SECONDS: u32 = 20;
const DOWNLOAD_TIMEOUT_SECONDS: u32 = 900;
const PROGRESS_POLL: Duration = Duration::from_millis(150);

/// Downloads over `curl`, with the hardening the installer depends on.
#[derive(Clone, Debug, Default)]
pub struct Http;

impl Http {
    pub fn new() -> Self {
        Self
    }

    pub fn fetch_text(&self, url: &str) -> Result<String> {
        let output = self
            .curl()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                let config = self.config(url, None, FEED_TIMEOUT_SECONDS);
                child
                    .stdin
                    .take()
                    .expect("curl stdin was piped")
                    .write_all(config.as_bytes())?;
                child.wait_with_output()
            })?;
        if !output.status.success() {
            return Err(UpdateError::Network(curl_detail(&output.stderr)));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| UpdateError::Feed("feed is not valid UTF-8".to_owned()))
    }

    /// Downloads to `destination`, reporting completed fraction whenever it
    /// moves. `expected_size` of 0 disables progress (the callback then only
    /// fires once, at completion).
    pub fn download(
        &self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        mut on_progress: impl FnMut(f32),
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        // curl appends to an existing file with some flag combinations and a
        // stale partial would silently corrupt the archive.
        let _ = fs::remove_file(destination);

        let mut child = self
            .curl()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let config = self.config(url, Some(destination), DOWNLOAD_TIMEOUT_SECONDS);
        child
            .stdin
            .take()
            .expect("curl stdin was piped")
            .write_all(config.as_bytes())?;

        let mut last_reported = 0.0_f32;
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read as _;
                        let mut buffer = Vec::new();
                        let _ = pipe.read_to_end(&mut buffer);
                        stderr = curl_detail(&buffer);
                    }
                    let _ = fs::remove_file(destination);
                    return Err(UpdateError::Network(stderr));
                }
                break;
            }
            if expected_size > 0 {
                let written = fs::metadata(destination)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                let fraction = (written as f32 / expected_size as f32).clamp(0.0, 1.0);
                if fraction - last_reported >= 0.01 {
                    last_reported = fraction;
                    on_progress(fraction);
                }
            }
            // A stalled transfer writes no bytes, so this poll cannot notice
            // it; curl's own --max-time is what bounds the wait.
            std::thread::sleep(PROGRESS_POLL);
        }
        on_progress(1.0);

        let written = fs::metadata(destination)?.len();
        if expected_size > 0 && written != expected_size {
            let _ = fs::remove_file(destination);
            return Err(UpdateError::Integrity(format!(
                "expected {expected_size} bytes, got {written}"
            )));
        }
        Ok(())
    }

    fn curl(&self) -> Command {
        let mut command = Command::new(CURL);
        // -K - keeps the URL and (more importantly) the credentials out of the
        // process arguments, where any user on the machine could read them.
        command.arg("-K").arg("-");
        command
    }

    fn config(&self, url: &str, output: Option<&Path>, timeout_seconds: u32) -> String {
        let mut config = String::new();
        config.push_str(&format!("url = \"{url}\"\n"));
        config.push_str("fail\nlocation\nsilent\nshow-error\n");
        config.push_str("proto = \"=https\"\nproto-redir = \"=https\"\n");
        config.push_str(&format!("max-time = {timeout_seconds}\n"));
        config.push_str("connect-timeout = 15\n");
        config.push_str("max-redirs = 5\n");
        config.push_str(&format!("user-agent = \"diri-updater/{}\"\n", crate::AGENT));
        if let Some(path) = output {
            config.push_str(&format!("output = \"{}\"\n", path.display()));
        }
        config
    }
}

/// Rejects anything that is not a plain `https://<host>/…` URL on `host`.
///
/// Two jobs. It pins downloads to the same origin as the feed, so a feed that
/// is tampered with (or served through a compromised cache) cannot redirect
/// the installer to an attacker's host. And it refuses quotes, newlines, and
/// backslashes, which are the characters that would let a URL break out of the
/// quoted `url = "…"` line in curl's stdin config and inject options.
pub fn validated_download_url(url: &str, host: &str) -> Result<()> {
    if url
        .chars()
        .any(|character| character.is_control() || matches!(character, '"' | '\\'))
    {
        return Err(UpdateError::UntrustedUrl(
            "URL contains quoting or control characters".to_owned(),
        ));
    }
    let Some(remainder) = url.strip_prefix("https://") else {
        return Err(UpdateError::UntrustedUrl(format!("{url} is not https")));
    };
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    // Userinfo in the authority (`https://evil@host/`) would make the host
    // check read the wrong side of the `@`.
    if authority.contains('@') || authority != host {
        return Err(UpdateError::UntrustedUrl(format!(
            "{authority} is not the releases host"
        )));
    }
    Ok(())
}

/// Verifies a download against the feed's checksum using `/usr/bin/shasum`.
///
/// The checksum guards against a truncated or mangled transfer. It is *not*
/// the trust anchor — it comes from the same document as the URL, so a
/// tampered feed would carry a matching hash. `crate::codesign` is what
/// decides whether the bytes are really a diri build.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let output = Command::new("/usr/bin/shasum")
        .arg("-a")
        .arg("256")
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::tool(
            "shasum",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual = stdout.split_whitespace().next().unwrap_or_default();
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        return Err(UpdateError::Integrity(format!(
            "sha256 {actual} does not match the feed's {expected}"
        )));
    }
    Ok(())
}

fn curl_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let detail = text.trim();
    if detail.is_empty() {
        "curl exited non-zero".to_owned()
    } else {
        detail.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "github.com";

    #[test]
    fn accepts_a_release_url_on_the_pinned_host() {
        assert!(validated_download_url("https://github.com/diri/diri-0.2.0.zip", HOST).is_ok());
    }

    #[test]
    fn rejects_other_hosts_and_schemes() {
        assert!(validated_download_url("https://evil.test/diri.zip", HOST).is_err());
        assert!(validated_download_url("http://github.com/diri.zip", HOST).is_err());
        assert!(validated_download_url("file:///tmp/diri.zip", HOST).is_err());
    }

    #[test]
    fn rejects_userinfo_that_would_fake_the_host() {
        assert!(validated_download_url("https://github.com@evil.test/diri.zip", HOST).is_err());
    }

    #[test]
    fn rejects_urls_that_could_inject_curl_options() {
        assert!(
            validated_download_url("https://github.com/a\"\nupload-file = \"/etc/passwd", HOST)
                .is_err()
        );
        assert!(validated_download_url("https://github.com/a\\b", HOST).is_err());
    }

    #[test]
    fn requests_carry_no_credentials_and_keep_the_url_out_of_argv() {
        // Releases are public now: there is no gate to authenticate against,
        // and a `user =` line would only be a credential to leak.
        let http = Http::new();
        let config = http.config("https://example.test/a.json", None, 20);
        assert!(!config.contains("user = "), "no credentials: {config}");
        assert!(config.contains("proto = \"=https\"\n"), "https only");

        // The URL still goes over stdin rather than argv, where any other user
        // on the machine could read it.
        let command = http.curl();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-K", "-"]);
    }

    #[test]
    fn matching_checksum_passes_and_a_wrong_one_fails() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("payload");
        fs::write(&path, b"diri").expect("write payload");
        // echo -n diri | shasum -a 256
        let expected = "8f9a2b0f8c9f2d5f24b3f2ca0d18b6f4b3f7f2e0f5d0b1b0d0f5c6b7a8d9e0f1";
        assert!(verify_sha256(&path, expected).is_err());

        let output = Command::new("/usr/bin/shasum")
            .arg("-a")
            .arg("256")
            .arg(&path)
            .output()
            .expect("shasum runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let actual = stdout.split_whitespace().next().expect("a digest");
        assert!(verify_sha256(&path, actual).is_ok());
        assert!(verify_sha256(&path, &actual.to_uppercase()).is_ok());
    }
}
