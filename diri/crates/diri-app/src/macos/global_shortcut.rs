//! A single opt-in Carbon hotkey. No event tap or Accessibility permission.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;

use gpui::{App, Keystroke};
use tokio::sync::mpsc;

use crate::store::Prefs;

pub(crate) fn should_hide(window: &gpui::Window) -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSView};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    if !NSApplication::sharedApplication(mtm).isActive() {
        return false;
    }
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::AppKit(handle) = handle.as_raw()
    {
        // SAFETY: GPUI owns this NSView for the lifetime of the Window.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        return view
            .window()
            .is_some_and(|native| native.isVisible() && !native.isMiniaturized());
    }
    false
}

pub(crate) fn reveal(window: &gpui::Window) {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::AppKit(handle) = handle.as_raw()
    {
        // SAFETY: GPUI owns this NSView for the lifetime of the Window.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        if let Some(native) = view.window()
            && native.isMiniaturized()
        {
            native.deminiaturize(None);
        }
    }
    window.activate_window();
}

type NativeRef = *mut c_void;
type Handler = unsafe extern "C" fn(NativeRef, NativeRef, NativeRef) -> i32;

#[repr(C)]
struct EventType {
    class: u32,
    kind: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct HotKeyId {
    signature: u32,
    id: u32,
}

const HOTKEY_ID: HotKeyId = HotKeyId {
    signature: u32::from_be_bytes(*b"Diri"),
    id: 1,
};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> NativeRef;
    fn InstallEventHandler(
        target: NativeRef,
        handler: Handler,
        count: u32,
        events: *const EventType,
        context: NativeRef,
        result: *mut NativeRef,
    ) -> i32;
    fn RemoveEventHandler(handler: NativeRef) -> i32;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: HotKeyId,
        target: NativeRef,
        options: u32,
        result: *mut NativeRef,
    ) -> i32;
    fn UnregisterEventHotKey(hotkey: NativeRef) -> i32;
    fn GetEventParameter(
        event: NativeRef,
        name: u32,
        kind: u32,
        actual_kind: *mut u32,
        size: u32,
        actual_size: *mut u32,
        result: NativeRef,
    ) -> i32;
}

struct Registration(NativeRef);

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: this owns a successful registration, on the main thread.
        unsafe { UnregisterEventHotKey(self.0) };
    }
}

#[derive(Default)]
struct State {
    handler: NativeRef,
    // Carbon borrows this stable allocation until the handler is removed.
    sender: Option<Box<mpsc::Sender<()>>>,
    configured: Option<(u16, u32)>,
    registration: Option<Registration>,
    recordings: usize,
    error: Option<&'static str>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.registration = None;
        if !self.handler.is_null() {
            // SAFETY: the handler is owned here; sender is still alive.
            unsafe { RemoveEventHandler(self.handler) };
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

unsafe extern "C" fn pressed(_: NativeRef, event: NativeRef, context: NativeRef) -> i32 {
    let mut id = HotKeyId {
        signature: 0,
        id: 0,
    };
    // SAFETY: Carbon supplies a hotkey event and the live sender allocation.
    let status = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            ptr::null_mut(),
            std::mem::size_of::<HotKeyId>() as u32,
            ptr::null_mut(),
            (&mut id as *mut HotKeyId).cast(),
        )
    };
    if status != 0 || id.signature != HOTKEY_ID.signature || id.id != HOTKEY_ID.id {
        return -9874; // eventNotHandledErr
    }
    // SAFETY: the sender outlives the installed handler.
    let sender = unsafe { &*context.cast::<mpsc::Sender<()>>() };
    let _ = sender.try_send(());
    0
}

