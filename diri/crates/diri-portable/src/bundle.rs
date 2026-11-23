//! The `.diribundle` container: one file, entries streamed in order, each
//! with its SHA-256, optionally sealed under a passphrase.
//!
//! ```text
//! "DIRIBNDL" u16le format u8 flags
//! [encrypted: salt[16] nonce_prefix[7] u32le m_kib u32le t u32le p]
//! payload
//! ```
//! The payload is a sequence of `u32le path_len, path, u64le len,
//! sha256[32], bytes` ending with `u32le 0`. Sealed, the payload is cut into
//! chunks of at most 1 MiB, each `u8 last, u32le len, ciphertext, tag[16]`
//! under AES-256-GCM with nonce `prefix || u32be counter || last` and the
//! header as associated data. A Argon2id key derived from the passphrase
//! seals it. Reordered, truncated, extended or flipped chunks fail
//! authentication; a missing final chunk is detected at end of input.
//!
//! Every entry path must classify as a migration artifact
//! ([`crate::classify::classify_bundle_path`]) on both write and read.

use std::io::{self, Read, Write};

use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes256Gcm, KeyInit};
use zeroize::Zeroize;

use crate::classify::{classify_bundle_path, migration_allowed};
use crate::files::sha256_hex;

const MAGIC: &[u8; 8] = b"DIRIBNDL";
const FORMAT: u16 = 1;
const FLAG_ENCRYPTED: u8 = 1;
const CHUNK: usize = 1024 * 1024;
const TAG: usize = 16;
/// Largest single entry (a transcript is capped lower, at 64 MiB).
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Argon2id cost. RFC 9106's second recommended option (64 MiB, t=3, p=1)
/// at about a quarter second on an M-series Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

impl KdfParams {
    /// Cheap parameters for tests only.
    pub const TEST: Self = Self {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    /// A forged header must not make the reader allocate without bound.
    const MAX_M_KIB: u32 = 1024 * 1024;
    const MAX_T: u32 = 16;
}

#[derive(Debug)]
pub enum BundleError {
    NotABundle,
    UnsupportedFormat(u16),
    PassphraseRequired,
    WrongPassphrase,
    Corrupt(String),
    Io(io::Error),
}

impl BundleError {
    /// A stable code for control errors.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotABundle => "bundle_not_a_bundle",
            Self::UnsupportedFormat(_) => "bundle_unsupported_format",
            Self::PassphraseRequired => "bundle_passphrase_required",
            Self::WrongPassphrase => "bundle_wrong_passphrase",
            Self::Corrupt(_) => "bundle_corrupt",
            Self::Io(_) => "bundle_io",
        }
    }
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotABundle => f.write_str("This file is not a Diri backup"),
            Self::UnsupportedFormat(v) => {
                write!(f, "This backup was made by a newer Diri (format {v})")
            }
            Self::PassphraseRequired => {
                f.write_str("This backup is encrypted. Enter its passphrase")
            }
            Self::WrongPassphrase => f.write_str("That passphrase does not open this backup"),
            Self::Corrupt(why) => write!(f, "The backup is damaged: {why}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BundleError {}

impl From<io::Error> for BundleError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn derive_key(passphrase: &str, salt: &[u8], params: KdfParams) -> io::Result<[u8; 32]> {
    let argon_params = argon2::Params::new(params.m_kib, params.t, params.p, Some(32))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let argon = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon_params,
    );
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(key)
}

fn nonce(prefix: &[u8; 7], counter: u32, last: bool) -> aes_gcm::Nonce<aes_gcm::aead::consts::U12> {
    let mut nonce = [0u8; 12];
    nonce[..7].copy_from_slice(prefix);
    nonce[7..11].copy_from_slice(&counter.to_be_bytes());
    nonce[11] = u8::from(last);
    nonce.into()
}

struct Sealer {
    cipher: Aes256Gcm,
    prefix: [u8; 7],
    aad: Vec<u8>,
    counter: u32,
}

enum Sink<W: Write> {
    Plain(W),
    Sealed {
        out: W,
        sealer: Box<Sealer>,
        buffer: Vec<u8>,
    },
}

