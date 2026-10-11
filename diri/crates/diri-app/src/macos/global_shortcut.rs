//! Opt-in Carbon hotkeys: Show/hide Diri and the quick terminal. No event
//! tap or Accessibility permission, and nothing registered until the user
//! records a shortcut in Settings.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;

use gpui::{App, Keystroke};
use tokio::sync::mpsc;

use crate::commands::CommandId;
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

/// A system-wide shortcut Diri can own. Each has its own Carbon id and its
/// own recorded physical key, so the two never share a registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Hotkey {
    ShowApp,
    QuickTerminal,
}

impl Hotkey {
    pub(crate) const ALL: [Self; 2] = [Self::ShowApp, Self::QuickTerminal];

    fn index(self) -> usize {
        match self {
            Self::ShowApp => 0,
            Self::QuickTerminal => 1,
        }
    }

    fn carbon_id(self) -> HotKeyId {
        HotKeyId {
            signature: SIGNATURE,
            id: self.index() as u32 + 1,
        }
    }

    fn from_carbon_id(id: HotKeyId) -> Option<Self> {
        Self::ALL.into_iter().find(|hotkey| {
            hotkey.carbon_id().signature == id.signature && hotkey.carbon_id().id == id.id
        })
    }

    pub(crate) fn command(self) -> CommandId {
        match self {
            Self::ShowApp => CommandId::ShowApp,
            Self::QuickTerminal => CommandId::ToggleQuickTerminal,
        }
    }

    pub(crate) fn for_command(command: CommandId) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|hotkey| hotkey.command() == command)
    }

    pub(crate) fn key_code(self, prefs: &Prefs) -> Option<u16> {
        match self {
            Self::ShowApp => prefs.global_shortcut_key_code,
            Self::QuickTerminal => prefs.quick_terminal_key_code,
        }
    }

    pub(crate) fn set_key_code(self, prefs: &mut Prefs, code: Option<u16>) {
        match self {
            Self::ShowApp => prefs.global_shortcut_key_code = code,
            Self::QuickTerminal => prefs.quick_terminal_key_code = code,
        }
    }

    fn dispatch(self, cx: &mut App) {
        match self {
            Self::ShowApp => cx.dispatch_action(&crate::commands::ShowApp),
            Self::QuickTerminal => cx.dispatch_action(&crate::commands::ToggleQuickTerminal),
        }
    }
}

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

const SIGNATURE: u32 = u32::from_be_bytes(*b"Diri");

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
struct Slot {
    configured: Option<(u16, u32)>,
    registration: Option<Registration>,
    error: Option<&'static str>,
}

#[derive(Default)]
struct State {
    handler: NativeRef,
    // Carbon borrows this stable allocation until the handler is removed.
    sender: Option<Box<mpsc::Sender<Hotkey>>>,
    slots: [Slot; 2],
    recordings: usize,
}

impl Drop for State {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.registration = None;
        }
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
    let Some(hotkey) = (status == 0).then(|| Hotkey::from_carbon_id(id)).flatten() else {
        return -9874; // eventNotHandledErr
    };
    // SAFETY: the sender outlives the installed handler.
    let sender = unsafe { &*context.cast::<mpsc::Sender<Hotkey>>() };
    let _ = sender.try_send(hotkey);
    0
}

fn register(hotkey: Hotkey, code: u16, modifiers: u32) -> Result<Registration, &'static str> {
    let mut reference = ptr::null_mut();
    // SAFETY: called on the main thread with validated key/modifier values.
    let status = unsafe {
        RegisterEventHotKey(
            u32::from(code),
            modifiers,
            hotkey.carbon_id(),
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

fn configuration(hotkey: Hotkey, prefs: &Prefs) -> Result<Option<(u16, u32)>, &'static str> {
    let stable_id = crate::commands::command(hotkey.command()).stable_id;
    let Some(binding) = prefs
        .shortcut_overrides
        .get(stable_id)
        .and_then(Option::as_ref)
    else {
        return Ok(None);
    };
    let code = hotkey
        .key_code(prefs)
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
pub(crate) fn configure(hotkey: Hotkey, prefs: &Prefs) -> Result<(), &'static str> {
    let configured = configuration(hotkey, prefs)?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if configured.is_some() && state.handler.is_null() {
            return Err("settings.shortcuts.global_unavailable");
        }
        let recordings = state.recordings;
        let slot = &mut state.slots[hotkey.index()];
        if slot.configured == configured && slot.registration.is_some() {
            return Ok(());
        }
        let registration = configured
            .map(|(code, modifiers)| register(hotkey, code, modifiers))
            .transpose()?;
        // Keep a successful replacement reserved while the current recording
        // is committed; dropping and re-registering would introduce a race.
        slot.registration = if recordings <= 1 { registration } else { None };
        slot.configured = configured;
        slot.error = None;
        Ok(())
    })
}

