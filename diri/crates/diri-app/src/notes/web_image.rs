//! Pictures a note links by web address (`![](https://…)`).
//!
//! A note keeps the address; the picture is downloaded once, the first time
//! its row is drawn, into a cache folder and shown from there like a pasted
//! one. The note itself is never rewritten.
//!
//! The download goes through the system `curl`, the way the rest of diri
//! sends HTTPS (see `api_client::http`), and stays narrow:
//!
//! - only `http` and `https`, to start with and across redirects, and never
//!   an address carrying a user name or password;
//! - `-q` first so no `~/.curlrc` applies, and the address travels over
//!   stdin (`-K -`), never the argument list;
//! - no cookies, headers or credentials are sent;
//! - the body is read with a hard cap and `curl` killed past it;
//! - only bytes that start like a PNG, JPEG, GIF or WebP are kept.
//!
//! Nothing here logs or records the address.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use sha2::{Digest as _, Sha256};

/// The largest picture fetched: the same bound a pasted picture has.
const MAX_BYTES: usize = diri_notes::store::MAX_ASSET_BYTES;
/// The cache folder is trimmed to this, oldest pictures first.
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const TIMEOUT_SECONDS: u32 = 30;
const CONNECT_TIMEOUT_SECONDS: u32 = 10;
const MAX_REDIRECTS: u32 = 5;
const CACHE_FOLDER: &str = "note-images";

/// Where a web picture stands for the view drawing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WebImage {
    Loading,
    Ready(PathBuf),
    Failed,
}

/// What one editor knows about the web pictures in its note, by address.
#[derive(Default)]
pub(crate) struct WebImages {
    known: HashMap<String, WebImage>,
}

impl WebImages {
    /// The picture's state, and whether the caller must start its download.
    /// A picture already in the cache is ready at once, with no request.
    pub(crate) fn look(&mut self, url: &str, cache: &Path) -> (WebImage, bool) {
        if let Some(state) = self.known.get(url) {
            return (state.clone(), false);
        }
        let file = cache_file(cache, url);
        let (state, start) = if file.is_file() {
            (WebImage::Ready(file), false)
        } else {
            (WebImage::Loading, true)
        };
        self.known.insert(url.to_owned(), state.clone());
        (state, start)
    }

    /// Records how a download ended.
    pub(crate) fn landed(&mut self, url: &str, file: Option<PathBuf>) {
        let state = file.map_or(WebImage::Failed, WebImage::Ready);
        self.known.insert(url.to_owned(), state);
    }
}

/// The folder web pictures are kept in, under diri's cache.
pub(crate) fn cache_dir() -> Option<PathBuf> {
    let home = diri_platform::home_dir()?;
    Some(diri_proto::paths::DirijorPaths::cache_dir(home).join(CACHE_FOLDER))
}

/// The address to request for an image `src`, when it is a plain `http` or
/// `https` one. Anything else (a relative path, `file:`, `data:`, an address
/// with credentials in it) is not a web picture.
pub(crate) fn web_url(src: &str) -> Option<String> {
    let src = src.trim();
    if src.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let parsed = url::Url::parse(src).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let url = parsed.as_str();
    // These would end the quoted `url = "…"` line of curl's config. A parsed
    // URL has them percent-encoded already; this is the guard if it does not.
    if url.contains(['"', '\\']) {
        return None;
    }
    Some(url.to_owned())
}

/// Where the picture at `url` is cached: named by the address's hash, so the
/// address is not readable from the folder.
fn cache_file(cache: &Path, url: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    cache.join(name)
}

fn config(url: &str) -> String {
    let mut config = String::new();
    config.push_str(&format!("url = \"{url}\"\n"));
    config.push_str("fail\nsilent\nlocation\n");
    config.push_str("proto = \"=http,https\"\nproto-redir = \"=http,https\"\n");
    config.push_str(&format!("max-redirs = {MAX_REDIRECTS}\n"));
    config.push_str(&format!("max-time = {TIMEOUT_SECONDS}\n"));
    config.push_str(&format!("connect-timeout = {CONNECT_TIMEOUT_SECONDS}\n"));
    config.push_str(&format!("max-filesize = {MAX_BYTES}\n"));
    config.push_str(&format!(
        "user-agent = \"diri/{}\"\n",
        env!("CARGO_PKG_VERSION")
    ));
    config
}

/// Downloads the picture at `url` into `cache` and returns its file. Blocks
/// for the transfer: call it off the main thread. `None` when the address
/// does not answer with a picture diri shows.
pub(crate) fn fetch(cache: &Path, url: &str) -> Option<PathBuf> {
    fetch_with(diri_platform::curl_executable(), cache, url)
}

fn fetch_with(curl: &Path, cache: &Path, url: &str) -> Option<PathBuf> {
    let url = web_url(url)?;
    let file = cache_file(cache, &url);
    if file.is_file() {
        return Some(file);
    }
    let bytes = download(curl, &url)?;
    // A cheap header check, not a decode: an HTML error page, an SVG or a
    // truncated transfer is refused here rather than cached.
    diri_notes::store::image_size_of(&bytes)?;
    diri_platform::private::create_dir_all(cache).ok()?;
    let temporary = cache.join(format!(
        ".{}.{}.tmp",
        file.file_name()?.to_str()?,
        std::process::id()
    ));
    diri_platform::private::replace_via(&temporary, &file, &bytes).ok()?;
    trim(cache, MAX_CACHE_BYTES, &file);
    Some(file)
}