impl<W: Write> Sink<W> {
    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(out) => out.write_all(bytes),
            Self::Sealed {
                out,
                sealer,
                buffer,
            } => {
                while !bytes.is_empty() {
                    let room = CHUNK - buffer.len();
                    let take = room.min(bytes.len());
                    buffer.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                    // Seal a full chunk only once more data follows, so the
                    // final chunk is always the one `finish` marks last.
                    if buffer.len() == CHUNK && !bytes.is_empty() {
                        seal_chunk(out, sealer, buffer, false)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn finish(self) -> io::Result<W> {
        match self {
            Self::Plain(mut out) => {
                out.flush()?;
                Ok(out)
            }
            Self::Sealed {
                mut out,
                mut sealer,
                mut buffer,
            } => {
                seal_chunk(&mut out, &mut sealer, &mut buffer, true)?;
                out.flush()?;
                Ok(out)
            }
        }
    }
}

fn seal_chunk<W: Write>(
    out: &mut W,
    sealer: &mut Sealer,
    buffer: &mut Vec<u8>,
    last: bool,
) -> io::Result<()> {
    let tag = sealer
        .cipher
        .encrypt_inout_detached(
            &nonce(&sealer.prefix, sealer.counter, last),
            &sealer.aad,
            (&mut buffer[..]).into(),
        )
        .map_err(|_| io::Error::other("sealing failed"))?;
    sealer.counter = sealer
        .counter
        .checked_add(1)
        .ok_or_else(|| io::Error::other("bundle too large"))?;
    out.write_all(&[u8::from(last)])?;
    out.write_all(&(buffer.len() as u32).to_le_bytes())?;
    out.write_all(buffer)?;
    out.write_all(&tag)?;
    buffer.zeroize();
    buffer.clear();
    Ok(())
}

/// Streams entries into a bundle.
pub struct BundleWriter<W: Write> {
    sink: Sink<W>,
    entries: usize,
}

impl<W: Write> BundleWriter<W> {
    pub fn create(out: W, passphrase: Option<&str>) -> io::Result<Self> {
        Self::create_with(out, passphrase, KdfParams::default())
    }

    pub fn create_with(
        mut out: W,
        passphrase: Option<&str>,
        params: KdfParams,
    ) -> io::Result<Self> {
        let mut header = Vec::with_capacity(48);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT.to_le_bytes());
        let sink = match passphrase.filter(|p| !p.is_empty()) {
            None => {
                header.push(0);
                out.write_all(&header)?;
                Sink::Plain(out)
            }
            Some(passphrase) => {
                header.push(FLAG_ENCRYPTED);
                let mut salt = [0u8; 16];
                let mut prefix = [0u8; 7];
                getrandom::fill(&mut salt).map_err(|_| io::Error::other("no OS randomness"))?;
                getrandom::fill(&mut prefix).map_err(|_| io::Error::other("no OS randomness"))?;
                header.extend_from_slice(&salt);
                header.extend_from_slice(&prefix);
                header.extend_from_slice(&params.m_kib.to_le_bytes());
                header.extend_from_slice(&params.t.to_le_bytes());
                header.extend_from_slice(&params.p.to_le_bytes());
                let mut key = derive_key(passphrase, &salt, params)?;
                let cipher = Aes256Gcm::new(&key.into());
                key.zeroize();
                out.write_all(&header)?;
                Sink::Sealed {
                    out,
                    sealer: Box::new(Sealer {
                        cipher,
                        prefix,
                        aad: header,
                        counter: 0,
                    }),
                    buffer: Vec::with_capacity(CHUNK),
                }
            }
        };
        Ok(Self { sink, entries: 0 })
    }

    /// Adds one entry. Refuses any path that is not a migration artifact.
    pub fn add(&mut self, rel: &str, bytes: &[u8]) -> io::Result<()> {
        let allowed = classify_bundle_path(rel).is_some_and(migration_allowed);
        if !allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{rel} is not a portable artifact"),
            ));
        }
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, rel.to_owned()));
        }
        let digest = sha2_raw(bytes);
        self.sink.write_all(&(rel.len() as u32).to_le_bytes())?;
        self.sink.write_all(rel.as_bytes())?;
        self.sink.write_all(&(bytes.len() as u64).to_le_bytes())?;
        self.sink.write_all(&digest)?;
        self.sink.write_all(bytes)?;
        self.entries += 1;
        Ok(())
    }

    pub fn entries(&self) -> usize {
        self.entries
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.sink.write_all(&0u32.to_le_bytes())?;
        self.sink.finish()
    }
}