/// Reconfigure every hotkey, e.g. after all overrides were reset.
pub(crate) fn configure_all(prefs: &Prefs) {
    for hotkey in Hotkey::ALL {
        if let Err(error) = configure(hotkey, prefs) {
            STATE.with(|state| state.borrow_mut().slots[hotkey.index()].error = Some(error));
        }
    }
}

pub(crate) fn error(hotkey: Hotkey) -> Option<&'static str> {
    STATE.with(|state| state.borrow().slots[hotkey.index()].error)
}

/// Temporarily release our own hotkeys so the shortcut editor can record them.
pub(crate) struct Recording;

impl Recording {
    pub(crate) fn begin() -> Self {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.recordings += 1;
            for slot in &mut state.slots {
                slot.registration = None;
            }
        });
        Self
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.recordings = state.recordings.saturating_sub(1);
            if state.recordings != 0 {
                return;
            }
            for hotkey in Hotkey::ALL {
                let slot = &mut state.slots[hotkey.index()];
                if slot.registration.is_some() {
                    continue;
                }
                let Some((code, modifiers)) = slot.configured else {
                    continue;
                };
                match register(hotkey, code, modifiers) {
                    Ok(registration) => {
                        slot.registration = Some(registration);
                        slot.error = None;
                    }
                    Err(error) => slot.error = Some(error),
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
            kind: 5, // kEventHotKeyPressed
        };
        // SAFETY: Carbon borrows a stable Box, retained until handler removal.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                pressed,
                1,
                &event,
                (&mut *sender as *mut mpsc::Sender<Hotkey>).cast(),
                &mut state.handler,
            )
        };
        if status == 0 {
            state.sender = Some(sender);
        }
    });
    configure_all(prefs);
    cx.spawn(async move |cx| {
        while let Some(hotkey) = events.recv().await {
            cx.update(|cx| hotkey.dispatch(cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_map_to_distinct_carbon_ids_and_commands() {
        for hotkey in Hotkey::ALL {
            let id = hotkey.carbon_id();
            assert_eq!(id.signature, SIGNATURE);
            assert_eq!(Hotkey::from_carbon_id(id), Some(hotkey));
            assert_eq!(Hotkey::for_command(hotkey.command()), Some(hotkey));
        }
        assert_ne!(
            Hotkey::ShowApp.carbon_id().id,
            Hotkey::QuickTerminal.carbon_id().id
        );
        let foreign = HotKeyId {
            signature: u32::from_be_bytes(*b"Othr"),
            id: 1,
        };
        assert_eq!(Hotkey::from_carbon_id(foreign), None);
        assert_eq!(Hotkey::for_command(CommandId::Quit), None);
    }

    #[test]
    fn each_hotkey_reads_its_own_recorded_key() {
        let mut prefs = Prefs::default();
        Hotkey::QuickTerminal.set_key_code(&mut prefs, Some(50));
        assert_eq!(Hotkey::QuickTerminal.key_code(&prefs), Some(50));
        assert_eq!(Hotkey::ShowApp.key_code(&prefs), None);
    }

    #[test]
    fn configuration_needs_a_recorded_key_and_a_real_modifier() {
        let mut prefs = Prefs::default();
        assert_eq!(configuration(Hotkey::QuickTerminal, &prefs), Ok(None));
        prefs
            .shortcut_overrides
            .insert("quick-terminal".into(), Some("ctrl-`".into()));
        assert_eq!(
            configuration(Hotkey::QuickTerminal, &prefs),
            Err("settings.shortcuts.global_unsupported")
        );
        // kVK_ANSI_Grave
        Hotkey::QuickTerminal.set_key_code(&mut prefs, Some(50));
        assert_eq!(
            configuration(Hotkey::QuickTerminal, &prefs),
            Ok(Some((50, 1 << 12)))
        );
        // The other hotkey is untouched by this one's binding.
        assert_eq!(configuration(Hotkey::ShowApp, &prefs), Ok(None));
        prefs
            .shortcut_overrides
            .insert("quick-terminal".into(), Some("shift-`".into()));
        assert_eq!(
            configuration(Hotkey::QuickTerminal, &prefs),
            Err("settings.shortcuts.global_modifier")
        );
    }
}