fn register(code: u16, modifiers: u32) -> Result<Registration, &'static str> {
    let mut reference = ptr::null_mut();
    // SAFETY: called on the main thread with validated key/modifier values.
    let status = unsafe {
        RegisterEventHotKey(
            u32::from(code),
            modifiers,
            HOTKEY_ID,
            GetApplicationEventTarget(),
            1, // kEventHotKeyExclusive: report conflicts instead of sharing.
            &mut reference,
        )
    };
    if status == 0 && !reference.is_null() {
        Ok(Registration(reference))
    } else if status == -9878 {
        Err("settings.shortcuts.global_conflict")
    } else {
        Err("settings.shortcuts.global_unavailable")
    }
}

fn configuration(prefs: &Prefs) -> Result<Option<(u16, u32)>, &'static str> {
    let Some(binding) = prefs
        .shortcut_overrides
        .get("show-app")
        .and_then(Option::as_ref)
    else {
        return Ok(None);
    };
    let code = prefs
        .global_shortcut_key_code
        .filter(|code| *code <= 127)
        .ok_or("settings.shortcuts.global_unsupported")?;
    let key = Keystroke::parse(binding).map_err(|_| "settings.shortcuts.global_unsupported")?;
    let modifiers = key.modifiers;
    if modifiers.function || !(modifiers.platform || modifiers.control || modifiers.alt) {
        return Err("settings.shortcuts.global_modifier");
    }
    // HIToolbox cmdKey, shiftKey, optionKey, controlKey.
    let native = (u32::from(modifiers.platform) << 8)
        | (u32::from(modifiers.shift) << 9)
        | (u32::from(modifiers.alt) << 11)
        | (u32::from(modifiers.control) << 12);
    Ok(Some((code, native)))
}

/// Validate and replace atomically; a conflict leaves the previous hotkey intact.
pub(crate) fn configure(prefs: &Prefs) -> Result<(), &'static str> {
    let configured = configuration(prefs)?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if configured.is_some() && state.handler.is_null() {
            return Err("settings.shortcuts.global_unavailable");
        }
        if state.configured == configured && state.registration.is_some() {
            return Ok(());
        }
        let registration = configured
            .map(|(code, modifiers)| register(code, modifiers))
            .transpose()?;
        // Keep a successful replacement reserved while the current recording
        // is committed; dropping and re-registering would introduce a race.
        state.registration = if state.recordings <= 1 {
            registration
        } else {
            None
        };
        state.configured = configured;
        state.error = None;
        Ok(())
    })
}

pub(crate) fn error() -> Option<&'static str> {
    STATE.with(|state| state.borrow().error)
}

/// Temporarily release our own hotkey so the shortcut editor can record it.
pub(crate) struct Recording;

impl Recording {
    pub(crate) fn begin() -> Self {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.recordings += 1;
            state.registration = None;
        });
        Self
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.recordings = state.recordings.saturating_sub(1);
            if state.recordings == 0
                && state.registration.is_none()
                && let Some((code, modifiers)) = state.configured
            {
                match register(code, modifiers) {
                    Ok(registration) => {
                        state.registration = Some(registration);
                        state.error = None;
                    }
                    Err(error) => state.error = Some(error),
                }
            }
        });
    }
}

pub(crate) fn install(cx: &mut App, prefs: &Prefs) {
    let (sender, mut events) = mpsc::channel(1);
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let mut sender = Box::new(sender);
        let event = EventType {
            class: u32::from_be_bytes(*b"keyb"),
            kind: 6,
        };
        // SAFETY: Carbon borrows a stable Box, retained until handler removal.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                pressed,
                1,
                &event,
                (&mut *sender as *mut mpsc::Sender<()>).cast(),
                &mut state.handler,
            )
        };
        if status == 0 {
            state.sender = Some(sender);
        }
    });
    if let Err(error) = configure(prefs) {
        STATE.with(|state| state.borrow_mut().error = Some(error));
    }
    cx.spawn(async move |cx| {
        while events.recv().await.is_some() {
            cx.update(|cx| cx.dispatch_action(&crate::commands::ShowApp));
        }
    })
    .detach();
}
