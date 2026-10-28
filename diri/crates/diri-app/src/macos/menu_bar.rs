//! AppKit status-item menu. macOS owns layout, appearance, tracking, scrolling,
//! keyboard navigation and accessibility; every entry is a standard NSMenuItem.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSCellImagePosition, NSControlStateValueOff, NSControlStateValueOn,
    NSEventModifierFlags, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};
use unicode_segmentation::UnicodeSegmentation;

use diri_proto::{AgentKind, AttentionLevel, SessionId};
use diri_ui::BrandMarkKind;

use crate::i18n::{t, tf};
use crate::macos::brand_raster;
use crate::menu_inbox::{InboxModel, InboxRow, InboxSessionRow, TrailingStatus, build_inbox};
use crate::store::{SessionStore, WindowAction, WindowStore};

/// One window's handle on the shared status item.
pub struct NativeMenuBar(Rc<NativeMenuBarInner>);
impl Deref for NativeMenuBar {
    type Target = NativeMenuBarInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
// The status item lives as long as the process. Closing the last window used
// to remove and free it while the app kept running, and on macOS 15 such
// closes were followed within seconds by an Objective-C exception out of
// AppKit's display-cycle observer (`_crashOnException`: SIGTRAP on arm64,
// SIGILL on x86_64). Quitting, and closing any other window, never was. With
// no window left the item is hidden instead, and the next window shows it.
thread_local! { static SHARED_MENU: RefCell<Option<Rc<NativeMenuBarInner>>>=const { RefCell::new(None) }; }
pub struct NativeMenuBarInner {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    // AppKit does not retain delegates or action targets.
    target: Retained<MenuBarTarget>,
    /// Live `NativeMenuBar` handles, one per open window.
    windows: Cell<usize>,
}

impl Drop for NativeMenuBar {
    fn drop(&mut self) {
        let windows = self.windows.get().saturating_sub(1);
        self.windows.set(windows);
        if windows == 0 {
            self.menu.cancelTracking();
            self.status_item.setVisible(false);
        }
    }
}

impl NativeMenuBar {
    #[must_use]
    pub fn new(mtm: MainThreadMarker, store: Arc<RwLock<SessionStore>>) -> Option<Self> {
        if let Some(inner) = SHARED_MENU.with(|menu| menu.borrow().clone()) {
            let windows = inner.windows.get();
            inner.windows.set(windows + 1);
            if windows == 0 {
                inner.status_item.setVisible(true);
            }
            return Some(Self(inner));
        }
        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = status_item.button(mtm)?;
        if let Some(image) = brand_raster::template_diri_logo_ns_image(11.0) {
            button.setImage(Some(&image));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
        } else {
            button.setTitle(&NSString::from_str("Diri"));
        }
        let target = MenuBarTarget::new(mtm, store);
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Diri"));
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        target.menuNeedsUpdate(&menu);
        status_item.setMenu(Some(&menu));
        let inner = Rc::new(NativeMenuBarInner {
            status_item,
            menu,
            target,
            windows: Cell::new(1),
        });
        SHARED_MENU.with(|menu| *menu.borrow_mut() = Some(Rc::clone(&inner)));
        Some(Self(inner))
    }

    pub fn refresh(&mut self) {
        let store = self
            .target
            .ivars()
            .store
            .read()
            .expect("session store lock poisoned");
        let label = match store.global_attention() {
            AttentionLevel::NeedsInput => t("menubar.tooltip_needs_input"),
            _ if store.notifications().unread_count() > 0 => t("menubar.tooltip_unread"),
            AttentionLevel::DoneUnseen => t("menubar.tooltip_done"),
            AttentionLevel::Working => t("menubar.tooltip_working"),
            _ => "Diri",
        };
        if let Some(button) = self.status_item.button(self.target.mtm()) {
            button.setToolTip(Some(&NSString::from_str(label)));
        }
        // Build just before opening. Never reorder items under the pointer or
        // replace the selected session's identity during AppKit menu tracking.
    }
}

struct MenuBarTargetIvars {
    store: Arc<RwLock<SessionStore>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; AppKit invokes the
    // delegate and actions on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuBarTargetIvars]
    struct MenuBarTarget;

