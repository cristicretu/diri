use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::os::windows::io::{
    AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, IntoRawSocket, RawSocket,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn endpoint(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let sid = crate::security::current_user_sid()?;
    let owner = hex_digest(sid.as_bytes());
    // LocalAppData/Temp often exceeds sun_path after a normal long username.
    // Keep the protected namespace immediately below the user's profile.
    let root = crate::home_dir()
        .ok_or_else(|| io::Error::other("Windows profile directory is unavailable"))?
        .join(format!(".diri-ipc-{}", &owner[..8]));
    crate::security::private_dir_all(&root)?;
    // Canonicalize the existing parent, since the socket itself need not exist.
    let absolute = absolute
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| absolute.file_name().map(|n| p.join(n)))
        .unwrap_or(absolute);
    let key = hex_digest(absolute.as_os_str().as_encoded_bytes());
    let endpoint = root.join(format!("{}.sock", &key[..32]));
    if endpoint.as_os_str().as_encoded_bytes().len() >= 108 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows profile path is too long for a private AF_UNIX endpoint",
        ));
    }
    Ok(endpoint)
}

#[derive(Debug)]
pub struct UnixStream(pub(super) uds_windows::UnixStream);

impl UnixStream {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::connect_timeout(path, std::time::Duration::from_secs(2))
    }
    pub fn connect_timeout(
        path: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> io::Result<Self> {
        let address = socket2::SockAddr::unix(endpoint(path.as_ref())?)?;
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.connect_timeout(&address, timeout)?;
        // SAFETY: transfer the one connected AF_UNIX socket without duplicating ownership.
        Ok(Self(unsafe {
            uds_windows::UnixStream::from_raw_socket(socket.into_raw_socket())
        }))
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
    pub fn pair() -> io::Result<(Self, Self)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let virtual_path = std::env::temp_dir().join(format!(
            "diri-pair-{}-{}-{nonce}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = endpoint(&virtual_path)?;
        // Pair endpoints are private to this invocation. They need no persistent
        // recovery lock or logical discovery marker, unlike named listeners.
        struct TemporaryListener {
            inner: Option<uds_windows::UnixListener>,
            path: PathBuf,
        }
        impl Drop for TemporaryListener {
            fn drop(&mut self) {
                drop(self.inner.take());
                let _ = std::fs::remove_file(&self.path);
            }
        }
        let inner = uds_windows::UnixListener::bind(&path)?;
        let listener = TemporaryListener {
            inner: Some(inner),
            path,
        };
        crate::security::owner_only(&listener.path, false)?;
        let client = Self::connect(&virtual_path)?;
        let server = Self(
            listener
                .inner
                .as_ref()
                .expect("live pair listener")
                .accept()?
                .0,
        );
        drop(listener);
        Ok((client, server))
    }
}
impl std::ops::Deref for UnixStream {
    type Target = uds_windows::UnixStream;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Read for UnixStream {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for UnixStream {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Read for &UnixStream {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(b)
    }
}
impl Write for &UnixStream {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        (&self.0).write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}
impl AsRawSocket for UnixStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.0.as_raw_socket()
    }
}
impl IntoRawSocket for UnixStream {
    fn into_raw_socket(self) -> RawSocket {
        self.0.into_raw_socket()
    }
}

#[derive(Debug)]
pub struct UnixListener {
    inner: Option<uds_windows::UnixListener>,
    physical: PathBuf,
    logical: PathBuf,
    _lock: std::fs::File,
}
impl UnixListener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let logical = std::path::absolute(path.as_ref())?;
        let path = endpoint(&logical)?;
        let lock = crate::security::open_private_rw(&path.with_extension("lock"))?;
        lock.try_lock()
            .map_err(|_| io::Error::from(io::ErrorKind::AddrInUse))?;
        if uds_windows::UnixStream::connect(&path).is_ok() {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = uds_windows::UnixListener::bind(&path)?;
        crate::security::owner_only(&path, false)?;
        // Existing discovery/recovery code enumerates logical .sock names.
        // The marker carries no address or credential; connect derives its
        // physical endpoint independently from the SID and logical path.
        if let Some(parent) = logical.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let marker = crate::security::open_private_rw(&logical)?;
        marker.set_len(0)?;
        Ok(Self {
            inner: Some(listener),
            physical: path,
            logical,
            _lock: lock,
        })
    }
    pub fn accept(&self) -> io::Result<(UnixStream, uds_windows::SocketAddr)> {
        self.inner
            .as_ref()
            .expect("live listener")
            .accept()
            .map(|(s, a)| (UnixStream(s), a))
    }
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<UnixStream>> + '_ {
        std::iter::repeat_with(|| self.accept().map(|(s, _)| s))
    }
    pub fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        self.inner
            .as_ref()
            .expect("live listener")
            .set_nonblocking(enabled)
    }
}
impl AsRawSocket for UnixListener {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_ref().expect("live listener").as_raw_socket()
    }
}
impl Drop for UnixListener {
    fn drop(&mut self) {
        drop(self.inner.take());
        let _ = std::fs::remove_file(&self.physical);
        let _ = std::fs::remove_file(&self.logical);
        // The bind lock is released only after both names are removed.
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl AsSocket for UnixStream {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        unsafe { BorrowedSocket::borrow_raw(self.as_raw_socket()) }
    }
}