fn sha2_raw(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

struct Opener {
    cipher: Aes256Gcm,
    prefix: [u8; 7],
    aad: Vec<u8>,
    counter: u32,
    done: bool,
}

/// Plaintext view of the payload.
enum Source<R: Read> {
    Plain(R),
    Sealed {
        input: R,
        opener: Box<Opener>,
        chunk: Vec<u8>,
        at: usize,
    },
}

impl<R: Read> Source<R> {
    fn read_exact(&mut self, mut out: &mut [u8]) -> Result<(), BundleError> {
        match self {
            Self::Plain(input) => input.read_exact(out).map_err(|error| {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    BundleError::Corrupt("it ends early".into())
                } else {
                    BundleError::Io(error)
                }
            }),
            Self::Sealed {
                input,
                opener,
                chunk,
                at,
            } => {
                while !out.is_empty() {
                    if *at == chunk.len() {
                        if opener.done {
                            return Err(BundleError::Corrupt("it ends early".into()));
                        }
                        open_chunk(input, opener, chunk)?;
                        *at = 0;
                        continue;
                    }
                    let take = (chunk.len() - *at).min(out.len());
                    out[..take].copy_from_slice(&chunk[*at..*at + take]);
                    *at += take;
                    out = &mut out[take..];
                }
                Ok(())
            }
        }
    }

    /// After the end marker: nothing may follow.
    fn expect_end(&mut self) -> Result<(), BundleError> {
        let mut probe = [0u8; 1];
        match self {
            Self::Plain(input) => match input.read(&mut probe)? {
                0 => Ok(()),
                _ => Err(BundleError::Corrupt("data after the end".into())),
            },
            Self::Sealed {
                input,
                opener,
                chunk,
                at,
            } => {
                if *at != chunk.len() || !opener.done {
                    return Err(BundleError::Corrupt("data after the end".into()));
                }
                match input.read(&mut probe)? {
                    0 => Ok(()),
                    _ => Err(BundleError::Corrupt("data after the end".into())),
                }
            }
        }
    }
}

fn open_chunk<R: Read>(
    input: &mut R,
    opener: &mut Opener,
    chunk: &mut Vec<u8>,
) -> Result<(), BundleError> {
    let mut head = [0u8; 5];
    input
        .read_exact(&mut head)
        .map_err(|_| BundleError::Corrupt("it ends early".into()))?;
    let last = match head[0] {
        0 => false,
        1 => true,
        _ => return Err(BundleError::Corrupt("bad chunk".into())),
    };
    let len = u32::from_le_bytes(head[1..5].try_into().expect("4 bytes")) as usize;
    if len > CHUNK {
        return Err(BundleError::Corrupt("bad chunk".into()));
    }
    chunk.zeroize();
    chunk.clear();
    chunk.resize(len, 0);
    input
        .read_exact(chunk)
        .map_err(|_| BundleError::Corrupt("it ends early".into()))?;
    let mut tag = [0u8; TAG];
    input
        .read_exact(&mut tag)
        .map_err(|_| BundleError::Corrupt("it ends early".into()))?;
    let tag = aes_gcm::Tag::try_from(&tag[..]).expect("16-byte tag");
    let first = opener.counter == 0;
    opener
        .cipher
        .decrypt_inout_detached(
            &nonce(&opener.prefix, opener.counter, last),
            &opener.aad,
            (&mut chunk[..]).into(),
            &tag,
        )
        .map_err(|_| {
            if first {
                BundleError::WrongPassphrase
            } else {
                BundleError::Corrupt("a chunk failed authentication".into())
            }
        })?;
    opener.counter += 1;
    opener.done = last;
    Ok(())
}