    unsafe impl NSObjectProtocol for MenuBarTarget {}

    unsafe impl NSMenuDelegate for MenuBarTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let selected=WindowStore::focused(&self.ivars().store).and_then(|window|window.read().expect("store").selected_session_id().cloned());
            let model = {
                let mut store = self.ivars().store.write().expect("session store lock poisoned");
                let projection = store.menu_bar_projection();
                let mut model = build_inbox(&projection, &HashSet::new());
                for row in &mut model.rows {
                    if let InboxRow::Session(session) = row
                        && store.notifications().session_unread(&SessionId::new(session.session_id.clone()))
                        && session.trailing != Some(TrailingStatus::NeedsYou)
                    {
                        session.trailing = Some(TrailingStatus::Unread);
                    }
                }
                model
            };
            self.populate(menu, &model, selected.as_ref());
        }
    }

    impl MenuBarTarget {
        #[unsafe(method(openDiri:))]
        fn open_diri(&self, _sender: Option<&AnyObject>) {
            self.dispatch(WindowAction::Focus);
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.dispatch(WindowAction::OpenSettings);
        }

        #[unsafe(method(newAgent:))]
        fn new_agent(&self, _sender: Option<&AnyObject>) {
            self.dispatch(WindowAction::OpenLauncher);
        }

        #[unsafe(method(spawnAgent:))]
        fn spawn_agent(&self, sender: &NSMenuItem) {
            let kind=match sender.tag() { 1=>None,2=>Some(AgentKind::CODEX),_=>{
                let kind=self.ivars().store.read().expect("store").preferences().default_agent.clone();Some(kind)
            }};
            self.dispatch(WindowAction::Spawn(kind));
        }

        #[unsafe(method(selectSession:))]
        fn select_session(&self, sender: &NSMenuItem) {
            if let Some(id)=item_session_id(sender) { self.dispatch(WindowAction::Select(id)); }
        }

        #[unsafe(method(closeSession:))]
        fn close_session(&self, sender: &NSMenuItem) {
            if let Some(id)=item_session_id(sender) { self.dispatch(WindowAction::Close(id)); }
        }

        #[unsafe(method(quitDiri:))]
        fn quit_diri(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl MenuBarTarget {
    fn new(mtm: MainThreadMarker, store: Arc<RwLock<SessionStore>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuBarTargetIvars { store });
        // SAFETY: NSObject's init is its designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn populate(&self, menu: &NSMenu, model: &InboxModel, selected: Option<&SessionId>) {
        let mtm = self.mtm();
        menu.removeAllItems();
        for row in &model.rows {
            match row {
                InboxRow::Project { name, .. } => {
                    if menu.numberOfItems() > 0 {
                        menu.addItem(&NSMenuItem::separatorItem(mtm));
                    }
                    let header = NSMenuItem::sectionHeaderWithTitle(
                        &NSString::from_str(&compact_title(name)),
                        mtm,
                    );
                    header.setToolTip(Some(&NSString::from_str(name)));
                    menu.addItem(&header);
                }
                InboxRow::Session(session) => {
                    let item = self.action_item(&session_title(session), sel!(selectSession:), "");
                    let identity = NSString::from_str(&session.session_id);
                    // SAFETY: our actions read representedObject as an NSString;
                    // NSMenuItem retains it for the lifetime of the item.
                    unsafe {
                        item.setRepresentedObject(Some(&identity));
                    }
                    item.setIndentationLevel(session.depth.min(15) as isize);
                    item.setState(if selected.is_some_and(|id| id.0 == session.session_id) {
                        NSControlStateValueOn
                    } else {
                        NSControlStateValueOff
                    });
                    item.setImage(agent_image(&session.agent_id).as_deref());
                    item.setToolTip(Some(&NSString::from_str(&tf(
                        "menubar.session_tooltip",
                        &[("title", &session.title)],
                    ))));
                    menu.addItem(&item);

                    // Native alternate item replaces the old hover-only close
                    // button, keeping the store's existing confirmation flow.
                    let close = self.action_item(
                        &tf(
                            "menubar.close_session",
                            &[("title", &compact_title(&session.title))],
                        ),
                        sel!(closeSession:),
                        "",
                    );
                    unsafe {
                        close.setRepresentedObject(Some(&identity));
                    }
                    close.setIndentationLevel(item.indentationLevel());
                    close.setAlternate(true);
                    close.setKeyEquivalentModifierMask(NSEventModifierFlags::Option);
                    menu.addItem(&close);
                }
            }
        }
        if model.rows.is_empty() {
            menu.addItem(&NSMenuItem::sectionHeaderWithTitle(
                &NSString::from_str(t("menubar.no_sessions")),
                mtm,
            ));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.action_item(t("menubar.open_diri"), sel!(openDiri:), ""));
        menu.addItem(&self.action_item(t("menubar.new_agent"), sel!(newAgent:), "n"));

        // Retain the panel's quick-create shortcuts without adding an
        // unfiltered Agent catalog to this navigation menu. Spawn methods
        // still enforce the selected host's cached availability.
        for (tag, title, key, modifiers) in [
            (
                0,
                t("menubar.default_agent"),
                "t",
                NSEventModifierFlags::Command,
            ),
            (
                1,
                t("menubar.terminal"),
                "t",
                NSEventModifierFlags::Command | NSEventModifierFlags::Option,
            ),
            (
                2,
                "Codex",
                "n",
                NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
            ),
        ] {
            let item = self.action_item(title, sel!(spawnAgent:), key);
            item.setTag(tag);
            item.setKeyEquivalentModifierMask(modifiers);
            item.setHidden(true);
            item.setAllowsKeyEquivalentWhenHidden(true);
            menu.addItem(&item);
        }
        let settings = self.action_item(t("menubar.settings"), sel!(openSettings:), ",");
        settings.setImage(brand_raster::template_settings_ns_image(16.0).as_deref());
        menu.addItem(&settings);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.action_item(t("menubar.quit"), sel!(quitDiri:), "q"));
    }

    fn action_item(&self, title: &str, action: Sel, key: &str) -> Retained<NSMenuItem> {
        // SAFETY: callers supply selectors implemented above with one object
        // argument. NativeMenuBar keeps the target alive until items are removed.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        unsafe {
            item.setTarget(Some(self));
        }
        item.setKeyEquivalentModifierMask(if key.is_empty() {
            NSEventModifierFlags::empty()
        } else {
            NSEventModifierFlags::Command
        });
        item
    }

    fn dispatch(&self, action: WindowAction) {
        if !WindowStore::focused(&self.ivars().store).is_some_and(|window| window.enqueue(action)) {
            objc2_app_kit::NSBeep();
        }
    }
}

