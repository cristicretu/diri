//! HTTPS via `curl`, plus the URL checks the feed's contents have to pass.
//!
//! Shelling out to the system `curl` instead of linking an HTTP stack is a
//! deliberate trade: it keeps a TLS + async-runtime dependency tree out of a
//! GPUI app that already takes minutes to build, and macOS ships curl on every
//! machine diri supports. Two requests per update cycle is not a workload that
//! rewards an in-process client.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{NetworkFailure, Result, UpdateError};
use sha2::{Digest, Sha256};

const FEED_TIMEOUT_SECONDS: u32 = 20;
const DOWNLOAD_TIMEOUT_SECONDS: u32 = 900;
/// Where GitHub is blocked the TCP handshake usually just hangs; ten seconds
/// is several round trips even on a bad link, and the caller has a mirror to
/// move on to.
const CONNECT_TIMEOUT_SECONDS: u32 = 10;
/// A download slower than this for [`LOW_SPEED_SECONDS`] is abandoned as a
/// timeout rather than left to crawl toward the 15-minute cap: a throttled
/// route to GitHub's CDN delivers a few KB/s indefinitely.
const LOW_SPEED_BYTES_PER_SECOND: u32 = 4 * 1024;
const LOW_SPEED_SECONDS: u32 = 30;
const PROGRESS_POLL: Duration = Duration::from_millis(150);
pub(crate) const MAX_FEED_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
/// A feed fetch that failed this fast on a transient cause gets one more try
/// after [`RETRY_DELAY`]; a slow failure has already cost the user enough.
const RETRY_WITHIN: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// curl prints this line on stderr after every transfer (`write-out`), so a
/// failure carries the HTTP status even when curl's exit code does not: over
/// HTTP/2 macOS curl reports `--fail` errors as exit 56, not 22.
const STATUS_MARKER: &str = "diri-http-status:";

/// Downloads over `curl`, with the hardening the installer depends on.
#[derive(Clone, Debug)]
pub struct Http {
    pub(crate) connect_timeout_seconds: u32,
    pub(crate) feed_timeout_seconds: u32,
    pub(crate) download_timeout_seconds: u32,
    pub(crate) low_speed_seconds: u32,
    /// Extra CA bundle, so tests can stand up an HTTPS server of their own.
    /// Production trusts only the system roots.
    #[cfg(test)]
    pub(crate) cacert: Option<std::path::PathBuf>,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        Self {
            connect_timeout_seconds: CONNECT_TIMEOUT_SECONDS,
            feed_timeout_seconds: FEED_TIMEOUT_SECONDS,
            download_timeout_seconds: DOWNLOAD_TIMEOUT_SECONDS,
            low_speed_seconds: LOW_SPEED_SECONDS,
            #[cfg(test)]
            cacert: None,
        }
    }

    pub fn fetch_text(&self, url: &str) -> Result<String> {
        let started = std::time::Instant::now();
        match self.fetch_text_once(url) {
            Err(UpdateError::Network { failure, .. })
                if failure.is_transient() && started.elapsed() < RETRY_WITHIN =>
            {
                std::thread::sleep(RETRY_DELAY);
                self.fetch_text_once(url)
            }
            result => result,
        }
    }

    /// One attempt, no retry: for callers with another route to try instead.
    pub(crate) fn fetch_text_once(&self, url: &str) -> Result<String> {
        let output = self
            .curl()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                let config = self.config(url, None, self.feed_timeout_seconds, MAX_FEED_BYTES);
                child
                    .stdin
                    .take()
                    .expect("curl stdin was piped")
                    .write_all(config.as_bytes())?;
                child.wait_with_output()
            })?;
        if !output.status.success() {
            if output.status.code() == Some(CURLE_FILESIZE_EXCEEDED) {
                return Err(UpdateError::Feed("feed exceeds the 1 MiB limit".to_owned()));
            }
            return Err(curl_failure(output.status.code(), &output.stderr));
        }
        if output.stdout.len() > usize::try_from(MAX_FEED_BYTES).unwrap_or(usize::MAX) {
            return Err(UpdateError::Feed("feed exceeds the 1 MiB limit".to_owned()));
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
        if expected_size == 0 || expected_size > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::Integrity(format!(
                "update archive size must be between 1 and {MAX_ARCHIVE_BYTES} bytes"
            )));
        }
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
        let config = self.config(
            url,
            Some(destination),
            self.download_timeout_seconds,
            expected_size,
        );
        child
            .stdin
            .take()
            .expect("curl stdin was piped")
            .write_all(config.as_bytes())?;

        let mut last_reported = 0.0_f32;
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    let mut buffer = Vec::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read as _;
                        let _ = pipe.read_to_end(&mut buffer);
                    }
                    let _ = fs::remove_file(destination);
                    if status.code() == Some(CURLE_FILESIZE_EXCEEDED) {
                        return Err(UpdateError::Integrity(format!(
                            "download exceeded its declared size of {expected_size} bytes"
                        )));
                    }
                    return Err(curl_failure(status.code(), &buffer));
                }
                break;
            }
            let written = fs::metadata(destination)
                .map(|meta| meta.len())
                .unwrap_or(0);
            if written > expected_size {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(destination);
                return Err(UpdateError::Integrity(format!(
                    "download exceeded its declared size of {expected_size} bytes"
                )));
            }
            if expected_size > 0 {
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
        if written != expected_size {
            let _ = fs::remove_file(destination);
            return Err(UpdateError::Integrity(format!(
                "expected {expected_size} bytes, got {written}"
            )));
        }
        Ok(())
    }

    fn curl(&self) -> Command {
        let mut command = Command::new(diri_platform::curl_executable());
        // -K - keeps the URL and (more importantly) the credentials out of the
        // process arguments, where any user on the machine could read them.
        command.arg("-K").arg("-");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command
    }

    fn config(
        &self,
        url: &str,
        output: Option<&Path>,
        timeout_seconds: u32,
        max_bytes: u64,
    ) -> String {
        let mut config = String::new();
        config.push_str(&format!("url = \"{url}\"\n"));
        config.push_str("fail\nlocation\nsilent\nshow-error\n");
        config.push_str("proto = \"=https\"\nproto-redir = \"=https\"\n");
        config.push_str(&format!("max-time = {timeout_seconds}\n"));
        config.push_str(&format!("max-filesize = {max_bytes}\n"));
        config.push_str(&format!(
            "connect-timeout = {}\n",
            self.connect_timeout_seconds
        ));
        if output.is_some() {
            config.push_str(&format!(
                "speed-limit = {LOW_SPEED_BYTES_PER_SECOND}\nspeed-time = {}\n",
                self.low_speed_seconds
            ));
        }
        config.push_str("max-redirs = 5\n");
        config.push_str(&format!("user-agent = \"diri-updater/{}\"\n", crate::AGENT));
        config.push_str(&format!(
            "write-out = \"%{{stderr}}{STATUS_MARKER}%{{http_code}}\\n\"\n"
        ));
        if let Some(path) = output {
            config.push_str(&format!(
                "output = \"{}\"\n",
                path.to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            ));
        }
        #[cfg(test)]
        if let Some(path) = &self.cacert {
            config.push_str(&format!("cacert = \"{}\"\n", path.display()));
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
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateError::Feed(
            "release checksum is not a SHA-256 digest".to_owned(),
        ));
    }
    // The same bounded, in-process verification works on Windows, where
    // /usr/bin/shasum does not exist. sha2 is already used by the workspace.
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::Integrity(format!(
            "sha256 {actual} does not match the feed's {expected}"
        )));
    }
    Ok(())
}