/// Reads entries from a bundle, verifying each one's hash and class.
pub struct BundleReader<R: Read> {
    source: Source<R>,
    encrypted: bool,
    finished: bool,
}

impl<R: Read> BundleReader<R> {
    pub fn open(mut input: R, passphrase: Option<&str>) -> Result<Self, BundleError> {
        let mut head = [0u8; 11];
        input
            .read_exact(&mut head)
            .map_err(|_| BundleError::NotABundle)?;
        if &head[..8] != MAGIC {
            return Err(BundleError::NotABundle);
        }
        let format = u16::from_le_bytes([head[8], head[9]]);
        if format != FORMAT {
            return Err(BundleError::UnsupportedFormat(format));
        }
        let flags = head[10];
        if flags & !FLAG_ENCRYPTED != 0 {
            return Err(BundleError::UnsupportedFormat(format));
        }
        if flags & FLAG_ENCRYPTED == 0 {
            return Ok(Self {
                source: Source::Plain(input),
                encrypted: false,
                finished: false,
            });
        }
        let mut rest = [0u8; 16 + 7 + 12];
        input
            .read_exact(&mut rest)
            .map_err(|_| BundleError::Corrupt("header ends early".into()))?;
        let passphrase = passphrase
            .filter(|p| !p.is_empty())
            .ok_or(BundleError::PassphraseRequired)?;
        let salt = &rest[..16];
        let prefix: [u8; 7] = rest[16..23].try_into().expect("7 bytes");
        let word = |at: usize| u32::from_le_bytes(rest[at..at + 4].try_into().expect("4 bytes"));
        let params = KdfParams {
            m_kib: word(23),
            t: word(27),
            p: word(31),
        };
        if params.m_kib > KdfParams::MAX_M_KIB || params.t > KdfParams::MAX_T || params.p != 1 {
            return Err(BundleError::Corrupt("unsupported key parameters".into()));
        }
        let mut key = derive_key(passphrase, salt, params)
            .map_err(|_| BundleError::Corrupt("unsupported key parameters".into()))?;
        let cipher = Aes256Gcm::new(&key.into());
        key.zeroize();
        let mut aad = head.to_vec();
        aad.extend_from_slice(&rest);
        Ok(Self {
            source: Source::Sealed {
                input,
                opener: Box::new(Opener {
                    cipher,
                    prefix,
                    aad,
                    counter: 0,
                    done: false,
                }),
                chunk: Vec::new(),
                at: 0,
            },
            encrypted: true,
            finished: false,
        })
    }

    pub fn encrypted(&self) -> bool {
        self.encrypted
    }

    /// The next entry, or `None` at the (verified) end.
    pub fn next_entry(&mut self) -> Result<Option<(String, Vec<u8>)>, BundleError> {
        if self.finished {
            return Ok(None);
        }
        let mut word = [0u8; 4];
        self.source.read_exact(&mut word)?;
        let path_len = u32::from_le_bytes(word) as usize;
        if path_len == 0 {
            self.source.expect_end()?;
            self.finished = true;
            return Ok(None);
        }
        if path_len > 1024 {
            return Err(BundleError::Corrupt("bad entry".into()));
        }
        let mut path = vec![0u8; path_len];
        self.source.read_exact(&mut path)?;
        let path = String::from_utf8(path).map_err(|_| BundleError::Corrupt("bad entry".into()))?;
        if !classify_bundle_path(&path).is_some_and(migration_allowed) {
            return Err(BundleError::Corrupt(format!("unexpected entry {path}")));
        }
        let mut long = [0u8; 8];
        self.source.read_exact(&mut long)?;
        let len = u64::from_le_bytes(long);
        if len > MAX_ENTRY_BYTES {
            return Err(BundleError::Corrupt(format!("{path} is too large")));
        }
        let mut digest = [0u8; 32];
        self.source.read_exact(&mut digest)?;
        let mut bytes = vec![0u8; len as usize];
        self.source.read_exact(&mut bytes)?;
        if sha2_raw(&bytes) != digest {
            return Err(BundleError::Corrupt(format!(
                "{path} does not match its hash"
            )));
        }
        Ok(Some((path, bytes)))
    }
}

