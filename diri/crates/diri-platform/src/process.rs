//! Bounded native Windows process accounting, off the PTY hot path.
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{ProcessStatus::*, SystemInformation::*, Threading::*},
};

pub fn usage(pid: u32) -> io::Result<(u64, u64)> {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    if unsafe {
        GetProcessMemoryInfo(
            process.as_raw_handle(),
            (&raw mut counters).cast(),
            counters.cb,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let ticks = |t: FILETIME| u64::from(t.dwLowDateTime) | (u64::from(t.dwHighDateTime) << 32);
    Ok((
        counters.PrivateUsage as u64,
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    ))
}
/// Page faults taken by this process, including soft faults.
pub fn page_faults() -> io::Result<u64> {
    // SAFETY: the current-process pseudo handle stays valid and the buffer
    // carries the exact SDK structure size written by GetProcessMemoryInfo.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(counters.PageFaultCount))
}

pub fn physical_memory() -> io::Result<u64> {
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status.ullTotalPhys)
}

/// Return TCP listeners for the requested tree, including IPv6. A failed or
/// oversized table is unknown, never an empty result that permits freezing.
pub fn listeners(pids: &[i32]) -> io::Result<Vec<(u16, u32)>> {
    use windows_sys::Win32::{
        Foundation::ERROR_INSUFFICIENT_BUFFER,
        NetworkManagement::IpHelper::*,
        Networking::WinSock::{AF_INET, AF_INET6},
    };
    let mut result = Vec::new();
    for family in [AF_INET, AF_INET6] {
        let mut length = 0;
        let mut storage = Vec::<u32>::new();
        let mut ready = false;
        for _ in 0..4 {
            let status = unsafe {
                GetExtendedTcpTable(
                    if storage.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        storage.as_mut_ptr().cast()
                    },
                    &mut length,
                    0,
                    family as u32,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if status == 0 {
                ready = true;
                break;
            }
            if status != ERROR_INSUFFICIENT_BUFFER {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            if length > 16 * 1024 * 1024 {
                return Err(io::Error::other("TCP table exceeds limit"));
            }
            storage.resize((length as usize).div_ceil(4), 0);
        }
        if !ready || storage.is_empty() {
            return Err(io::Error::other("TCP table changed during enumeration"));
        }
        let row_size = if family == AF_INET {
            size_of::<MIB_TCPROW_OWNER_PID>()
        } else {
            size_of::<MIB_TCP6ROW_OWNER_PID>()
        };
        let count = storage[0] as usize;
        if count > (length as usize).saturating_sub(4) / row_size {
            return Err(io::Error::other("invalid TCP table length"));
        }
        for index in 0..count {
            let row = unsafe { storage.as_ptr().cast::<u8>().add(4 + index * row_size) };
            let (port, pid) = if family == AF_INET {
                let row = unsafe { row.cast::<MIB_TCPROW_OWNER_PID>().read_unaligned() };
                (row.dwLocalPort, row.dwOwningPid)
            } else {
                let row = unsafe { row.cast::<MIB_TCP6ROW_OWNER_PID>().read_unaligned() };
                (row.dwLocalPort, row.dwOwningPid)
            };
            if pids.contains(&(pid as i32)) {
                result.push((u16::from_be(port as u16), pid));
            }
        }
    }
    result.sort_unstable();
    result.dedup();
    Ok(result)
}