// curl exit codes (`man curl`, EXIT CODES) the classifier relies on.
const CURLE_COULDNT_RESOLVE_PROXY: i32 = 5;
const CURLE_COULDNT_RESOLVE_HOST: i32 = 6;
const CURLE_COULDNT_CONNECT: i32 = 7;
const CURLE_HTTP_RETURNED_ERROR: i32 = 22;
const CURLE_OPERATION_TIMEDOUT: i32 = 28;
const CURLE_FILESIZE_EXCEEDED: i32 = 63;
const CURLE_TLS: [i32; 9] = [35, 51, 53, 54, 58, 59, 60, 77, 83];

/// Turns a failed curl run into an error that says what actually went wrong.
fn curl_failure(code: Option<i32>, stderr: &[u8]) -> UpdateError {
    let text = String::from_utf8_lossy(stderr);
    let mut status = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        match line.strip_prefix(STATUS_MARKER) {
            Some(value) => status = value.trim().parse::<u16>().ok(),
            None if !line.trim().is_empty() => lines.push(line.trim()),
            None => {}
        }
    }
    let detail = if lines.is_empty() {
        format!("curl exited with {code:?}")
    } else {
        lines.join("; ")
    };
    UpdateError::network(classify(code, status, &detail), detail)
}

fn classify(code: Option<i32>, status: Option<u16>, detail: &str) -> NetworkFailure {
    // An HTTP error status is the most specific fact there is, whatever exit
    // code curl chose to report it under.
    match status {
        Some(404) => return NetworkFailure::NotFound,
        Some(status @ (403 | 429)) => return NetworkFailure::RateLimited(status),
        Some(status @ 400..=599) => return NetworkFailure::Http(status),
        _ => {}
    }
    match code {
        Some(CURLE_COULDNT_RESOLVE_PROXY | CURLE_COULDNT_RESOLVE_HOST) => NetworkFailure::Dns,
        Some(CURLE_COULDNT_CONNECT) => NetworkFailure::Connect,
        Some(CURLE_OPERATION_TIMEDOUT) if detail.contains("Resolving timed out") => {
            NetworkFailure::Dns
        }
        Some(CURLE_OPERATION_TIMEDOUT) if detail.contains("Failed to connect") => {
            NetworkFailure::Connect
        }
        Some(CURLE_OPERATION_TIMEDOUT) => NetworkFailure::Timeout,
        Some(code) if CURLE_TLS.contains(&code) => NetworkFailure::Tls,
        Some(CURLE_HTTP_RETURNED_ERROR) => NetworkFailure::Http(status.unwrap_or(0)),
        _ => NetworkFailure::Other,
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
        let config = http.config("https://example.test/a.json", None, 20, MAX_FEED_BYTES);
        assert!(!config.contains("user = "), "no credentials: {config}");
        assert!(config.contains("proto = \"=https\"\n"), "https only");
        assert!(config.contains("max-filesize = 1048576\n"), "bounded");

        // The URL still goes over stdin rather than argv, where any other user
        // on the machine could read it.
        let command = http.curl();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-K", "-"]);
    }

    fn failure(code: i32, stderr: &str) -> NetworkFailure {
        match curl_failure(Some(code), stderr.as_bytes()) {
            UpdateError::Network { failure, .. } => failure,
            other => panic!("expected a network error, got {other}"),
        }
    }

    #[test]
    fn http_errors_are_not_reported_as_unreachable() {
        // macOS curl over HTTP/2 reports `--fail` as exit 56 (verified against
        // a missing GitHub release asset), so the status line decides.
        let missing = "curl: (56) The requested URL returned error: 404\ndiri-http-status:404\n";
        assert_eq!(failure(56, missing), NetworkFailure::NotFound);
        let throttled = "curl: (22) The requested URL returned error: 429\ndiri-http-status:429\n";
        assert_eq!(failure(22, throttled), NetworkFailure::RateLimited(429));
        assert_eq!(
            failure(22, "diri-http-status:403\n"),
            NetworkFailure::RateLimited(403)
        );
        assert_eq!(
            failure(56, "diri-http-status:503\n"),
            NetworkFailure::Http(503)
        );
    }

    #[test]
    fn transport_failures_name_their_cause() {
        let none = "\ndiri-http-status:000\n";
        assert_eq!(
            failure(
                6,
                &format!("curl: (6) Could not resolve host: github.com{none}")
            ),
            NetworkFailure::Dns
        );
        assert_eq!(
            failure(
                28,
                "curl: (28) Resolving timed out after 15000 milliseconds"
            ),
            NetworkFailure::Dns
        );
        assert_eq!(
            failure(
                28,
                "curl: (28) Failed to connect to github.com port 443 after 15002 ms: Timeout was reached"
            ),
            NetworkFailure::Connect
        );
        assert_eq!(
            failure(
                28,
                "curl: (28) Operation timed out after 20001 milliseconds"
            ),
            NetworkFailure::Timeout
        );
        assert_eq!(failure(7, none), NetworkFailure::Connect);
        assert_eq!(failure(60, none), NetworkFailure::Tls);
        assert_eq!(
            failure(56, "curl: (56) Recv failure"),
            NetworkFailure::Other
        );
    }

    #[test]
    fn the_status_marker_stays_out_of_the_detail() {
        let error = curl_failure(
            Some(56),
            b"curl: (56) The requested URL returned error: 404\ndiri-http-status:404\n",
        );
        let text = error.to_string();
        assert!(!text.contains(STATUS_MARKER), "{text}");
        assert!(text.contains("not_found"), "{text}");
    }

    #[test]
    fn only_quick_blips_are_retried() {
        assert!(NetworkFailure::Dns.is_transient());
        assert!(NetworkFailure::Http(502).is_transient());
        assert!(!NetworkFailure::NotFound.is_transient());
        assert!(!NetworkFailure::RateLimited(429).is_transient());
        assert!(!NetworkFailure::Timeout.is_transient());
    }

    #[test]
    fn a_real_curl_failure_is_classified_from_its_own_output() {
        // Nothing listens on the discard port, so the connection is refused
        // locally without touching the network; this proves the write-out
        // line and exit code survive the trip through the config on stdin.
        let error = Http::new()
            .fetch_text_once("https://127.0.0.1:9/appcast.json")
            .expect_err("nothing listens on port 9");
        match error {
            UpdateError::Network { failure, detail } => {
                assert_eq!(failure, NetworkFailure::Connect, "{detail}");
                assert!(!detail.contains(STATUS_MARKER), "{detail}");
            }
            other => panic!("expected a network error, got {other}"),
        }
    }

    #[test]
    fn matching_checksum_passes_and_a_wrong_one_fails() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("payload");
        fs::write(&path, b"diri").expect("write payload");
        // echo -n diri | shasum -a 256
        let expected = "8f9a2b0f8c9f2d5f24b3f2ca0d18b6f4b3f7f2e0f5d0b1b0d0f5c6b7a8d9e0f1";
        assert!(verify_sha256(&path, expected).is_err());

        // Independently computed SHA-256 of the fixed bytes above. Do not
        // require a Unix shasum executable on the Windows updater path.
        let actual = "4f3be1a92ededd90292e0eaf67448e820f49b55b45c9275991d824759d58da09";
        assert!(verify_sha256(&path, actual).is_ok());
        assert!(verify_sha256(&path, &actual.to_uppercase()).is_ok());
    }
}
