//! The Dock icon's menu (right-click or long-press on Diri in the Dock):
//! the sessions that need you, then the agents working, each a native item
//! with its agent's mark; choosing one raises the window already showing it
//! (or the focused one) on that session. "Mark All as Seen" clears every
//! finished or failed mark at once.
//!
//! AppKit asks for the menu each time it opens (`applicationDockMenu:`, via
//! `gpui_macos::set_dock_menu_provider`), so it is filled from the store
//! then and nothing runs while it is closed. One `NSMenu` and one target
//! live for the process.

use std::cell::RefCell;
use std::sync::{Arc, RwLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSMenu, NSMenuItem, NSRequestUserAttentionType,
    NSRunningApplication,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};

use diri_proto::SessionId;

use crate::dock_menu::{DockMenuModel, DockMenuRow};
use crate::i18n::{t, tf};
use crate::macos::menu_bar::agent_image;
use crate::store::{SessionStore, WindowAction, WindowStore};

struct DockMenu {
    menu: Retained<NSMenu>,
    // AppKit does not retain action targets.
    target: Retained<DockMenuTarget>,
}

thread_local! { static DOCK_MENU: RefCell<Option<DockMenu>> = const { RefCell::new(None) }; }

/// Answers the Dock's menu requests from `store` for the rest of the process.
pub fn install(mtm: MainThreadMarker, store: Arc<RwLock<SessionStore>>) {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Diri"));
    menu.setAutoenablesItems(false);
    let target = DockMenuTarget::new(mtm, store);
    DOCK_MENU.with(|slot| *slot.borrow_mut() = Some(DockMenu { menu, target }));
    gpui_macos::set_dock_menu_provider(Some(Box::new(|| {
        DOCK_MENU.with(|slot| {
            let slot = slot.borrow();
            let Some(dock) = slot.as_ref() else {
                return std::ptr::null_mut();
            };
            let model = dock.target.model();
            dock.target.populate(&dock.menu, &model);
            Retained::as_ptr(&dock.menu) as *mut std::ffi::c_void
        })
    })));
}

/// One bounce of the Dock icon (an informational request: it stops by
/// itself, and at once when Diri comes to the front).
pub fn bounce() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm)
            .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
    }
}

struct DockMenuTargetIvars {
    store: Arc<RwLock<SessionStore>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; AppKit invokes the
    // actions on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DockMenuTargetIvars]
    struct DockMenuTarget;

    unsafe impl NSObjectProtocol for DockMenuTarget {}

    impl DockMenuTarget {
        #[unsafe(method(selectSession:))]
        fn select_session(&self, sender: &NSMenuItem) {
            let Some(id) = item_session_id(sender) else {
                return;
            };
            let window = WindowStore::showing(&self.ivars().store, &id);
            self.deliver(window, WindowAction::Select(id));
        }

        #[unsafe(method(openDiri:))]
        fn open_diri(&self, _sender: Option<&AnyObject>) {
            self.deliver(WindowStore::focused(&self.ivars().store), WindowAction::Focus);
        }

        #[unsafe(method(markAllSeen:))]
        fn mark_all_seen(&self, _sender: Option<&AnyObject>) {
            self.ivars()
                .store
                .write()
                .expect("session store lock poisoned")
                .mark_all_seen();
        }
    }
);