fn download(curl: &Path, url: &str) -> Option<Vec<u8>> {
    let mut command = Command::new(curl);
    command
        .args(["-q", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let sent = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(config(url).as_bytes()));
    let mut bytes = Vec::new();
    let read = child.stdout.take().map(|stdout| {
        stdout
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_ok()
    });
    // `max-filesize` only stops a transfer that declares its length; this
    // stops one that does not.
    let oversized = bytes.len() > MAX_BYTES;
    if oversized {
        let _ = child.kill();
    }
    let finished = child.wait().is_ok_and(|status| status.success());
    let complete = matches!(sent, Some(Ok(()))) && read == Some(true) && finished && !oversized;
    (complete && !bytes.is_empty()).then_some(bytes)
}

/// Deletes the oldest cached pictures until the folder fits in `limit`,
/// keeping `keep` (the one just written).
fn trim(cache: &Path, limit: u64, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                    entry.path(),
                )
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= limit {
        return;
    }
    files.sort();
    for (_, size, path) in files {
        if total <= limit {
            break;
        }
        if path != keep && std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The smallest PNG header `image_size_of` reads a size from: 3 × 2.
    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    /// Answers each connection with `status`, `headers` and `body`, and
    /// returns the address and a count of requests served.
    fn serve(
        status: &'static str,
        headers: String,
        body: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let head = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (address, served)
    }

    fn curl() -> &'static Path {
        diri_platform::curl_executable()
    }

    #[test]
    fn only_plain_web_addresses_are_web_pictures() {
        assert_eq!(
            web_url("https://ucarecdn.com/a5559ec8/shot.png").as_deref(),
            Some("https://ucarecdn.com/a5559ec8/shot.png")
        );
        assert_eq!(
            web_url(" http://example.com/a.png?w=2 ").as_deref(),
            Some("http://example.com/a.png?w=2")
        );
        for refused in [
            "",
            "assets/n/a.png",
            "/etc/passwd",
            "file:///etc/passwd",
            "data:image/png;base64,AAAA",
            "ftp://example.com/a.png",
            "https://user:secret@example.com/a.png",
            "https://user@example.com/a.png",
            "https://exa mple.com/a.png",
            "https://example.com/a.png\noutput = \"/tmp/x\"",
        ] {
            assert_eq!(web_url(refused), None, "{refused}");
        }
    }

    #[test]
    fn an_address_cannot_leave_its_config_line() {
        let url = web_url("https://example.com/a\"b\\c.png").unwrap();
        assert!(!url.contains(['"', '\\']), "{url}");
        let config = config(&url);
        assert_eq!(config.lines().next(), Some(&*format!("url = \"{url}\"")));
        assert!(config.contains("proto-redir = \"=http,https\""));
    }

    #[test]
    fn a_picture_is_downloaded_once_and_then_read_from_the_cache() {
        let cache = tempfile::tempdir().unwrap();
        let (address, served) = serve("200 OK", "Content-Type: image/png\r\n".into(), png());
        let url = format!("{address}/shot.png");

        let mut images = WebImages::default();
        assert_eq!(images.look(&url, cache.path()), (WebImage::Loading, true));
        // Asking again while it loads does not start a second download.
        assert_eq!(images.look(&url, cache.path()), (WebImage::Loading, false));

        let file = fetch_with(curl(), cache.path(), &url).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), png());
        assert_eq!(diri_notes::store::image_size(&file), Some((3, 2)));
        images.landed(&url, Some(file.clone()));
        assert_eq!(
            images.look(&url, cache.path()),
            (WebImage::Ready(file.clone()), false)
        );

        // Another editor, or the next launch: ready with no request.
        assert_eq!(
            WebImages::default().look(&url, cache.path()),
            (WebImage::Ready(file.clone()), false)
        );
        assert_eq!(fetch_with(curl(), cache.path(), &url), Some(file));
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn what_is_not_a_picture_is_not_kept() {
        let cache = tempfile::tempdir().unwrap();
        let (page, _) = serve(
            "200 OK",
            "Content-Type: text/html\r\n".into(),
            b"<html>sign in</html>".to_vec(),
        );
        let (missing, _) = serve("404 Not Found", String::new(), png());
        let (nowhere, _) = serve(
            "302 Found",
            "Location: file:///etc/passwd\r\n".into(),
            Vec::new(),
        );
        for url in [page, missing, nowhere] {
            assert_eq!(fetch_with(curl(), cache.path(), &url), None, "{url}");
        }
        let kept = std::fs::read_dir(cache.path()).map_or(0, Iterator::count);
        assert_eq!(kept, 0);

        let mut images = WebImages::default();
        images.landed("https://example.com/a.png", None);
        assert_eq!(
            images.look("https://example.com/a.png", cache.path()),
            (WebImage::Failed, false)
        );
    }

    #[test]
    fn a_redirect_to_a_picture_is_followed() {
        let cache = tempfile::tempdir().unwrap();
        let (picture, _) = serve("200 OK", String::new(), png());
        let (redirect, _) = serve(
            "302 Found",
            format!("Location: {picture}/real.png\r\n"),
            Vec::new(),
        );
        let file = fetch_with(curl(), cache.path(), &format!("{redirect}/short")).unwrap();
        assert_eq!(std::fs::read(file).unwrap(), png());
    }

    #[test]
    fn the_cache_is_trimmed_oldest_first() {
        let cache = tempfile::tempdir().unwrap();
        let old = cache.path().join("old");
        let new = cache.path().join("new");
        let kept = cache.path().join("kept");
        for (path, age) in [(&kept, 300), (&old, 200), (&new, 100)] {
            std::fs::write(path, [0u8; 10]).unwrap();
            let when = SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        trim(cache.path(), 30, &kept);
        assert!(old.exists() && new.exists() && kept.exists());
        // Over the limit: the oldest goes first, but never the one just written.
        trim(cache.path(), 20, &kept);
        assert!(!old.exists() && new.exists() && kept.exists());
    }
}