fn item_session_id(item: &NSMenuItem) -> Option<SessionId> {
    let object = item.representedObject()?;
    Some(SessionId::new(
        object.downcast_ref::<NSString>()?.to_string(),
    ))
}

fn session_title(session: &InboxSessionRow) -> String {
    let title = compact_title(&session.title);
    let status = match session.trailing {
        Some(TrailingStatus::NeedsYou) if session.destructive => t("menubar.status_needs_approval"),
        Some(TrailingStatus::NeedsYou) => t("menubar.status_needs_you"),
        Some(TrailingStatus::Unread) => t("menubar.status_unread"),
        Some(TrailingStatus::Done) => t("menubar.status_done"),
        Some(TrailingStatus::Sleeping) => t("menubar.status_sleeping"),
        None if session.working => t("menubar.status_working"),
        None => return title,
    };
    format!("{title} · {status}")
}

/// Keep generated conversation titles on one line without splitting emoji or
/// combining characters. The full title remains available in the tooltip.
fn compact_title(title: &str) -> String {
    let single_line = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut graphemes = single_line.graphemes(true);
    let mut result: String = graphemes.by_ref().take(48).collect();
    if graphemes.next().is_some() {
        result.push('…');
    }
    result
}

pub(super) fn agent_image(agent_id: &str) -> Option<Retained<NSImage>> {
    let kind = match agent_id {
        AgentKind::CLAUDE_CODE_ID => Some(BrandMarkKind::Claude),
        AgentKind::CODEX_ID => Some(BrandMarkKind::OpenAi),
        AgentKind::CURSOR_ID => Some(BrandMarkKind::Cursor),
        AgentKind::GEMINI_ID => Some(BrandMarkKind::Gemini),
        _ => None,
    };
    if let Some(kind) = kind {
        return brand_raster::template_ns_image(kind, 16.0);
    }
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("terminal"),
        Some(&NSString::from_str("Terminal")),
    )?;
    image.setTemplate(true);
    Some(image)
}