impl DockMenuTarget {
    fn new(mtm: MainThreadMarker, store: Arc<RwLock<SessionStore>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DockMenuTargetIvars { store });
        // SAFETY: NSObject's init is its designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn model(&self) -> DockMenuModel {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let mut store = self
            .ivars()
            .store
            .write()
            .expect("session store lock poisoned");
        crate::dock_menu::build(&mut store, now_ms)
    }

    fn populate(&self, menu: &NSMenu, model: &DockMenuModel) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let section = |title: &str, rows: &[DockMenuRow]| {
            if rows.is_empty() {
                return;
            }
            if menu.numberOfItems() > 0 {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
                &NSString::from_str(title),
                mtm,
            ));
            for row in rows {
                let item = self.action_item(&row.label(), sel!(selectSession:));
                let identity = NSString::from_str(&row.session_id.0);
                // SAFETY: `selectSession:` reads representedObject as an
                // NSString; the item retains it for its lifetime.
                unsafe {
                    item.setRepresentedObject(Some(&identity));
                }
                item.setImage(agent_image(&row.agent_id).as_deref());
                menu.addItem(&item);
            }
        };
        section(t("dock.section_needs_you"), &model.attention);
        section(t("dock.section_working"), &model.working);
        if model.more > 0 {
            menu.addItem(&self.action_item(&tf("dock.more", &[("n", &model.more)]), sel!(openDiri:)));
        }
        if model.can_mark_seen {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&self.action_item(t("dock.mark_all_seen"), sel!(markAllSeen:)));
        }
        // AppKit appends the window list and the standard items after these.
    }

    fn action_item(&self, title: &str, action: Sel) -> Retained<NSMenuItem> {
        // SAFETY: callers pass selectors implemented above that take one
        // object argument; DOCK_MENU keeps the target alive for the process.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(self));
        }
        item
    }

    /// Brings Diri forward with only its key window (the one the action then
    /// raises), not every window it has.
    fn deliver(&self, window: Option<WindowStore>, action: WindowAction) {
        if !window.is_some_and(|window| window.enqueue(action)) {
            objc2_app_kit::NSBeep();
            return;
        }
        NSRunningApplication::currentApplication()
            .activateWithOptions(NSApplicationActivationOptions::empty());
    }
}

fn item_session_id(item: &NSMenuItem) -> Option<SessionId> {
    let object = item.representedObject()?;
    Some(SessionId::new(
        object.downcast_ref::<NSString>()?.to_string(),
    ))
}

/// Main-thread AppKit check of the Dock menu, with an in-memory store. Run
/// with DIRI_NATIVE_MENU_SMOKE=1 cargo run -p diri-app (after the menu-bar
/// smoke).
#[cfg(debug_assertions)]
pub fn smoke_test() {
    use crate::dock_menu::DockRowState;
    let mtm = MainThreadMarker::new().expect("dock menu smoke must run on the main thread");
    let (store, _effects) = SessionStore::headless(crate::store::Prefs::default());
    let target = DockMenuTarget::new(mtm, Arc::new(RwLock::new(store)));
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Diri"));
    target.populate(&menu, &target.model());
    assert_eq!(menu.numberOfItems(), 0, "nothing to list, nothing added");
    let row = |id: &str, state| DockMenuRow {
        session_id: SessionId::new(id),
        agent_id: diri_proto::AgentKind::CLAUDE_CODE_ID.into(),
        state,
        title: id.into(),
        detail: "Allow deploy?".into(),
        age: Some("now".into()),
    };
    let model = DockMenuModel {
        attention: vec![row("asks", DockRowState::NeedsYou), row("done", DockRowState::Finished)],
        working: vec![row("busy", DockRowState::Working)],
        more: 2,
        can_mark_seen: true,
    };
    target.populate(&menu, &model);
    let titles: Vec<String> = menu
        .itemArray()
        .iter()
        .map(|item| item.title().to_string())
        .collect();
    assert_eq!(
        titles,
        [
            "Needs You",
            "asks — Allow deploy? · now",
            "done — Allow deploy? · now",
            "",
            "Working",
            "busy — Allow deploy? · now",
            "2 more in Diri…",
            "",
            "Mark All as Seen",
        ]
    );
    let first = menu.itemAtIndex(1).unwrap();
    assert_eq!(item_session_id(&first), Some(SessionId::new("asks")));
    assert!(first.image().is_some(), "agent mark");
    // Reopening rebuilds from scratch.
    target.populate(&menu, &DockMenuModel::default());
    assert_eq!(menu.numberOfItems(), 0);
    eprintln!("diri: dock menu smoke passed (empty, sections, overflow, mark seen, identity)");
}
