//! Identity-safe freeze/thaw of one privately owned Windows Job.
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{JobObjects::*, Threading::*},
};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process: HANDLE) -> i32;
    fn NtResumeProcess(process: HANDLE) -> i32;
}

#[derive(Default)]
pub struct FrozenTree {
    processes: Vec<(u32, OwnedHandle)>,
    complete: bool,
}
impl FrozenTree {
    pub fn freeze(&mut self, job: &OwnedHandle) -> io::Result<()> {
        if self.complete {
            return Ok(());
        }
        if !self.processes.is_empty() {
            self.thaw()?;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        let result = (|| {
            loop {
                let mut added = false;
                // The list belongs to this Job, not a guessed PID ancestry.
                // Once all producers are suspended a second empty pass proves
                // no unsuspended descendant escaped the first enumeration.
                for pid in members(job)? {
                    if self.processes.iter().any(|(old, _)| *old == pid) {
                        continue;
                    }
                    let raw = unsafe {
                        OpenProcess(
                            PROCESS_SUSPEND_RESUME
                                | PROCESS_QUERY_LIMITED_INFORMATION
                                | PROCESS_SYNCHRONIZE,
                            0,
                            pid,
                        )
                    };
                    if raw.is_null() {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                            continue;
                        }
                        return Err(e);
                    }
                    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
                    let mut in_job = 0;
                    if unsafe {
                        IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut in_job)
                    } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if in_job == 0 {
                        continue;
                    } // PID reused outside this session.
                    let status = unsafe { NtSuspendProcess(process.as_raw_handle()) };
                    if status < 0 {
                        if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }
                            == WAIT_OBJECT_0
                        {
                            continue;
                        }
                        return Err(io::Error::other(format!(
                            "process suspend failed: NTSTATUS {status:#x}"
                        )));
                    }
                    self.processes.push((pid, process));
                    added = true;
                }
                if !added {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "session tree did not quiesce",
                    ));
                }
            }
        })();
        self.complete = result.is_ok();
        if result.is_err() {
            let _ = self.thaw();
        }
        result
    }
    pub fn thaw(&mut self) -> io::Result<()> {
        let mut error = None;
        // Resume only handles whose suspension this owner actually acquired.
        self.processes.retain(|(_, process)| {
            let status = unsafe { NtResumeProcess(process.as_raw_handle()) };
            if status >= 0
                || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0
            {
                return false;
            }
            error = Some(io::Error::other(format!(
                "process resume failed: NTSTATUS {status:#x}"
            )));
            true
        });
        if self.processes.is_empty() {
            self.complete = false;
        }
        error.map_or(Ok(()), Err)
    }
}

fn members(job: &OwnedHandle) -> io::Result<Vec<u32>> {
    // Fixed upper bound prevents a runaway process tree from forcing unbounded
    // allocation. Failure rolls back suspension; it never reports a partial freeze.
    let mut data = vec![0usize; 4098];
    let bytes = (data.len() * size_of::<usize>()) as u32;
    if unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicProcessIdList,
            data.as_mut_ptr().cast(),
            bytes,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let list = unsafe { &*data.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
    if list.NumberOfAssignedProcesses != list.NumberOfProcessIdsInList
        || list.NumberOfProcessIdsInList > 4096
    {
        return Err(io::Error::other("session Job process list exceeds bound"));
    }
    let pids = unsafe {
        std::slice::from_raw_parts(
            list.ProcessIdList.as_ptr(),
            list.NumberOfProcessIdsInList as usize,
        )
    };
    pids.iter()
        .map(|pid| u32::try_from(*pid).map_err(io::Error::other))
        .collect()
}
