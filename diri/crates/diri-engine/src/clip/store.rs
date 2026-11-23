//! The Context Ring on disk: an owner-only (`0700`) directory holding
//! `ring.json` (the index), `affinity.json` (per-tab memory) and one
//! owner-only (`0600`) PNG per clip. Every write goes to a fresh temp file
//! and is renamed into place. Nothing runs in the background: expiry is
//! applied whenever the ring is read or written.
//!
//! Captured pixels, text and URLs are never logged.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};

use diri_proto::clip::{ClipRecord, ClipSaveParams, MAX_PNG_BYTES, MAX_TEXT_BYTES};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::affinity::Affinity;
use super::prompt::clamp_text;
use super::ring::{self, Limits};
use super::tab_key;

const RING_FILE: &str = "ring.json";
const AFFINITY_FILE: &str = "affinity.json";
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// One process-wide lock: the control server handles requests on several
/// threads, and the index is read-modify-written.
static LOCK: Mutex<()> = Mutex::new(());

pub struct ClipStore {
    dir: PathBuf,
    limits: Limits,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

impl ClipStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            limits: Limits::default(),
        }
    }

    #[cfg(test)]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the directory owner-only, or verifies an existing one is a
    /// real directory this user owns, and tightens it to `0700`.
    fn ensure_dir(&self) -> io::Result<()> {
        match fs::symlink_metadata(&self.dir) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() {
                    return Err(invalid("the clip directory is not a directory"));
                }
                #[cfg(unix)]
                {
                    if metadata.uid() != unsafe { libc::geteuid() } {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "the clip directory is not owned by this user",
                        ));
                    }
                    if metadata.mode() & 0o777 != 0o700 {
                        use std::os::unix::fs::PermissionsExt as _;
                        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
                    }
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = self.dir.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                builder.mode(0o700);
                builder.create(&self.dir)
            }
            Err(error) => Err(error),
        }
    }

    fn read_json<T: DeserializeOwned + Default>(&self, name: &str) -> io::Result<T> {
        let path = self.dir.join(name);
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        // A damaged index starts the ring over rather than wedging capture.
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        self.write_private(name, &bytes)
    }

    /// Writes `bytes` to `name` through an owner-only temp file and rename.
    fn write_private(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temp = self.dir.join(format!(".{name}.{}.tmp", random_hex(6)));
        let result = (|| {
            let mut file = create_private(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temp, self.dir.join(name))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn load_ring(&self, now_ms: u64) -> io::Result<Vec<ClipRecord>> {
        let ring: Vec<ClipRecord> = self.read_json(RING_FILE)?;
        let before = ring.len();
        let (kept, evicted) = ring::evict(ring, now_ms, self.limits);
        if !evicted.is_empty() || kept.len() != before {
            self.remove_files(&evicted);
            self.write_json(RING_FILE, &kept)?;
        }
        Ok(kept)
    }

    fn remove_files(&self, clips: &[ClipRecord]) {
        for clip in clips {
            // Only files inside our own directory, named by our own ids.
            if valid_id(&clip.id) {
                let _ = fs::remove_file(self.dir.join(format!("{}.png", clip.id)));
            }
        }
    }

    /// Removes screenshots no index entry names (left by a damaged index
    /// or an interrupted save). Runs only when a clip is saved.
    fn sweep_orphans(&self, ring: &[ClipRecord]) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_suffix(".png") else {
                continue;
            };
            if valid_id(id) && !ring.iter().any(|clip| clip.id == id) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn resolve<'a>(ring: &'a [ClipRecord], id: &str) -> Option<&'a ClipRecord> {
        if id == "latest" {
            ring.first()
        } else {
            ring.iter().find(|clip| clip.id == id)
        }
    }

    /// Ingests a capture: copies the PNG into the ring, keeps the text,
    /// files the page key, and evicts what no longer fits.
    pub fn save(&self, params: ClipSaveParams, now_ms: u64) -> io::Result<ClipRecord> {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ensure_dir()?;
        let id = format!("{:011x}{}", now_ms, random_hex(5));
        let (png_path, png_bytes, width, height) = match params.png_path.as_deref() {
            Some(source) => {
                let (bytes, width, height) = self.ingest_png(Path::new(source), &id)?;
                (
                    Some(
                        self.dir
                            .join(format!("{id}.png"))
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    bytes,
                    width,
                    height,
                )
            }
            None => (None, 0, params.width, params.height),
        };
        let (text, text_truncated) = match params.text.filter(|t| !t.trim().is_empty()) {
            Some(text) => {
                let (text, cut) = clamp_text(text, MAX_TEXT_BYTES);
                (Some(text), cut)
            }
            None => (None, false),
        };
        let text_origin = if text.is_some() {
            params.text_origin
        } else {
            diri_proto::clip::ClipTextOrigin::None
        };
        let record = ClipRecord {
            id,
            created_at_ms: now_ms,
            tab_key: tab_key::tab_key(&params.source),
            source: params.source,
            text,
            text_origin,
            text_truncated,
            png_path,
            png_bytes,
            width,
            height,
            sent_to: Vec::new(),
        };
        let mut ring: Vec<ClipRecord> = self.read_json(RING_FILE)?;
        ring.insert(0, record.clone());
        let (kept, evicted) = ring::evict(ring, now_ms, self.limits);
        self.remove_files(&evicted);
        self.write_json(RING_FILE, &kept)?;
        self.sweep_orphans(&kept);
        if !kept.iter().any(|clip| clip.id == record.id) {
            return Err(invalid("the clip is larger than the Context Ring allows"));
        }
        Ok(record)
    }

    /// Copies a caller's PNG into `<id>.png` without following links,
    /// checking ownership, size and the PNG signature; answers its length
    /// and the dimensions from its header.
    fn ingest_png(&self, source: &Path, id: &str) -> io::Result<(u64, u32, u32)> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let mut file = options.open(source)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("the screenshot is not a regular file"));
        }
        #[cfg(unix)]
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the screenshot is not owned by this user",
            ));
        }
        if metadata.len() > MAX_PNG_BYTES {
            return Err(invalid("the screenshot is too large"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_PNG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PNG_BYTES {
            return Err(invalid("the screenshot is too large"));
        }
        let (width, height) = png_size(&bytes).ok_or_else(|| invalid("not a PNG image"))?;
        self.write_private(&format!("{id}.png"), &bytes)?;
        Ok((bytes.len() as u64, width, height))
    }

    pub fn list(&self, now_ms: u64) -> io::Result<Vec<ClipRecord>> {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        self.ensure_dir()?;
        self.load_ring(now_ms)
    }

    pub fn get(&self, id: &str, now_ms: u64) -> io::Result<Option<ClipRecord>> {
        Ok(Self::resolve(&self.list(now_ms)?, id).cloned())
    }

    pub fn delete(&self, id: &str, now_ms: u64) -> io::Result<Option<String>> {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.dir.exists() {
            return Ok(None);
        }
        self.ensure_dir()?;
        let mut ring = self.load_ring(now_ms)?;
        let Some(found) = Self::resolve(&ring, id).map(|clip| clip.id.clone()) else {
            return Ok(None);
        };
        let removed: Vec<_> = ring.iter().filter(|c| c.id == found).cloned().collect();
        ring.retain(|clip| clip.id != found);
        self.remove_files(&removed);
        self.write_json(RING_FILE, &ring)?;
        Ok(Some(found))
    }

    /// Notes that `clip_id` went to `session_id` and remembers the Session
    /// for the clip's page. Answers the page key remembered, if any.
    pub fn mark_sent(
        &self,
        clip_id: &str,
        session_id: &str,
        now_ms: u64,
    ) -> io::Result<Option<String>> {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ensure_dir()?;
        let mut ring = self.load_ring(now_ms)?;
        let Some(clip) = ring.iter_mut().find(|clip| clip.id == clip_id) else {
            return Ok(None);
        };
        let session = diri_proto::SessionId::new(session_id);
        if clip.sent_to.last() != Some(&session) {
            clip.sent_to.push(session);
        }
        let key = clip.tab_key.clone();
        self.write_json(RING_FILE, &ring)?;
        if let Some(key) = &key {
            let mut affinity: Affinity = self.read_json(AFFINITY_FILE)?;
            affinity.record(key, session_id, now_ms);
            self.write_json(AFFINITY_FILE, &affinity)?;
        }
        Ok(key)
    }

    pub fn affinity(&self) -> io::Result<Affinity> {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.dir.exists() {
            return Ok(Affinity::default());
        }
        self.read_json(AFFINITY_FILE)
    }
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

/// Ids are lowercase hex the store minted; anything else is never used to
/// build a path.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::fill(&mut buffer).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = (nanos >> (index * 8)) as u8;
        }
    }
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// Width and height from a PNG's IHDR chunk.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != PNG_MAGIC || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::clip::{ClipSource, ClipTextOrigin};

    /// A valid 2x3 PNG header followed by filler.
    fn png(width: u32, height: u32, filler: usize) -> Vec<u8> {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend(std::iter::repeat_n(0u8, filler));
        bytes
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "diri-clip-test-{name}-{}-{}",
            std::process::id(),
            random_hex(4)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn params(dir: &Path, url: &str, text: Option<&str>) -> ClipSaveParams {
        let source = dir.join(format!("in-{}.png", random_hex(3)));
        fs::write(&source, png(2, 3, 10)).unwrap();
        ClipSaveParams {
            source: ClipSource {
                url: Some(url.into()),
                ..Default::default()
            },
            text: text.map(str::to_owned),
            text_origin: ClipTextOrigin::Ocr,
            png_path: Some(source.to_string_lossy().into_owned()),
            width: 999,
            height: 999,
        }
    }

    #[test]
    fn saves_privately_and_reads_back() {
        let root = temp_dir("save");
        let store = ClipStore::new(root.join("clips"));
        let clip = store
            .save(
                params(&root, "https://example.com/a?utm_x=1", Some("hello")),
                1_000,
            )
            .unwrap();
        assert_eq!(clip.tab_key.as_deref(), Some("https://example.com/a"));
        assert_eq!(
            (clip.width, clip.height),
            (2, 3),
            "dimensions come from the PNG"
        );
        assert_eq!(clip.png_bytes, 34);
        let png = PathBuf::from(clip.png_path.clone().unwrap());
        assert!(png.starts_with(store.dir()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(store.dir()), 0o700);
            assert_eq!(mode(&png), 0o600);
            assert_eq!(mode(&store.dir().join(RING_FILE)), 0o600);
        }
        assert_eq!(store.get("latest", 1_001).unwrap().unwrap(), clip);
        assert_eq!(
            store.get(&clip.id, 1_001).unwrap().unwrap().text.as_deref(),
            Some("hello")
        );
        assert_eq!(
            store.delete(&clip.id, 1_002).unwrap(),
            Some(clip.id.clone())
        );
        assert!(!png.exists());
        assert!(store.list(1_003).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eviction_removes_screenshots() {
        let root = temp_dir("evict");
        let store = ClipStore::new(root.join("clips")).with_limits(Limits {
            max_clips: 2,
            max_bytes: 1 << 20,
            ttl_ms: 10_000,
        });
        let first = store
            .save(params(&root, "https://a.io/1", None), 1)
            .unwrap();
        store
            .save(params(&root, "https://a.io/2", None), 2)
            .unwrap();
        store
            .save(params(&root, "https://a.io/3", None), 3)
            .unwrap();
        let ids: Vec<_> = store
            .list(4)
            .unwrap()
            .into_iter()
            .map(|c| c.created_at_ms)
            .collect();
        assert_eq!(ids, [3, 2]);
        assert!(!PathBuf::from(first.png_path.unwrap()).exists());
        // Expiry happens on read, with no timer.
        assert!(store.list(20_000).unwrap().is_empty());
        let leftover: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .collect();
        assert!(leftover.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_links_non_pngs_and_oversized_text() {
        let root = temp_dir("reject");
        let store = ClipStore::new(root.join("clips"));
        let mut bad = params(&root, "https://a.io", None);
        fs::write(bad.png_path.as_ref().unwrap(), b"not a png at all, sorry!").unwrap();
        assert!(store.save(bad.clone(), 1).is_err());
        #[cfg(unix)]
        {
            let real = root.join("real.png");
            fs::write(&real, png(1, 1, 0)).unwrap();
            let link = root.join("link.png");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            bad.png_path = Some(link.to_string_lossy().into_owned());
            assert!(store.save(bad, 1).is_err(), "symlinks are not followed");
        }
        let long = "é".repeat(MAX_TEXT_BYTES);
        let clip = store
            .save(params(&root, "https://a.io", Some(&long)), 2)
            .unwrap();
        assert!(clip.text_truncated);
        assert!(clip.text.unwrap().len() <= MAX_TEXT_BYTES);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sending_records_per_tab_affinity_that_persists() {
        let root = temp_dir("affinity");
        let dir = root.join("clips");
        let store = ClipStore::new(&dir);
        let clip = store
            .save(params(&root, "http://127.0.0.1:3000/settings/", None), 5)
            .unwrap();
        let key = store.mark_sent(&clip.id, "s1", 6).unwrap();
        assert_eq!(key.as_deref(), Some("http://localhost:3000/settings"));
        store.mark_sent(&clip.id, "s1", 7).unwrap();
        let reopened = ClipStore::new(&dir);
        assert_eq!(reopened.get(&clip.id, 8).unwrap().unwrap().sent_to.len(), 1);
        let affinity = reopened.affinity().unwrap();
        assert_eq!(
            affinity.lookup("http://localhost:3000/billing", |_| true),
            Some("s1")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_damaged_index_starts_over() {
        let root = temp_dir("damaged");
        let store = ClipStore::new(root.join("clips"));
        store.save(params(&root, "https://a.io", None), 1).unwrap();
        fs::write(store.dir().join(RING_FILE), b"{not json").unwrap();
        assert!(store.list(2).unwrap().is_empty());
        store.save(params(&root, "https://a.io", None), 3).unwrap();
        assert_eq!(store.list(4).unwrap().len(), 1);
        let pngs = fs::read_dir(store.dir())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .count();
        assert_eq!(pngs, 1, "the orphaned screenshot was swept");
        fs::remove_dir_all(root).unwrap();
    }
}
