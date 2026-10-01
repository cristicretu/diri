//! Native ConPTY with one kill-on-close Job per child tree.
//!
//! ConPTY requires synchronous pipes. Two narrow I/O adapters expose a pollable
//! byte stream; they own no parser or product state. The output adapter keeps
//! draining during ClosePseudoConsole, including after its consumer disappears.
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle, RawSocket},
};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::{Exit, PtySpec};
use diri_platform::ipc::UnixStream;
use diri_platform::windows_sys::Win32::{
    Foundation::*,
    System::{Console::*, JobObjects::*, Pipes::CreatePipe, Threading::*},
};

pub const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);
pub const REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

struct Console(HPCON);
impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: this object is the sole owner; the output drainer is still alive.
        unsafe {
            ClosePseudoConsole(self.0);
        }
    }
}

pub struct Pty {
    console: Option<Console>,
    job: OwnedHandle,
    process: OwnedHandle,
    pid: u32,
    identity: Option<diri_proto::process::ProcessIdentity>,
    stream: UnixStream,
    dimensions: AtomicU32,
    frozen: std::sync::Mutex<diri_platform::job::FrozenTree>,
}

impl Pty {
    pub fn spawn(spec: &PtySpec) -> io::Result<Self> {
        let size = dimensions(spec.cols, spec.rows)?;
        let argv = diri_platform::launch::resolve_argv(&spec.argv, &spec.env, &spec.cwd)?;
        let application = wide(OsStr::new(&argv[0]))?;
        let mut command_line = command_line(&argv)?;
        let cwd = wide(spec.cwd.as_os_str())?;
        let environment = environment_block(&spec.env)?;
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        let (stream, adapter) = UnixStream::pair()?;
        let mut output_sink = adapter.try_clone()?;
        let mut output = File::from(output_read);
        std::thread::Builder::new()
            .name("conpty-output".into())
            .spawn(move || {
                let mut buffer = [0u8; 16384];
                let mut connected = true;
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if connected && output_sink.write_all(&buffer[..n]).is_err() {
                                connected = false;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = output_sink.shutdown(Shutdown::Write);
            })?;
        let mut handle = 0;
        // SAFETY: both pipe handles and the initialized output pointer remain live.
        hresult(unsafe {
            CreatePseudoConsole(
                size,
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut handle,
            )
        })?;
        let console = Console(handle);
        drop(input_read);
        drop(output_write);
        let mut input = File::from(input_write);
        let mut input_source = adapter;
        std::thread::Builder::new()
            .name("conpty-input".into())
            .spawn(move || {
                let _ = io::copy(&mut input_source, &mut input);
            })?;
        let job = job()?;
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // Without STARTF_USESTDHANDLES a console child copies the parent's
        // std handles, so a redirected parent (the detached Holder has NUL
        // stdio) leaves the child reading EOF instead of the pseudoconsole.
        // Null handles are inherited as-is; INVALID_HANDLE_VALUE makes the
        // child open its console, which the attribute below makes ConPTY.
        // bInheritHandles=false still prevents unrelated parent inheritance.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        let attributes = Attributes::new(console.0)?;
        startup.lpAttributeList = attributes.pointer();
        let mut child: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: all UTF-16 inputs are terminated; the attribute list and env
        // outlive CreateProcessW. Spawn suspended so no descendant can escape
        // before Job admission. No parent handles are inherited.
        check(unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                environment.as_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut child,
            )
        })?;
        let process = unsafe { OwnedHandle::from_raw_handle(child.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(child.hThread) };
        if let Err(error) =
            check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) })
        {
            // An unadmitted suspended process has never run user code.
            unsafe {
                TerminateProcess(process.as_raw_handle(), 1);
            }
            return Err(error);
        }
        let identity = crate::process_identity::observe(child.dwProcessId).ok();
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            console: Some(console),
            job,
            process,
            pid: child.dwProcessId,
            identity,
            stream,
            frozen: Default::default(),
            dimensions: AtomicU32::new((u32::from(spec.rows) << 16) | u32::from(spec.cols)),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn child_identity(&self) -> Option<diri_proto::process::ProcessIdentity> {
        self.identity
    }
    /// ConPTY has no POSIX foreground process group.
    pub fn foreground_pgid(&self) -> Option<i32> {
        None
    }
    /// Win32 has no termios canonical/echo projection.
    pub fn secret_input(&self) -> bool {
        false
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        hresult(unsafe {
            ResizePseudoConsole(
                self.console
                    .as_ref()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
                    .0,
                dimensions(cols, rows)?,
            )
        })?;
        self.dimensions
            .store((u32::from(rows) << 16) | u32::from(cols), Ordering::Relaxed);
        Ok(())
    }
    pub fn size(&self) -> io::Result<(u16, u16)> {
        let size = self.dimensions.load(Ordering::Relaxed);
        Ok((size as u16, (size >> 16) as u16))
    }
    pub fn reader(&self) -> io::Result<PtyStream> {
        self.stream.try_clone().map(PtyStream)
    }
    pub fn writer(&self) -> io::Result<PtyStream> {
        self.stream.try_clone().map(PtyStream)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
                Ok(Some(Exit::Code(code as i32)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn wait(&mut self) -> io::Result<Exit> {
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.try_wait()?
            .ok_or_else(|| io::Error::other("process wait completed without exit"))
    }
    pub fn kill_group(&self, signal: i32) -> io::Result<()> {
        match signal {
            2 => (&self.stream).write_all(&[3]),
            19 => self
                .frozen
                .lock()
                .map_err(|_| io::Error::other("Job freeze lock poisoned"))?
                .freeze(&self.job),
            18 => self
                .frozen
                .lock()
                .map_err(|_| io::Error::other("Job freeze lock poisoned"))?
                .thaw(),
            9 | 15 => check(unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) }),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this POSIX signal has no Windows Job operation",
            )),
        }
    }
    pub fn terminate(&mut self, _grace: Duration) -> io::Result<Exit> {
        self.kill_group(9)?;
        self.wait()
    }
    /// Close ConPTY while its final output is still being consumed by the owner.
    /// ClosePseudoConsole can wait for output-pipe space, so never call it on
    /// the draining owner thread before EOF.
    pub fn begin_close(&mut self) -> io::Result<()> {
        let Some(console) = self.console.take() else {
            return Ok(());
        };
        let pending = std::sync::Arc::new(std::sync::Mutex::new(Some(console)));
        let worker = std::sync::Arc::clone(&pending);
        if let Err(error) = std::thread::Builder::new()
            .name("conpty-close".into())
            .spawn(move || {
                drop(worker.lock().expect("ConPTY close").take());
            })
        {
            self.console = pending.lock().expect("ConPTY close").take();
            return Err(error);
        }
        Ok(())
    }
    pub fn wait_draining(&mut self, timeout: Duration) -> io::Result<Option<Exit>> {
        let deadline = Instant::now() + timeout;
        let mut reader = self.reader()?;
        reader.set_nonblocking(true)?;
        let mut buffer = [0u8; 16384];
        loop {
            while matches!(reader.read(&mut buffer), Ok(n) if n > 0) {}
            if let Some(exit) = self.try_wait()? {
                return Ok(Some(exit));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let _ = reader.wait_readable(REAP_POLL_INTERVAL)?;
        }
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        // Terminating the Job is scoped to this session, never a PID-only kill.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
        // The Console field drops next, while the output adapter drains/discards.
    }
}

