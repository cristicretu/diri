//! Open at login and the wake helper, both through `SMAppService`, so each
//! is a standard item the user sees, approves, and can remove in System
//! Settings > General > Login Items.
//!
//! - `mainAppService`: diri opens at login, so runs due across a restart can
//!   catch up. No privileges.
//! - `daemonServiceWithPlistName:`: the bundled `diri-wake-helper`, which
//!   runs as root on demand and only schedules wakes (see
//!   `diri_engine::wake`). macOS requires an administrator to approve it.

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoginItemStatus {
    Enabled,
    Disabled,
    /// Registered, but the user switched it off in System Settings.
    RequiresApproval,
    /// Not a bundled app (a dev build) or the service is missing.
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Service {
    /// diri itself, opened at login.
    App,
    /// The privileged wake helper.
    WakeHelper,
}

/// Plist in `Contents/Library/LaunchDaemons`, named after its launchd label.
const WAKE_HELPER_PLIST: &str = "com.dirijor.diri.wake.plist";

fn service(which: Service) -> Option<*mut AnyObject> {
    let class = AnyClass::get(c"SMAppService")?;
    // SAFETY: both are class methods returning an autoreleased SMAppService
    // that stays valid for the current autorelease scope.
    let service: *mut AnyObject = unsafe {
        match which {
            Service::App => msg_send![class, mainAppService],
            Service::WakeHelper => {
                let name = objc2_foundation::NSString::from_str(WAKE_HELPER_PLIST);
                msg_send![class, daemonServiceWithPlistName: &*name]
            }
        }
    };
    (!service.is_null()).then_some(service)
}

pub(crate) fn status() -> LoginItemStatus {
    status_of(Service::App)
}

pub(crate) fn set_enabled(enabled: bool) -> Result<LoginItemStatus, String> {
    set_enabled_of(Service::App, enabled)
}

/// A synchronous XPC round trip to launchd that can take seconds: call it
/// off the UI thread.
pub(crate) fn status_of(which: Service) -> LoginItemStatus {
    // Callers are executor threads; drain the autoreleased service here.
    let status = objc2::rc::autoreleasepool(|_| {
        let service = service(which)?;
        // SAFETY: `status` is an NSInteger-valued property of SMAppService.
        Some(unsafe { msg_send![service, status] })
    });
    let Some(status): Option<isize> = status else {
        return LoginItemStatus::Unavailable;
    };
    match status {
        1 => LoginItemStatus::Enabled,
        0 => LoginItemStatus::Disabled,
        2 => LoginItemStatus::RequiresApproval,
        _ => LoginItemStatus::Unavailable,
    }
}

/// Registers or unregisters a service. Returns the new status; a helper that
/// still needs the administrator's approval reports `RequiresApproval`. Like
/// `status_of`, call it off the UI thread.
pub(crate) fn set_enabled_of(which: Service, enabled: bool) -> Result<LoginItemStatus, String> {
    objc2::rc::autoreleasepool(|_| set_enabled_in_pool(which, enabled))
}

fn set_enabled_in_pool(which: Service, enabled: bool) -> Result<LoginItemStatus, String> {
    let service = service(which).ok_or_else(|| "Login items aren't available here.".to_owned())?;
    let mut error: *mut AnyObject = std::ptr::null_mut();
    // SAFETY: both selectors take an `NSError **` out-parameter and return BOOL.
    let ok: Bool = unsafe {
        if enabled {
            msg_send![service, registerAndReturnError: &mut error]
        } else {
            msg_send![service, unregisterAndReturnError: &mut error]
        }
    };
    let now = status_of(which);
    if ok.as_bool() || (enabled && now == LoginItemStatus::RequiresApproval) {
        return Ok(now);
    }
    Err(if error.is_null() {
        "macOS didn't accept the change.".to_owned()
    } else {
        // SAFETY: a non-null out-parameter is an NSError.
        let description: *mut AnyObject = unsafe { msg_send![error, localizedDescription] };
        nsstring(description).unwrap_or_else(|| "macOS didn't accept the change.".to_owned())
    })
}

/// Opens System Settings > General > Login Items.
pub(crate) fn open_settings() {
    if let Some(class) = AnyClass::get(c"SMAppService") {
        // SAFETY: a class method with no arguments and no return value.
        let _: () = unsafe { msg_send![class, openSystemSettingsLoginItems] };
    }
}

fn nsstring(string: *mut AnyObject) -> Option<String> {
    if string.is_null() {
        return None;
    }
    // SAFETY: `UTF8String` returns a NUL-terminated buffer owned by `string`.
    let utf8: *const std::ffi::c_char = unsafe { msg_send![string, UTF8String] };
    (!utf8.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned()
    })
}
