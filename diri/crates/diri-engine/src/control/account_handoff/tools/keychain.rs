//! Claude stores MCP and model credentials in one macOS Keychain item. Merge
//! only MCP fields, in process; secrets never enter command arguments or logs.
use super::*;
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::ptr;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecKeychainFindGenericPassword(
        keychain: *const c_void,
        service_len: u32,
        service: *const u8,
        account_len: u32,
        account: *const u8,
        password_len: *mut u32,
        password: *mut *mut c_void,
        item: *mut *mut c_void,
    ) -> i32;
    fn SecKeychainItemFreeContent(attributes: *mut c_void, data: *mut c_void) -> i32;
    fn SecKeychainItemModifyAttributesAndData(
        item: *mut c_void,
        attributes: *const c_void,
        length: u32,
        data: *const c_void,
    ) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}

struct ItemRef(*mut c_void);
impl Drop for ItemRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}
struct Credential {
    bytes: Vec<u8>,
    item: ItemRef,
}
impl Drop for Credential {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            unsafe { ptr::write_volatile(byte, 0) };
        }
    }
}
fn unavailable() -> ControlError {
    ControlError::bad_request(
        "Cannot preserve Claude MCP authorization in Keychain. Unlock the login keychain and retry.",
    )
}
fn service(root: Option<&Path>) -> String {
    root.map_or_else(
        || "Claude Code-credentials".into(),
        |root| {
            let digest = Sha256::digest(root.to_string_lossy().as_bytes());
            format!(
                "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
                digest[0], digest[1], digest[2], digest[3]
            )
        },
    )
}
fn read(service: &str) -> Result<Option<Credential>, ControlError> {
    let mut length = 0;
    let mut data = ptr::null_mut();
    let mut item = ptr::null_mut();
    let status = unsafe {
        SecKeychainFindGenericPassword(
            ptr::null(),
            service.len() as u32,
            service.as_ptr(),
            0,
            ptr::null(),
            &mut length,
            &mut data,
            &mut item,
        )
    };
    let item = ItemRef(item);
    if status == -25300 {
        return Ok(None);
    }
    if status != 0 {
        return Err(unavailable());
    }
    let bytes = if length as usize <= MAX_SETTINGS && (length == 0 || !data.is_null()) {
        Some(if length == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize) }.to_vec()
        })
    } else {
        None
    };
    if !data.is_null() {
        unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) };
    }
    Ok(Some(Credential {
        bytes: bytes.ok_or_else(unavailable)?,
        item,
    }))
}

struct Patch {
    service: String,
    expected: Vec<u8>,
    bytes: Vec<u8>,
}
fn patches(handoffs: &[PreparedHandoff]) -> Result<BTreeMap<String, Patch>, ControlError> {
    let mut patches: BTreeMap<String, Patch> = BTreeMap::new();
    let mut visited = std::collections::HashSet::new();
    let mut grants = BTreeMap::new();
    for handoff in handoffs {
        if handoff.source.kind != AgentKind::CLAUDE_CODE
            || handoff.source.host.is_some()
            || handoff.source_location.root == handoff.target_location.root
        {
            continue;
        }
        let source_service = service(
            handoff
                .source
                .account_profile
                .as_ref()
                .map(|_| handoff.source_location.root.as_path()),
        );
        if !visited.insert(source_service.clone()) {
            continue;
        }
        let Some(source) = read(&source_service)? else {
            continue;
        };
        let fields = object(&source.bytes)?;
        let Some(oauth) = fields.get("mcpOAuth") else {
            continue;
        };
        for (name, grant) in oauth.as_object().ok_or_else(invalid_settings)? {
            if let Some(previous) = grants.insert(name.clone(), grant.clone())
                && previous != *grant
            {
                return Err(ControlError::bad_request(
                    "Source accounts have different Keychain authorizations for the same MCP connection. Use distinct server names before combining them.",
                ));
            }
        }
        let target_service = service(Some(&handoff.target_location.root));
        if !patches.contains_key(&target_service) {
            // Never create a second authority beside an existing file-backed login.
            // A signed-in native profile already has its credential item.
            let target = read(&target_service)?.ok_or_else(|| ControlError::bad_request("Sign in to the destination Claude profile before switching its Keychain MCP connections."))?;
            patches.insert(
                target_service.clone(),
                Patch {
                    service: target_service.clone(),
                    expected: target.bytes.clone(),
                    bytes: target.bytes.clone(),
                },
            );
        }
        let patch = patches.get_mut(&target_service).expect("inserted");
        patch.bytes = merge_json_fields(&source.bytes, &patch.bytes, &["mcpOAuth"])?;
    }
    Ok(patches)
}

pub(super) fn preflight(handoffs: &[PreparedHandoff]) -> Result<(), ControlError> {
    patches(handoffs)?;
    Ok(())
}
pub(super) fn install(handoffs: &[PreparedHandoff]) -> Result<(), ControlError> {
    for patch in patches(handoffs)?.into_values() {
        if patch.bytes == patch.expected {
            continue;
        }
        let current = read(&patch.service)?.ok_or_else(unavailable)?;
        if current.bytes != patch.expected || patch.bytes.len() > MAX_SETTINGS {
            return Err(unavailable());
        }
        let status = unsafe {
            SecKeychainItemModifyAttributesAndData(
                current.item.0,
                ptr::null(),
                patch.bytes.len() as u32,
                patch.bytes.as_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(unavailable());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_keychain_names_match_profile_isolation() {
        assert_eq!(service(None), "Claude Code-credentials");
        assert_ne!(
            service(Some(Path::new("/one"))),
            service(Some(Path::new("/two")))
        );
        assert_eq!(
            service(Some(Path::new("/one"))).len(),
            "Claude Code-credentials-".len() + 8
        );
    }
}
