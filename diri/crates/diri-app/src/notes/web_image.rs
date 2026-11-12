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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest as _, Sha256};

/// The largest picture fetched: the same bound a pasted picture has.
const MAX_BYTES: usize = diri_notes::store::MAX_ASSET_BYTES;
/// The cache folder is trimmed to this, least recently shown first.
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// A failed picture is tried again after this, doubling up to the last.
const FIRST_RETRY: Duration = Duration::from_secs(30);
const LAST_RETRY: Duration = Duration::from_secs(60 * 60);
/// A temporary older than this is a crashed download, not one in flight.
const STALE_TEMPORARY: Duration = Duration::from_secs(10 * 60);
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

/// What one editor knows about one web picture.
struct Known {
    state: WebImage,
    /// After a failure: when the next draw may try again, and the wait
    /// after that one fails too.
    retry: Option<(Instant, Duration)>,
}

/// What one editor knows about the web pictures in its note, by address.
#[derive(Default)]
pub(crate) struct WebImages {
    known: HashMap<String, Known>,
}

impl WebImages {
    /// The picture's state, and whether the caller must start its download.
    /// A picture already in the cache is ready at once, with no request.
    /// A failed one is tried again when it is drawn after its back-off,
    /// so a note opened offline loads once the network is back; nothing
    /// wakes an idle editor to do it.
    pub(crate) fn look(&mut self, url: &str, cache: &Path) -> (WebImage, bool) {
        self.look_at(url, cache, Instant::now())
    }

    fn look_at(&mut self, url: &str, cache: &Path, now: Instant) -> (WebImage, bool) {
        if let Some(known) = self.known.get_mut(url) {
            return match known.retry {
                Some((at, _)) if known.state == WebImage::Failed && now >= at => {
                    known.state = WebImage::Loading;
                    (WebImage::Loading, true)
                }
                _ => (known.state.clone(), false),
            };
        }
        let file = cache_file(cache, url);
        let (state, start) = if file.is_file() {
            // Shown again: it is the newest, so trimming takes others first.
            touch(&file);
            (WebImage::Ready(file), false)
        } else {
            (WebImage::Loading, true)
        };
        self.known.insert(
            url.to_owned(),
            Known {
                state: state.clone(),
                retry: None,
            },
        );
        (state, start)
    }

    /// Records how a download ended.
    pub(crate) fn landed(&mut self, url: &str, file: Option<PathBuf>) {
        self.landed_at(url, file, Instant::now());
    }

    fn landed_at(&mut self, url: &str, file: Option<PathBuf>, now: Instant) {
        let previous = self.known.get(url).and_then(|known| known.retry);
        let known = match file {
            Some(file) => Known {
                state: WebImage::Ready(file),
                retry: None,
            },
            None => {
                let wait = previous.map_or(FIRST_RETRY, |(_, wait)| (wait * 2).min(LAST_RETRY));
                Known {
                    state: WebImage::Failed,
                    retry: Some((now + wait, wait)),
                }
            }
        };
        self.known.insert(url.to_owned(), known);
    }
}

