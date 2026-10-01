use std::ffi::OsStr;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    fs::MetadataExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    value.push(0);
    Ok(value)
}

struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: each output is initialized, sized by the API, and owned until all
    // pointers into it are consumed. Token and LocalAlloc storage have RAII owners.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut length = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length);
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _text = Local(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}

fn descriptor(directory: bool) -> io::Result<Local> {
    let flags = if directory { "OICI" } else { "" };
    let sid = current_user_sid()?;
    // An administrator token can default new objects to the Administrators
    // group. Specify the individual owner as well as the private DACL so our
    // own subsequent opens pass the same strict ownership check as other opens.
    let sddl = wide(OsStr::new(&format!("O:{sid}D:P(A;{flags};FA;;;{sid})")))?;
    let mut descriptor = null_mut();
    // SAFETY: NUL-terminated SDDL input; API allocates descriptor freed by Local.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(descriptor))
}

pub fn owner_only(path: &Path, directory: bool) -> io::Result<()> {
    let path = wide(path.as_os_str())?;
    let security = descriptor(directory)?;
    // SAFETY: security is a valid descriptor for this whole call. Protecting the
    // DACL stops inherited broad profile ACLs from granting another user access.
    if unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security.0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn private_dir_all(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory must be absolute",
        ));
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other(
                "private directory is not an ordinary directory",
            ));
        }
        let directory = open_directory(path)?;
        validate_owner(&directory, false)?;
        return owner_only(path, true);
    }
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        private_dir_all(parent)?;
    }
    let name = wide(path.as_os_str())?;
    let descriptor = descriptor(true)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: both pointers are valid for the synchronous call; descriptor is
    // supplied at creation, so there is no permissive-then-chmod window.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "private directory changed during creation",
        ));
    }
    validate_owner(&open_directory(path)?, false)?;
    owner_only(path, true)
}

pub(super) fn open_file(
    path: &Path,
    write: bool,
    create: bool,
    exclusive: bool,
) -> io::Result<std::fs::File> {
    let name = wide(path.as_os_str())?;
    let descriptor = descriptor(false)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: descriptor applies atomically at file creation; reparse points
    // are opened themselves and rejected below, never followed to another file.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | if write { GENERIC_WRITE } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            if exclusive {
                CREATE_NEW
            } else if create {
                OPEN_ALWAYS
            } else {
                OPEN_EXISTING
            },
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_handle(raw) };
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("file is a reparse point"));
    }
    Ok(file)
}

fn open_directory(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_dir() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("directory is a reparse point"));
    }
    Ok(file)
}
pub(super) fn validate_directory(path: &Path) -> io::Result<()> {
    validate_owner(&open_directory(path)?, true)
}
pub(super) fn validate_file(file: &std::fs::File, private: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::other("file is not a single-link ordinary file"));
    }
    validate_owner(file, private)
}

fn validate_owner(file: &std::fs::File, private: bool) -> io::Result<()> {
    let user = wide(OsStr::new(&current_user_sid()?))?;
    // SAFETY: all pointers are returned into a LocalAlloc-owned descriptor and
    // are inspected before it is freed. ACEs are returned by the ACL API.
    unsafe {
        let mut sid = null_mut();
        if ConvertStringSidToSidW(user.as_ptr(), &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _sid = Local(sid);
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let status = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor = Local(descriptor);
        if owner.is_null() || EqualSid(owner, sid) == 0 {
            return Err(io::Error::other("object is not owned by the current user"));
        }
        if private {
            if dacl.is_null() {
                return Err(io::Error::other("object has an unrestricted DACL"));
            }
            for index in 0..(*dacl).AceCount {
                let mut ace = null_mut();
                if GetAce(dacl, u32::from(index), &mut ace) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let header = &*ace.cast::<ACE_HEADER>();
                if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                    continue;
                }
                if header.AceType == ACCESS_DENIED_ACE_TYPE as u8 {
                    continue;
                }
                if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8 {
                    return Err(io::Error::other("unsupported private-file ACL entry"));
                }
                let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                let trustee = (&raw const allowed.SidStart).cast_mut().cast();
                if EqualSid(trustee, sid) == 0 {
                    return Err(io::Error::other("object grants another identity access"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn private_objects_remain_owned_by_the_individual_user_on_reopen() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("diri-owner-{}-{nonce}", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        super::private_dir_all(&path).unwrap();
        super::validate_directory(&path).unwrap();
        super::private_dir_all(&path).unwrap();
        let file = path.join("private");
        drop(crate::security::create_private(&file).unwrap());
        drop(crate::security::read_owned(&file, true).unwrap());
        drop(crate::security::open_private_rw(&file).unwrap());
        let created_on_open = path.join("created-on-open");
        drop(crate::security::open_private_rw(&created_on_open).unwrap());
        drop(crate::security::read_owned(&created_on_open, true).unwrap());
    }
}