pub struct PtyStream(UnixStream);
impl PtyStream {
    pub fn set_nonblocking(&self, value: bool) -> io::Result<()> {
        self.0.set_nonblocking(value)
    }
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        diri_platform::ipc::wait(&self.0, false, Some(timeout))
    }
}
impl AsRawSocket for PtyStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.0.as_raw_socket()
    }
}
impl Read for PtyStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Write for PtyStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub struct ExitWatcher(OwnedHandle);
impl ExitWatcher {
    pub fn new(pid: u32) -> io::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let result = unsafe {
            WaitForSingleObject(
                self.0.as_raw_handle(),
                timeout.map_or(INFINITE, |v| {
                    v.as_millis().min(u128::from(u32::MAX - 1)) as u32
                }),
            )
        };
        match result {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

fn dimensions(cols: u16, rows: u16) -> io::Result<COORD> {
    if cols == 0 || rows == 0 || cols > i16::MAX as u16 || rows > i16::MAX as u16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid ConPTY dimensions",
        ));
    }
    Ok(COORD {
        X: cols as i16,
        Y: rows as i16,
    })
}
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    check(unsafe { CreatePipe(&mut read, &mut write, null(), 0) })?;
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}
fn job() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateJobObjectW(null(), null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(unsafe {
        SetInformationJobObject(
            handle.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    })?;
    Ok(handle)
}
fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn hresult(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::other(format!(
            "ConPTY HRESULT 0x{:08x}",
            result as u32
        )))
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in process argument",
        ));
    }
    value.push(0);
    Ok(value)
}
/// CRT argv quoting, never cmd.exe shell syntax. Batch shims are resolved to
/// their real interpreter/script by the launch seam before reaching here.
fn command_line(argv: &[String]) -> io::Result<Vec<u16>> {
    let mut line = String::new();
    for argument in argv {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push('"');
        let mut slashes = 0;
        for c in argument.chars() {
            if c == '\\' {
                slashes += 1;
                continue;
            }
            line.extend(std::iter::repeat_n(
                '\\',
                if c == '"' { 2 * slashes + 1 } else { slashes },
            ));
            line.push(c);
            slashes = 0;
        }
        line.extend(std::iter::repeat_n('\\', slashes * 2));
        line.push('"');
    }
    let result = wide(OsStr::new(&line))?;
    if result.len() > 32767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows command line exceeds 32767 UTF-16 units",
        ));
    }
    Ok(result)
}
fn environment_block(env: &[(String, String)]) -> io::Result<Vec<u16>> {
    let mut entries = std::collections::BTreeMap::new();
    for (name, value) in env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment entry",
            ));
        }
        entries.insert(name.to_uppercase(), (name, value));
    }
    let mut block = Vec::new();
    for (_, (name, value)) in entries {
        block.extend(wide(OsStr::new(&format!("{name}={value}")))?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}
struct Attributes(Vec<usize>);
impl Attributes {
    fn pointer(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_ptr() as *mut c_void
    }
    fn new(console: HPCON) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        check(unsafe {
            InitializeProcThreadAttributeList(data.as_mut_ptr().cast(), 1, 0, &mut bytes)
        })?;
        let attributes = Self(data);
        check(unsafe {
            UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        })?;
        Ok(attributes)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.pointer());
        }
    }
}