/// Hex digest helper re-exported for index files.
pub fn digest_hex(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(passphrase: Option<&str>, entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut writer =
            BundleWriter::create_with(Vec::new(), passphrase, KdfParams::TEST).unwrap();
        for (path, bytes) in entries {
            writer.add(path, bytes).unwrap();
        }
        writer.finish().unwrap()
    }

    fn read_all(
        bytes: &[u8],
        passphrase: Option<&str>,
    ) -> Result<Vec<(String, Vec<u8>)>, BundleError> {
        let mut reader = BundleReader::open(bytes, passphrase)?;
        let mut out = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            out.push(entry);
        }
        Ok(out)
    }

    fn sample() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("bundle.json", b"{}".to_vec()),
            ("profile/notes/a.md", b"# a".to_vec()),
            // Spans several chunks.
            (
                "transcripts/claude/x.jsonl",
                (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8).collect(),
            ),
        ]
    }

    #[test]
    fn plain_and_sealed_round_trip() {
        for passphrase in [None, Some("correct horse")] {
            let bytes = write(passphrase, &sample());
            let entries = read_all(&bytes, passphrase).unwrap();
            assert_eq!(entries.len(), 3);
            for ((path, data), (expected_path, expected)) in entries.iter().zip(sample()) {
                assert_eq!(path, expected_path);
                assert_eq!(data, &expected);
            }
        }
    }

    #[test]
    fn sealed_bundles_hide_their_contents_and_need_the_passphrase() {
        let bytes = write(
            Some("pw"),
            &[("profile/notes/a.md", b"secret plan".to_vec())],
        );
        assert!(!bytes.windows(11).any(|w| w == b"secret plan"));
        assert!(!bytes.windows(8).any(|w| w == b"notes/a."));
        assert!(matches!(
            read_all(&bytes, None),
            Err(BundleError::PassphraseRequired)
        ));
        assert!(matches!(
            read_all(&bytes, Some("nope")),
            Err(BundleError::WrongPassphrase)
        ));
    }

    #[test]
    fn tampering_is_detected() {
        let bytes = write(Some("pw"), &sample());
        // Truncation at a chunk boundary: drop the final chunk.
        let first_chunk = 11 + 35 + 5 + CHUNK + TAG;
        assert!(read_all(&bytes[..first_chunk], Some("pw")).is_err());
        // A flipped bit anywhere in the payload.
        let mut flipped = bytes.clone();
        let at = flipped.len() - 40;
        flipped[at] ^= 1;
        assert!(read_all(&flipped, Some("pw")).is_err());
        // A flipped header byte (the KDF salt) breaks the key.
        let mut header = bytes.clone();
        header[12] ^= 1;
        assert!(read_all(&header, Some("pw")).is_err());
        // Trailing garbage.
        let mut extended = bytes;
        extended.push(0);
        assert!(read_all(&extended, Some("pw")).is_err());
        // Plain: a flipped payload byte fails its hash.
        let mut plain = write(None, &sample());
        let at = plain.len() - 10;
        plain[at] ^= 1;
        assert!(read_all(&plain, None).is_err());
    }

    #[test]
    fn writer_refuses_non_portable_paths() {
        let mut writer = BundleWriter::create(Vec::new(), None).unwrap();
        for path in [
            "accounts.json",
            "profile/hosts.json",
            "profile/../x",
            "claude-logins/a/.credentials.json",
        ] {
            assert!(writer.add(path, b"x").is_err(), "{path}");
        }
    }

    #[test]
    fn not_a_bundle() {
        assert!(matches!(
            BundleReader::open(&b"hello world, nothing here"[..], None).err(),
            Some(BundleError::NotABundle)
        ));
    }
}
