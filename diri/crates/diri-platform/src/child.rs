//! Ownership of short-lived transport/tool trees. Detached session Holders use
//! their separate launcher; dropping this owner must never kill a Holder.
#[cfg(unix)]
pub use std::process::Child;
#[cfg(unix)]
pub fn spawn(command: &mut std::process::Command) -> std::io::Result<Child> {
    command.spawn()
}

#[cfg(windows)]
pub struct Child {
    inner: std::process::Child,
    job: std::os::windows::io::OwnedHandle,
}
#[cfg(windows)]
impl std::ops::Deref for Child {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
#[cfg(windows)]
impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
#[cfg(windows)]
impl Child {
    pub fn kill(&mut self) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1)
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
#[cfg(windows)]
pub fn spawn(command: &mut std::process::Command) -> std::io::Result<Child> {
    use std::{
        io,
        os::windows::{
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        },
    };
    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::{JobObjects::*, Threading::*},
    };
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: HANDLE) -> i32;
    }
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut inner = command.spawn()?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), inner.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        let _ = inner.kill();
        let _ = inner.wait();
        return Err(error);
    }
    let status = unsafe { NtResumeProcess(inner.as_raw_handle()) };
    if status < 0 {
        unsafe {
            TerminateJobObject(job.as_raw_handle(), 1);
        }
        let _ = inner.wait();
        return Err(io::Error::other(format!(
            "transport resume failed: NTSTATUS {status:#x}"
        )));
    }
    Ok(Child { inner, job })
}

/// Bounded collection for maintenance commands. Drains both pipes while the
/// child runs; the Job owner closes before readers join, including on timeout.
#[cfg(windows)]
pub fn output(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    limit: usize,
) -> std::io::Result<std::process::Output> {
    use std::{
        io::{self, Read},
        process::Stdio,
        time::Instant,
    };
    fn reader(
        mut pipe: impl Read + Send + 'static,
        limit: usize,
    ) -> std::thread::JoinHandle<io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut result = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let n = pipe.read(&mut buffer)?;
                if n == 0 {
                    return Ok(result);
                }
                let retain = n.min(limit.saturating_sub(result.len()));
                result.extend_from_slice(&buffer[..retain]);
            }
        })
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn(command)?;
    let stdout = reader(child.stdout.take().expect("piped stdout"), limit);
    let stderr = reader(child.stderr.take().expect("piped stderr"), 64 * 1024);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error);
            }
        }
    };
    drop(child);
    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("stdout reader failed"))??;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("stderr reader failed"))??;
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}