/// Main-thread AppKit integration check, using an in-memory store only.
/// Run with DIRI_NATIVE_MENU_SMOKE=1 cargo run -p diri-app.
#[cfg(debug_assertions)]
pub fn smoke_test() {
    let mtm = MainThreadMarker::new().expect("native menu smoke must run on the main thread");
    let _app = NSApplication::sharedApplication(mtm);
    let (store, _effects) = SessionStore::headless(crate::store::Prefs::default());
    let mut bar = NativeMenuBar::new(mtm, Arc::new(RwLock::new(store))).unwrap();
    bar.refresh();
    bar.target.menuNeedsUpdate(&bar.menu);
    bar.menu.update();
    assert!(bar.menu.numberOfItems() > 0);
    assert_eq!(
        bar.menu.itemAtIndex(0).unwrap().title().to_string(),
        "No active sessions"
    );

    let mut rows = vec![InboxRow::Project {
        id: "fixture".into(),
        name: "Native menu fixture".into(),
        collapsed: false,
    }];
    for index in 0..80 {
        rows.push(InboxRow::Session(InboxSessionRow {
            session_id: format!("session-{index}"),
            title: format!("Session {index}: {}", "👩🏽‍💻e\u{301}".repeat(35)),
            agent_id: AgentKind::CLAUDE_CODE_ID.into(),
            depth: u16::from(index == 1),
            trailing: Some(match index % 4 {
                0 => TrailingStatus::NeedsYou,
                1 => TrailingStatus::Unread,
                2 => TrailingStatus::Done,
                _ => TrailingStatus::Sleeping,
            }),
            working: false,
            destructive: index == 0,
        }));
    }
    bar.target.populate(
        &bar.menu,
        &InboxModel { rows },
        Some(&SessionId::new("session-1")),
    );
    let first = bar.menu.itemAtIndex(1).unwrap();
    let close = bar.menu.itemAtIndex(2).unwrap();
    let nested = bar.menu.itemAtIndex(3).unwrap();
    assert_eq!(item_session_id(&first), Some(SessionId::new("session-0")));
    assert_eq!(item_session_id(&close), item_session_id(&first));
    assert!(first.title().to_string().ends_with("… · Needs approval"));
    assert!(!first.isAlternate());
    assert!(close.isAlternate());
    assert_eq!(
        close.keyEquivalentModifierMask(),
        NSEventModifierFlags::Option
    );
    assert_eq!(nested.indentationLevel(), 1);
    assert_eq!(nested.state(), NSControlStateValueOn);
    for item in bar.menu.itemArray().iter() {
        assert!(item.view().is_none(), "every row must use AppKit rendering");
    }
    // Item identity is retained on the item itself, independent of list order
    // and subsequent rebuilds. No tag can select a different session by accident.
    bar.target
        .populate(&bar.menu, &InboxModel { rows: Vec::new() }, None);
    assert_eq!(item_session_id(&first), Some(SessionId::new("session-0")));
    assert_eq!(compact_title("hello\n\tworld"), "hello world");
    assert_eq!(
        compact_title(&"👩🏽‍💻".repeat(49)),
        format!("{}…", "👩🏽‍💻".repeat(48))
    );
    drop(bar);
    eprintln!(
        "diri: native menu smoke passed (empty, 80 sessions, statuses, selection, alternates, Unicode, teardown)"
    );
}
