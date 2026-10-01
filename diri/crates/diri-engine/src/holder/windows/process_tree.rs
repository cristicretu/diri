//! Read-only Toolhelp process relations for the Engine governor. Mutations
//! belong to the Holder's owned Job and never use these numeric PID samples.
use super::protocol::HolderProcessSample;
use diri_platform::windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE, System::Diagnostics::ToolHelp::*,
};
use std::collections::HashSet;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

pub struct ProcessTable(Vec<(u32, u32)>);
impl ProcessTable {
    pub fn capture() -> Self {
        let mut entries = Vec::new();
        // SAFETY: a snapshot owns its buffer; PROCESSENTRY32W carries its size.
        unsafe {
            let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if handle == INVALID_HANDLE_VALUE {
                return Self(entries);
            }
            let handle = OwnedHandle::from_raw_handle(handle);
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut next = Process32FirstW(handle.as_raw_handle(), &mut entry);
            while next != 0 {
                entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
                next = Process32NextW(handle.as_raw_handle(), &mut entry);
            }
        }
        Self(entries)
    }
}
/// Read-only child observation; process mutation remains Job-owned.
pub fn has_children(pid: i32) -> bool {
    pid > 1
        && ProcessTable::capture()
            .0
            .iter()
            .any(|(_, parent)| *parent == pid as u32)
}

pub fn enumerate(root: i32) -> Vec<HolderProcessSample> {
    enumerate_in(&ProcessTable::capture(), root)
}
pub fn enumerate_in(table: &ProcessTable, root: i32) -> Vec<HolderProcessSample> {
    if root <= 0 {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut pending = vec![root as u32];
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        pending.extend(
            table
                .0
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
    }
    seen.into_iter()
        .filter_map(|pid| {
            let identity = diri_pty::process_identity::observe(pid).ok()?;
            let diri_proto::process::ProcessBirth::Windows { creation_filetime } = identity.birth()
            else {
                return None;
            };
            // Historical field spelling; Windows stores exact native FILETIME ticks,
            // never rounded seconds. This field is read-only governor bookkeeping.
            Some(HolderProcessSample {
                pid: pid as i32,
                start_sec: creation_filetime as i64,
            })
        })
        .collect()
}
