//! Windows observations bracketed by the captured process creation time.
//! POSIX UIDs, process groups, and another process's cwd have no supported
//! equivalent in the existing wire schema; report them as unavailable.
use diri_platform::windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use diri_proto::{
    process::ProcessIdentity,
    process_facts::{ProcessAccount, ProcessFacts, ProcessValue, UnavailableReason},
};
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};

pub fn inspect(
    identity: &ProcessIdentity,
    _lookup: impl FnOnce(u32) -> ProcessValue<ProcessAccount>,
) -> io::Result<ProcessFacts> {
    crate::process_identity::inspect_verified(identity, || {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, identity.pid()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let executable = if unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut length)
        } == 0
        {
            ProcessValue::unavailable(UnavailableReason::Io)
        } else {
            match String::from_utf16(&buffer[..length as usize]) {
                Ok(value) => ProcessValue::available(value),
                Err(_) => ProcessValue::unavailable(UnavailableReason::InvalidData),
            }
        };
        Ok(ProcessFacts {
            identity: *identity,
            executable,
            process_group: ProcessValue::unavailable(UnavailableReason::Unsupported),
            foreground_process_group: ProcessValue::unavailable(UnavailableReason::Unsupported),
            working_directory: ProcessValue::unavailable(UnavailableReason::Unsupported),
            user_ids: ProcessValue::unavailable(UnavailableReason::Unsupported),
            account: ProcessValue::unavailable(UnavailableReason::Unsupported),
        })
    })
}