/// Marks a cached picture as just shown, for [`trim`]'s order.
fn touch(file: &Path) {
    if let Ok(handle) = std::fs::File::options().write(true).open(file) {
        let _ = handle.set_modified(SystemTime::now());
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
    // One temporary per download: two editors fetching the same address
    // at once must not truncate or rename each other's file.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary = cache.join(format!(
        ".{}.{}.{}.tmp",
        file.file_name()?.to_str()?,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
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

/// Deletes the least recently shown pictures until the folder fits in
/// `limit`, keeping `keep` (the one just written). A download still in
/// flight keeps its temporary; one left by a crash is cleared.
fn trim(cache: &Path, limit: u64, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let now = SystemTime::now();
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if entry.file_name().to_string_lossy().starts_with('.') {
                if now.duration_since(modified).unwrap_or_default() > STALE_TEMPORARY {
                    let _ = std::fs::remove_file(entry.path());
                }
                return None;
            }
            Some((modified, meta.len(), entry.path()))
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
                let body = body.clone();
                // One thread per request, so concurrent downloads overlap.
                std::thread::spawn(move || {
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
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

    fn age(path: &Path, seconds: u64) {
        let when = SystemTime::now() - Duration::from_secs(seconds);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn a_picture_shown_again_outlives_newer_downloads() {
        let cache = tempfile::tempdir().unwrap();
        let url = "https://example.com/shown.png";
        let shown = cache_file(cache.path(), url);
        let other = cache.path().join("other");
        let fresh = cache.path().join("fresh");
        for (path, seconds) in [(&shown, 300), (&other, 200), (&fresh, 0)] {
            std::fs::write(path, [0u8; 10]).unwrap();
            age(path, seconds);
        }
        // A note opens and shows the oldest picture from the cache.
        assert_eq!(
            WebImages::default().look(url, cache.path()),
            (WebImage::Ready(shown.clone()), false)
        );
        trim(cache.path(), 20, &fresh);
        assert!(shown.exists() && !other.exists() && fresh.exists());
    }

    #[test]
    fn trimming_spares_downloads_in_flight_and_clears_crashed_ones() {
        let cache = tempfile::tempdir().unwrap();
        let in_flight = cache.path().join(".abc.1.0.tmp");
        let crashed = cache.path().join(".abc.1.1.tmp");
        let kept = cache.path().join("kept");
        for path in [&in_flight, &crashed, &kept] {
            std::fs::write(path, [0u8; 10]).unwrap();
        }
        age(&crashed, STALE_TEMPORARY.as_secs() + 60);
        trim(cache.path(), 0, &kept);
        assert!(in_flight.exists() && !crashed.exists() && kept.exists());
    }

    #[test]
    fn a_failed_picture_is_tried_again_with_a_growing_wait() {
        let cache = tempfile::tempdir().unwrap();
        let url = "https://example.com/offline.png";
        let start = Instant::now();
        let mut images = WebImages::default();
        assert_eq!(
            images.look_at(url, cache.path(), start),
            (WebImage::Loading, true)
        );
        images.landed_at(url, None, start);

        let mut now = start;
        for wait in [30, 60, 120] {
            let before = now + Duration::from_secs(wait - 1);
            assert_eq!(
                images.look_at(url, cache.path(), before),
                (WebImage::Failed, false),
                "still waiting {wait}s"
            );
            now += Duration::from_secs(wait);
            assert_eq!(
                images.look_at(url, cache.path(), now),
                (WebImage::Loading, true)
            );
            // Drawn again while that retry runs: no second download.
            assert_eq!(
                images.look_at(url, cache.path(), now),
                (WebImage::Loading, false)
            );
            images.landed_at(url, None, now);
        }

        // The network is back: the next retry lands and stays.
        now += Duration::from_secs(240);
        assert_eq!(
            images.look_at(url, cache.path(), now),
            (WebImage::Loading, true)
        );
        let file = cache.path().join("landed");
        images.landed_at(url, Some(file.clone()), now);
        let later = now + LAST_RETRY * 2;
        assert_eq!(
            images.look_at(url, cache.path(), later),
            (WebImage::Ready(file), false)
        );
    }

    #[test]
    fn the_wait_between_retries_stops_growing() {
        let cache = tempfile::tempdir().unwrap();
        let url = "https://example.com/gone.png";
        let mut now = Instant::now();
        let mut images = WebImages::default();
        images.look_at(url, cache.path(), now);
        for _ in 0..20 {
            images.landed_at(url, None, now);
            now += LAST_RETRY;
            assert_eq!(
                images.look_at(url, cache.path(), now),
                (WebImage::Loading, true)
            );
        }
    }

    #[test]
    fn two_editors_fetching_one_address_at_once_both_get_it() {
        let cache = tempfile::tempdir().unwrap();
        // A body large enough that the writes overlap.
        let mut body = png();
        body.resize(4 * 1024 * 1024, 0);
        let (address, _) = serve("200 OK", String::new(), body.clone());
        let url = format!("{address}/same.png");
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| fetch_with(curl(), cache.path(), &url)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for result in &results {
            let file = result.as_ref().expect("every editor gets the picture");
            assert_eq!(std::fs::read(file).unwrap(), body);
        }
        let leftovers = std::fs::read_dir(cache.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(leftovers, 0);
    }
}
