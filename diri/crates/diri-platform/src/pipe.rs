//! Pollable subprocess pipes. Unix keeps the original fd; Windows adapts the
//! synchronous std::process pipe into a bounded AF_UNIX stream.
use std::io;
#[cfg(unix)]
pub use std::process::{ChildStdin, ChildStdout};
#[cfg(unix)]
pub fn input(value: std::process::ChildStdin) -> io::Result<ChildStdin> {
    Ok(value)
}
#[cfg(unix)]
pub fn output(value: std::process::ChildStdout) -> io::Result<ChildStdout> {
    Ok(value)
}

#[cfg(windows)]
pub type ChildStdin = crate::ipc::UnixStream;
#[cfg(windows)]
pub type ChildStdout = crate::ipc::UnixStream;
#[cfg(windows)]
pub fn input(mut value: std::process::ChildStdin) -> io::Result<ChildStdin> {
    let (caller, mut worker) = crate::ipc::UnixStream::pair()?;
    std::thread::Builder::new()
        .name("process-input".into())
        .spawn(move || {
            let _ = io::copy(&mut worker, &mut value);
            let _ = worker.shutdown(std::net::Shutdown::Both);
        })?;
    Ok(caller)
}
#[cfg(windows)]
pub fn output(mut value: std::process::ChildStdout) -> io::Result<ChildStdout> {
    let (caller, mut worker) = crate::ipc::UnixStream::pair()?;
    std::thread::Builder::new()
        .name("process-output".into())
        .spawn(move || {
            let _ = io::copy(&mut value, &mut worker);
            let _ = worker.shutdown(std::net::Shutdown::Both);
        })?;
    Ok(caller)
}
