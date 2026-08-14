//! Native menu-bar session list.
//!
//! Mirrors the sidebar: project collapse, spawn indent, agent marks, and Zzz
//! chip. Project collapse is local to the menu bar and never writes sidebar prefs.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::{Arc, RwLock};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationOptions, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSBox, NSBoxType, NSButton, NSCellImagePosition, NSColor, NSEvent, NSEventMask,
    NSEventModifierFlags, NSFocusRingType, NSFont, NSFontWeightMedium, NSFontWeightRegular,
    NSGradient, NSImage, NSImageAlignment, NSImageScaling, NSImageSymbolConfiguration, NSImageView,
    NSLineBreakMode, NSPanel, NSPopUpMenuWindowLevel, NSRunningApplication, NSScrollView,
    NSStatusBar, NSStatusBarButton, NSStatusItem, NSTextAlignment, NSTextField, NSTrackingArea,
    NSTrackingAreaOptions, NSVariableStatusItemLength, NSView, NSViewBoundsDidChangeNotification,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};

use diri_proto::{AgentKind, AttentionLevel, ProjectId, SessionId};
use diri_ui::{Appearance, BrandMarkKind, Chip, Fill, Ink, Radius, SemanticColors};
use gpui::Rgba;

use crate::app_theme;
use crate::macos::brand_raster;
use crate::menu_inbox::{InboxModel, InboxRow, InboxSessionRow, TrailingStatus, build_inbox};
use crate::store::{SessionStore, SpawnOptions};

const POPUP_WIDTH: f64 = 300.0;
/// One bar, at the bottom. A header and a footer bracketing eleven 28pt rows
/// spent 84pt of a 300pt panel on chrome and made the list look like a middle.
const FOOTER_HEIGHT: f64 = 42.0;
/// Height of the gradient that fades the list into the panel at each end, so a
/// scrolled row leaves by dissolving rather than by being sliced off.
const FADE_HEIGHT: f64 = 20.0;
/// Chin controls are 28pt centred in the bar.
const CHIN_CONTROL_Y: f64 = (FOOTER_HEIGHT - 28.0) / 2.0;
const PROJECT_HEIGHT: f64 = 28.0;
const ROW_HEIGHT: f64 = 28.0;
const BODY_PADDING: f64 = 4.0;
const EMPTY_BODY_HEIGHT: f64 = 88.0;
const MAX_BODY_HEIGHT: f64 = 360.0;
/// Matches `diri_ui::Space::INDENT` / sidebar fold column.
const INDENT: f64 = 12.0;
/// Matches `diri_ui::Space::ROW_H` (row padding inside the fill).
const ROW_PAD: f64 = 8.0;
/// Matches `diri_ui::Space::INSET`: the sidebar list insets every row fill.
const ROW_INSET: f64 = 10.0;
/// Matches the sidebar row's flex `gap(px(8.0))`.
const ROW_GAP: f64 = 8.0;
/// First content column, identical to the sidebar's `INSET + ROW_H`.
const CONTENT_X: f64 = ROW_INSET + ROW_PAD;
/// Trailing content edge, mirroring the sidebar's `pr(ROW_H)` inside the inset.
const CONTENT_RIGHT: f64 = POPUP_WIDTH - ROW_INSET - ROW_PAD;
/// One spawn level: the sidebar draws a rail column plus the row gap.
const DEPTH_STEP: f64 = INDENT + ROW_GAP;
const GLYPH_SIZE: f64 = 16.0;
/// Brand mark height; width follows the SVG aspect (59.5×42.5).
/// Kept short so the wide mark matches other ~16pt menu-bar icons optically.
const DIRI_LOGO_HEIGHT: f64 = 11.0;
const DIRI_LOGO_WIDTH: f64 = DIRI_LOGO_HEIGHT * (59.5 / 42.5);
/// Symmetric pad around the mark.
const BRAND_HIT_WIDTH: f64 = ROW_PAD + DIRI_LOGO_WIDTH + ROW_PAD;
/// Chin, left to right: brand, settings … Quit, New Agent.
const SETTINGS_X: f64 = ROW_INSET + BRAND_HIT_WIDTH + ROW_GAP;
const QUIT_X: f64 = POPUP_WIDTH - ROW_INSET - NEW_AGENT_WIDTH - ROW_GAP - QUIT_WIDTH;
const FOLDER_BADGE: f64 = 18.0;
/// `diri_ui::IconSize::COMPACT`: every sidebar glyph authored at 8-11pt
/// actually renders in a 14pt box.
const GLYPH_BOX_COMPACT: f64 = 14.0;
/// NSTextField draws its first glyph ~2pt inside the frame; GPUI text starts
/// at the element edge, so left-aligned labels need that back.
const TEXT_INSET: f64 = 2.0;
const NEW_AGENT_WIDTH: f64 = 100.0;
const SETTINGS_HIT: f64 = 32.0;
const QUIT_WIDTH: f64 = 54.0;
const CLOSE_HIT: f64 = 16.0;
/// SF Symbol point size for the row ✕. Sidebar maps 8.5→14pt SVG; the same
/// number on an SF Symbol fills the hit and looks oversized, so keep this low.
const CLOSE_GLYPH_PT: f64 = 9.0;
/// Matches `diri_ui::Radius::CHIP`.
const CLOSE_RADIUS: f64 = Radius::CHIP as f64;
const CLOSE_CHIP_GAP: f64 = 4.0;
const SELECTED_FILL_ALPHA: f64 = 0.10;
const HOVER_FILL_ALPHA: f64 = 0.06;

/// The panel's palette, resolved from the active diri theme rather than from
/// system colors. AppKit's `labelColor` family tracks macOS light/dark, which
/// is the wrong axis: a Dracula or Solarized workbench should not hand its menu
/// bar a stock grey panel. Colors are converted once per theme change, not per
/// row, because every row rebuild would otherwise re-cross the bridge.
struct MenuTheme {
    id: String,
    is_dark: bool,
    primary: Retained<NSColor>,
    secondary: Retained<NSColor>,
    tertiary: Retained<NSColor>,
    surface: Retained<NSColor>,
    separator: Retained<NSColor>,
}

impl MenuTheme {
    fn new(id: String, colors: SemanticColors) -> Self {
        Self {
            id,
            is_dark: colors.appearance == Appearance::Dark,
            primary: rgba_color(colors.primary),
            secondary: rgba_color(colors.secondary),
            tertiary: rgba_color(colors.tertiary),
            surface: rgba_color(colors.floating_surface()),
            separator: rgba_color(colors.floating_stroke()),
        }
    }

    /// Fills and quiet text tones are the foreground at reduced alpha, exactly
    /// as `Fill::subtle` and friends derive them on the GPUI side.
    fn primary_alpha(&self, alpha: f64) -> Retained<NSColor> {
        self.primary.colorWithAlphaComponent(alpha)
    }

    /// Anything AppKit still draws itself — the scroller, focus rings — follows
    /// the window's appearance, so a light theme has to say so explicitly or it
    /// gets a dark scrollbar over a light panel.
    fn appearance(&self) -> Option<Retained<NSAppearance>> {
        let name = if self.is_dark {
            unsafe { NSAppearanceNameDarkAqua }
        } else {
            unsafe { NSAppearanceNameAqua }
        };
        NSAppearance::appearanceNamed(name)
    }
}

fn rgba_color(color: Rgba) -> Retained<NSColor> {
    rgba_ns(color.r, color.g, color.b, color.a)
}

pub struct NativeMenuBar {
    store: Arc<RwLock<SessionStore>>,
    _status_item: Retained<NSStatusItem>,
    button: Retained<NSStatusBarButton>,
    panel: Retained<MenuBarPanel>,
    surface: Retained<NSVisualEffectView>,
    brand_hit: Retained<MenuBarBrandHit>,
    brand_mark: Retained<NSImageView>,
    new_agent_chrome: Retained<MenuBarHoverChrome>,
    settings_chrome: Retained<MenuBarHoverChrome>,
    quit_chrome: Retained<MenuBarHoverChrome>,
    backdrop: Retained<NSBox>,
    footer_divider: Retained<NSBox>,
    top_fade: Retained<MenuBarFade>,
    bottom_fade: Retained<MenuBarFade>,
    scroll: Retained<NSScrollView>,
    body: Retained<NSView>,
    target: Retained<MenuBarTarget>,
    /// Re-syncs row hover while the list scrolls; see [`MenuBarHoverRow::sync_hover_to_pointer`].
    scroll_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    last_fingerprint: Option<u64>,
    /// Last level pushed to the status item; see [`NativeMenuBar::set_attention`].
    last_attention: Option<AttentionLevel>,
    theme: MenuTheme,
    /// Colors are baked into views when they are built, so a theme change has
    /// to repaint even though the model behind the fingerprint is unchanged.
    needs_full_repaint: bool,
}

impl Drop for NativeMenuBar {
    fn drop(&mut self) {
        // The dismiss monitors are process-global and retain this target; clear
        // them so a rebuilt menu bar cannot stack observers on the same status item.
        self.target.remove_monitors();
        if let Some(observer) = self.scroll_observer.take() {
            // SAFETY: observer came from addObserverForName on this same center.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(observer.as_ref()) };
        }
    }
}

impl NativeMenuBar {
    #[must_use]
    pub fn new(mtm: MainThreadMarker, store: Arc<RwLock<SessionStore>>) -> Option<Self> {
        let theme = {
            let prefs_theme = store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .terminal_theme
                .clone();
            let colors = app_theme::sidebar_colors(&prefs_theme);
            MenuTheme::new(prefs_theme, colors)
        };

        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = status_item.button(mtm)?;
        button.setToolTip(Some(&NSString::from_str("diri")));

        // Custom panel: borderless windows refuse key status unless overridden.
        let panel = MenuBarPanel::new(mtm);
        panel.setFloatingPanel(true);
        panel.setLevel(NSPopUpMenuWindowLevel);
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setAnimationBehavior(NSWindowAnimationBehavior::UtilityWindow);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::MoveToActiveSpace
                | NSWindowCollectionBehavior::Transient
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );

        let surface = NSVisualEffectView::initWithFrame(
            NSVisualEffectView::alloc(mtm),
            rect(0.0, 0.0, POPUP_WIDTH, 140.0),
        );
        surface.setMaterial(NSVisualEffectMaterial::Sidebar);
        surface.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        surface.setState(NSVisualEffectState::Active);
        surface.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        surface.setWantsLayer(true);
        unsafe {
            let layer: *mut AnyObject = msg_send![&*surface, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: 10.0_f64];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
        // The vibrancy view stays for the shadow and rounding, but an opaque
        // themed plate sits on top of it: a Dracula workbench should not open a
        // stock grey menu. Painted as an NSBox rather than a layer color so this
        // file still does not need a second graphics binding just for one fill.
        let backdrop = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, POPUP_WIDTH, 140.0));
        backdrop.setBoxType(NSBoxType::Custom);
        backdrop.setBorderWidth(0.0);
        backdrop.setCornerRadius(10.0);
        backdrop.setContentViewMargins(NSSize::new(0.0, 0.0));
        backdrop.setFillColor(&theme.surface);
        backdrop.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        surface.addSubview(&backdrop);

        panel.setAppearance(theme.appearance().as_deref());
        panel.setContentView(Some(&surface));

        let body = NSView::initWithFrame(
            NSView::alloc(mtm),
            rect(0.0, 0.0, POPUP_WIDTH, EMPTY_BODY_HEIGHT),
        );

        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(0.0, FOOTER_HEIGHT, POPUP_WIDTH, EMPTY_BODY_HEIGHT),
        );
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        scroll.setDocumentView(Some(&body));
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        surface.addSubview(&scroll);

        let top_fade = MenuBarFade::new(mtm, FadeEdge::Top, &theme.surface);
        let bottom_fade = MenuBarFade::new(mtm, FadeEdge::Bottom, &theme.surface);
        surface.addSubview(&top_fade);
        surface.addSubview(&bottom_fade);

        // `updateTrackingAreas` is supposed to run as the visible rect moves,
        // but hover correctness should not rest on that: watch the clip view
        // directly so every scroll re-derives hover from the pointer.
        let clip = scroll.contentView();
        clip.setPostsBoundsChangedNotifications(true);
        let scroll_observer = {
            let body = body.clone();
            let clip_for_fades = clip.clone();
            let document = body.clone();
            let top = top_fade.clone();
            let bottom = bottom_fade.clone();
            let handler = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                for child in body.subviews().iter() {
                    if let Some(row) = child.downcast_ref::<MenuBarHoverRow>() {
                        row.sync_hover_to_pointer();
                    }
                }
                let doc_height = document.frame().size.height;
                let clip_height = clip_for_fades.bounds().size.height;
                let top_y = (doc_height - clip_height).max(0.0);
                let y = clip_for_fades.bounds().origin.y;
                top.setHidden(y >= top_y - 0.5);
                bottom.setHidden(y <= 0.5);
            });
            // SAFETY: retained below and removed in `Drop for NativeMenuBar`.
            unsafe {
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(NSViewBoundsDidChangeNotification),
                    Some(&clip),
                    None,
                    &handler,
                )
            }
        };

        let footer_divider = separator(
            rect(ROW_INSET, FOOTER_HEIGHT, POPUP_WIDTH - ROW_INSET * 2.0, 1.0),
            &theme,
            mtm,
        );
        surface.addSubview(&footer_divider);

        let target = MenuBarTarget::new(mtm, panel.clone(), button.clone(), Arc::clone(&store));

        // Everything lives in the chin now, so the brand is the mark alone —
        // the wordmark cost more width than identity here is worth.
        let brand_hit = MenuBarBrandHit::new(
            mtm,
            rect(ROW_INSET, CHIN_CONTROL_Y, BRAND_HIT_WIDTH, 28.0),
            target.clone(),
            &theme.primary,
        );
        // One rasterization, shared by the status item and the panel header:
        // same fixed vector at the same fixed size, only the tint differs.
        let logo = brand_raster::template_diri_logo_ns_image(DIRI_LOGO_HEIGHT as f32);
        let brand_mark = {
            let view = if let Some(image) = &logo {
                NSImageView::imageViewWithImage(image, mtm)
            } else {
                NSImageView::initWithFrame(
                    NSImageView::alloc(mtm),
                    rect(
                        ROW_PAD,
                        (28.0 - DIRI_LOGO_HEIGHT) / 2.0,
                        DIRI_LOGO_WIDTH,
                        DIRI_LOGO_HEIGHT,
                    ),
                )
            };
            view.setFrame(rect(
                ROW_PAD,
                (28.0 - DIRI_LOGO_HEIGHT) / 2.0,
                DIRI_LOGO_WIDTH,
                DIRI_LOGO_HEIGHT,
            ));
            view.setImageAlignment(NSImageAlignment::AlignCenter);
            view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
            view.setContentTintColor(Some(&theme.secondary));
            view
        };
        brand_hit.addSubview(&brand_mark);
        surface.addSubview(&brand_hit);

        let new_agent_chrome = MenuBarHoverChrome::new(
            mtm,
            rect(
                POPUP_WIDTH - ROW_INSET - NEW_AGENT_WIDTH,
                CHIN_CONTROL_Y,
                NEW_AGENT_WIDTH,
                28.0,
            ),
            6.0,
            &theme.primary,
        );
        let new_agent_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("New Agent"),
                Some(&*target as &AnyObject),
                Some(sel!(newAgent:)),
                mtm,
            )
        };
        // Match session/project title weight so the action does not look oversized.
        style_plain_button(&new_agent_button, false, &theme);
        new_agent_button.setImage(Some(&symbol_image("plus", "plus.circle")));
        new_agent_button.setImagePosition(NSCellImagePosition::ImageLeading);
        new_agent_button.setImageHugsTitle(true);
        new_agent_chrome.attach_control(&new_agent_button);
        surface.addSubview(&new_agent_chrome);

        let settings_chrome = MenuBarHoverChrome::new(
            mtm,
            rect(SETTINGS_X, CHIN_CONTROL_Y, SETTINGS_HIT, 28.0),
            6.0,
            &theme.primary,
        );
        let settings_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::new(),
                Some(&*target as &AnyObject),
                Some(sel!(openSettings:)),
                mtm,
            )
        };
        style_plain_button(&settings_button, false, &theme);
        // Sidebar title-bar settings: `sf_symbol("gearshape", 15)` → IconName::Settings SVG.
        if let Some(image) = brand_raster::template_settings_ns_image(16.0) {
            settings_button.setImage(Some(&image));
        } else {
            settings_button.setImage(Some(&glyph_image("gearshape", "gear", GLYPH_SIZE)));
        }
        settings_button.setImagePosition(NSCellImagePosition::ImageOnly);
        settings_button.setToolTip(Some(&NSString::from_str("Settings")));
        settings_chrome.attach_control(&settings_button);
        surface.addSubview(&settings_chrome);

        let quit_chrome = MenuBarHoverChrome::new(
            mtm,
            rect(QUIT_X, CHIN_CONTROL_Y, QUIT_WIDTH, 28.0),
            6.0,
            &theme.primary,
        );
        let quit_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Quit"),
                Some(&*target as &AnyObject),
                Some(sel!(quitDiri:)),
                mtm,
            )
        };
        style_plain_button(&quit_button, false, &theme);
        quit_chrome.attach_control(&quit_button);
        surface.addSubview(&quit_chrome);

        unsafe {
            button.setTarget(Some(&*target as &AnyObject));
            button.setAction(Some(sel!(toggleDiriMenu:)));
        }
        // Template mark set once; attention only retints (same Ink map as row glyphs).
        if let Some(image) = &logo {
            button.setImage(Some(image));
        }
        button.setTitle(&NSString::new());
        button.setImagePosition(NSCellImagePosition::ImageOnly);

        let mut menu_bar = Self {
            store,
            _status_item: status_item,
            button,
            panel,
            surface,
            brand_hit,
            brand_mark,
            new_agent_chrome,
            settings_chrome,
            quit_chrome,
            backdrop,
            footer_divider,
            top_fade,
            bottom_fade,
            scroll,
            body,
            target,
            scroll_observer: Some(scroll_observer),
            last_fingerprint: None,
            last_attention: None,
            theme,
            needs_full_repaint: false,
        };
        menu_bar.set_attention(AttentionLevel::None);
        Some(menu_bar)
    }

    /// Re-resolve the palette when the workbench theme changes, and force a
    /// full repaint: every color in the panel is baked into views at build time.
    fn sync_theme(&mut self, theme_id: &str) {
        if self.theme.id == theme_id {
            return;
        }
        self.theme = MenuTheme::new(theme_id.to_owned(), app_theme::sidebar_colors(theme_id));
        self.brand_mark
            .setContentTintColor(Some(&self.theme.secondary));
        self.backdrop.setFillColor(&self.theme.surface);
        self.panel.setAppearance(self.theme.appearance().as_deref());
        self.footer_divider.setFillColor(&self.theme.separator);
        self.top_fade.set_color(&self.theme.surface);
        self.bottom_fade.set_color(&self.theme.surface);
        self.last_attention = None;
        self.needs_full_repaint = true;
    }

    pub fn refresh(&mut self) {
        // Closed panel: only the status-item tint. Building the inbox first
        // would put the most expensive work on the hottest path.
        if !self.panel.isVisible() {
            let (attention, theme_id) = {
                let store = self.store.read().expect("session store lock poisoned");
                (
                    store.global_attention(),
                    store.preferences().terminal_theme.clone(),
                )
            };
            self.sync_theme(&theme_id);
            self.set_attention(attention);
            self.last_fingerprint = None;
            return;
        }

        let collapsed = self.target.collapsed_projects();
        let (model, selected, attention, theme_id) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let projection = store.menu_bar_projection();
            let model = build_inbox(&projection, &collapsed);
            let selected = store.selected_session_id().cloned();
            let attention = store.global_attention();
            let theme_id = store.preferences().terminal_theme.clone();
            (model, selected, attention, theme_id)
        };
        self.sync_theme(&theme_id);
        self.apply_model(&model, selected.as_ref(), attention);
    }

    fn apply_model(
        &mut self,
        model: &InboxModel,
        selected_session_id: Option<&SessionId>,
        attention: AttentionLevel,
    ) {
        self.set_attention(attention);
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };

        let fingerprint = panel_fingerprint(model, selected_session_id);
        // A theme change repaints without being an open, so the viewport is
        // still restored rather than snapped back to the first project.
        let forced = std::mem::take(&mut self.needs_full_repaint);
        if !forced && self.last_fingerprint == Some(fingerprint) {
            return;
        }
        // `refresh` clears the fingerprint whenever the panel goes away, so a
        // missing one means this is the first paint since it opened — the only
        // time the list should jump back to the first project.
        let opening = self.last_fingerprint.is_none();
        self.last_fingerprint = Some(fingerprint);

        let content_height = content_height_for(model);
        let body_height = content_height.min(MAX_BODY_HEIGHT);
        let height = body_height + FOOTER_HEIGHT;

        // Resizing grows the panel downward from its origin; pin the top-left so
        // it stays hung off the status item as rows come and go. `refresh` only
        // reaches here with the panel visible, so there is nothing to guard.
        let old_top_left = {
            let frame = self.panel.frame();
            NSPoint::new(frame.origin.x, frame.origin.y + frame.size.height)
        };
        self.panel.setContentSize(NSSize::new(POPUP_WIDTH, height));
        self.panel.setFrameTopLeftPoint(old_top_left);
        self.surface.setFrame(rect(0.0, 0.0, POPUP_WIDTH, height));

        self.scroll
            .setFrame(rect(0.0, FOOTER_HEIGHT, POPUP_WIDTH, body_height));
        self.top_fade.setFrame(rect(
            0.0,
            FOOTER_HEIGHT + body_height - FADE_HEIGHT,
            POPUP_WIDTH,
            FADE_HEIGHT,
        ));
        self.bottom_fade
            .setFrame(rect(0.0, FOOTER_HEIGHT, POPUP_WIDTH, FADE_HEIGHT));
        self.brand_hit
            .setFrame(rect(ROW_INSET, CHIN_CONTROL_Y, BRAND_HIT_WIDTH, 28.0));
        self.brand_mark.setFrame(rect(
            ROW_PAD,
            (28.0 - DIRI_LOGO_HEIGHT) / 2.0,
            DIRI_LOGO_WIDTH,
            DIRI_LOGO_HEIGHT,
        ));
        self.settings_chrome
            .setFrame(rect(SETTINGS_X, CHIN_CONTROL_Y, SETTINGS_HIT, 28.0));
        self.quit_chrome
            .setFrame(rect(QUIT_X, CHIN_CONTROL_Y, QUIT_WIDTH, 28.0));
        self.new_agent_chrome.setFrame(rect(
            POPUP_WIDTH - ROW_INSET - NEW_AGENT_WIDTH,
            CHIN_CONTROL_Y,
            NEW_AGENT_WIDTH,
            28.0,
        ));

        self.rebuild_body(model, content_height, selected_session_id, opening, mtm);
    }

    /// Retint the template status-item / header mark. Same Ink map as row
    /// [`glyph_tint`] / sidebar `StatusGlyph` — logo pixels stay fixed.
    fn set_attention(&mut self, attention: AttentionLevel) {
        if self.last_attention == Some(attention) {
            return;
        }
        self.last_attention = Some(attention);

        // Absolute sRGB only, and nil for anything that is not a real signal.
        // A dynamic color like `labelColor` resolves against the app's
        // appearance rather than the menu bar's, so tinting with it paints the
        // mark black on a dark bar — which is what made the working state
        // disappear. Leaving the tint nil lets AppKit render the template the
        // way it renders every other status item.
        let signal_tint = match attention {
            AttentionLevel::NeedsInput => Some(rgba_ns(
                Ink::ATTENTION.r,
                Ink::ATTENTION.g,
                Ink::ATTENTION.b,
                1.0,
            )),
            AttentionLevel::DoneUnseen => {
                Some(rgba_ns(Ink::FRESH.r, Ink::FRESH.g, Ink::FRESH.b, 1.0))
            }
            AttentionLevel::Working
            | AttentionLevel::IdleSeen
            | AttentionLevel::None
            | AttentionLevel::Unknown => None,
        };
        self.button.setContentTintColor(signal_tint.as_deref());

        // The header mark sits inside the panel, where `labelColor` resolves
        // against the right appearance, so it can keep the quieter idle tones.
        let header_tint = signal_tint.unwrap_or_else(|| match attention {
            AttentionLevel::Working => self.theme.primary_alpha(0.82),
            _ => Retained::clone(&self.theme.primary),
        });
        self.brand_mark.setContentTintColor(Some(&header_tint));
    }

    fn rebuild_body(
        &self,
        model: &InboxModel,
        content_height: f64,
        selected_session_id: Option<&SessionId>,
        opening: bool,
        mtm: MainThreadMarker,
    ) {
        // Every state change rebuilds the body, and selecting a row is a state
        // change — so restoring the offset is what keeps a click from throwing
        // the user back to the top of the list.
        let anchor = (!opening).then(|| self.scroll_offset_from_top());
        for child in self.body.subviews().iter() {
            child.removeFromSuperview();
        }
        self.body
            .setFrame(rect(0.0, 0.0, POPUP_WIDTH, content_height));

        if model.rows.is_empty() {
            self.target.set_session_ids(Vec::new());
            self.target.set_project_ids(Vec::new());
            self.add_empty_state(content_height, mtm);
            self.scroll.setDocumentView(Some(&self.body));
            self.restore_scroll(anchor);
            return;
        }

        let session_ids: Vec<SessionId> = model
            .rows
            .iter()
            .filter_map(|row| match row {
                InboxRow::Session(session) => Some(SessionId::new(session.session_id.clone())),
                _ => None,
            })
            .collect();
        let project_ids: Vec<ProjectId> = model
            .rows
            .iter()
            .filter_map(|row| match row {
                InboxRow::Project { id, .. } => Some(ProjectId::new(id.clone())),
                _ => None,
            })
            .collect();
        self.target.set_session_ids(session_ids);
        self.target.set_project_ids(project_ids);

        let mut y = content_height - BODY_PADDING;
        let mut session_tag = 0isize;
        let mut project_tag = 0isize;
        for row in &model.rows {
            match row {
                InboxRow::Project {
                    name, collapsed, ..
                } => {
                    if project_tag > 0 {
                        y -= 6.0;
                    }
                    y -= PROJECT_HEIGHT;
                    self.add_project_header(name, *collapsed, project_tag, y, mtm);
                    project_tag += 1;
                }
                InboxRow::Session(session) => {
                    y -= ROW_HEIGHT;
                    self.add_session_row(
                        session,
                        session_tag,
                        selected_session_id.is_some_and(|id| id.0 == session.session_id),
                        y,
                        mtm,
                    );
                    session_tag += 1;
                }
            }
        }
        self.scroll.setDocumentView(Some(&self.body));
        self.restore_scroll(anchor);
    }

    /// Distance from the top of the list, which is the thing worth preserving:
    /// the document is non-flipped, so a raw `y` would hold position relative to
    /// the *bottom* and drift every time a row is added or removed above.
    fn scroll_offset_from_top(&self) -> f64 {
        let clip = self.scroll.contentView();
        offset_from_top(
            self.body.frame().size.height,
            clip.bounds().size.height,
            clip.bounds().origin.y,
        )
    }

    /// Show each fade only when there is something hidden behind it: a list
    /// short enough to fit needs no dissolve at either end.
    fn sync_fades(&self) {
        let clip = self.scroll.contentView();
        let doc_height = self.body.frame().size.height;
        let clip_height = clip.bounds().size.height;
        let top_y = (doc_height - clip_height).max(0.0);
        let y = clip.bounds().origin.y;
        self.top_fade.setHidden(y >= top_y - 0.5);
        self.bottom_fade.setHidden(y <= 0.5);
    }

    /// `None` pins to the top (the panel just opened); otherwise re-apply the
    /// captured distance, clamped to whatever the rebuilt list can now show.
    fn restore_scroll(&self, from_top: Option<f64>) {
        let clip = self.scroll.contentView();
        let y = clip_y_for_offset(
            self.body.frame().size.height,
            clip.bounds().size.height,
            from_top,
        );
        clip.scrollToPoint(NSPoint::new(0.0, y));
        self.scroll.reflectScrolledClipView(&clip);
        self.sync_fades();
    }

    fn add_project_header(
        &self,
        name: &str,
        collapsed: bool,
        tag: isize,
        y: f64,
        mtm: MainThreadMarker,
    ) {
        let hover_fill = row_fill_box(
            rect(
                ROW_INSET,
                1.0,
                POPUP_WIDTH - ROW_INSET * 2.0,
                PROJECT_HEIGHT - 2.0,
            ),
            HOVER_FILL_ALPHA,
            &self.theme.primary,
            mtm,
        );
        hover_fill.setHidden(true);

        let row = MenuBarHoverRow::new(
            mtm,
            rect(0.0, y, POPUP_WIDTH, PROJECT_HEIGHT),
            self.target.clone(),
            tag,
            false,
            false,
            hover_fill.clone(),
            None,
            None,
        );
        row.addSubview(&hover_fill);

        // NSBox badge with zero content margins so the folder glyph can center.
        let badge = NSBox::initWithFrame(
            NSBox::alloc(mtm),
            rect(
                CONTENT_X,
                (PROJECT_HEIGHT - FOLDER_BADGE) / 2.0,
                FOLDER_BADGE,
                FOLDER_BADGE,
            ),
        );
        badge.setBoxType(NSBoxType::Custom);
        badge.setBorderWidth(0.0);
        badge.setCornerRadius(5.0);
        badge.setContentViewMargins(NSSize::new(0.0, 0.0));
        badge.setFillColor(&self.theme.primary_alpha(0.08));
        let folder = glyph_view(
            "folder.fill",
            "folder",
            GLYPH_BOX_COMPACT,
            &self.theme.secondary,
            rect(0.0, 0.0, FOLDER_BADGE, FOLDER_BADGE),
            mtm,
        );
        // SF Symbol folder.fill carries its mass below the geometric mid;
        // nudge up one point so it reads centered in the 18pt badge.
        let folder_frame = folder.frame();
        folder.setFrameOrigin(NSPoint::new(
            folder_frame.origin.x,
            folder_frame.origin.y + 1.0,
        ));
        badge.addSubview(&folder);
        row.addSubview(&badge);

        let name_x = CONTENT_X + FOLDER_BADGE + ROW_GAP;
        let name = label(
            name,
            13.0,
            FontStyle::Medium,
            &self.theme.primary,
            rect(
                name_x - TEXT_INSET,
                (PROJECT_HEIGHT - 16.0) / 2.0 + 1.0,
                (CONTENT_RIGHT - INDENT - ROW_GAP - name_x).max(24.0),
                16.0,
            ),
            mtm,
        );
        row.addSubview(&name);

        // Collapse affordance in the sidebar's fold column width.
        let chevron = glyph_view(
            if collapsed {
                "chevron.right"
            } else {
                "chevron.down"
            },
            "chevron.right",
            GLYPH_BOX_COMPACT,
            &self.theme.tertiary,
            rect(CONTENT_RIGHT - INDENT, 0.0, INDENT, PROJECT_HEIGHT),
            mtm,
        );
        row.addSubview(&chevron);
        self.body.addSubview(&row);
    }

    fn add_session_row(
        &self,
        session: &InboxSessionRow,
        tag: isize,
        selected: bool,
        y: f64,
        mtm: MainThreadMarker,
    ) {
        // Sidebar row: rails + fold column + gap + 16 glyph + gap + title, all
        // starting at the shared content column.
        let icon_x = CONTENT_X + f64::from(session.depth) * DEPTH_STEP + INDENT + ROW_GAP;
        let title_x = icon_x + GLYPH_SIZE + ROW_GAP;
        let trailing = trailing_label(session, &self.theme);
        // Sidebar flex: title absorbs width; chips then trailing ✕ on hover.
        let chip_width = trailing.as_ref().map_or(0.0, |status| status.width);
        let trailing_slot = if chip_width > 0.0 {
            chip_width + CLOSE_CHIP_GAP + CLOSE_HIT + ROW_GAP
        } else {
            CLOSE_HIT + ROW_GAP
        };
        let title_right = CONTENT_RIGHT - trailing_slot;
        let icon_y = (ROW_HEIGHT - GLYPH_SIZE) / 2.0;
        let close_x = CONTENT_RIGHT - CLOSE_HIT;

        let hover_fill = row_fill_box(
            rect(
                ROW_INSET,
                1.0,
                POPUP_WIDTH - ROW_INSET * 2.0,
                ROW_HEIGHT - 2.0,
            ),
            HOVER_FILL_ALPHA,
            &self.theme.primary,
            mtm,
        );
        hover_fill.setHidden(true);

        let icon_color = glyph_tint(session, &self.theme);
        let agent_mark = agent_mark_view(&session.agent_id, &icon_color, icon_x, mtm);

        let close_button = MenuBarCloseHit::new(
            mtm,
            rect(
                close_x,
                (ROW_HEIGHT - CLOSE_HIT) / 2.0,
                CLOSE_HIT,
                CLOSE_HIT,
            ),
            self.target.clone(),
            tag,
            &self.theme.primary,
            &self.theme.secondary,
        );
        close_button.setHidden(true);

        let mut trailing_chip: Option<Retained<NSBox>> = None;
        if let Some(status) = trailing.as_ref() {
            // Idle position: flush right. On hover, set_hovered slides it left of ✕.
            // `Chip::height()` is the painted height of the sidebar's StateChip,
            // line box included, so this pill needs no fudge to match it.
            let font_size = f64::from(Chip::font_size());
            let chip_h = f64::from(Chip::height());
            let chip = NSBox::initWithFrame(
                NSBox::alloc(mtm),
                rect(
                    CONTENT_RIGHT - status.width,
                    (ROW_HEIGHT - chip_h) / 2.0,
                    status.width,
                    chip_h,
                ),
            );
            chip.setBoxType(NSBoxType::Custom);
            chip.setBorderWidth(0.0);
            chip.setCornerRadius(f64::from(Radius::CHIP));
            chip.setContentViewMargins(NSSize::new(0.0, 0.0));
            chip.setFillColor(&self.theme.primary_alpha(f64::from(Fill::SUBTLE_OPACITY)));
            // Non-flipped: center the label, then +1pt optical (NSTextField sits low).
            let label_h = font_size + 2.0;
            let label_y = ((chip_h - label_h) / 2.0) + 1.0;
            let chip_label = label(
                status.text,
                font_size,
                FontStyle::Medium,
                &status.color,
                rect(0.0, label_y, status.width, label_h),
                mtm,
            );
            chip_label.setAlignment(NSTextAlignment::Center);
            chip.addSubview(&chip_label);
            trailing_chip = Some(chip);
        }

        let row = MenuBarHoverRow::new(
            mtm,
            rect(0.0, y, POPUP_WIDTH, ROW_HEIGHT),
            self.target.clone(),
            tag,
            true,
            selected,
            hover_fill.clone(),
            Some(close_button.clone()),
            trailing_chip.clone(),
        );
        if selected {
            let selected_fill = row_fill_box(
                rect(
                    ROW_INSET,
                    1.0,
                    POPUP_WIDTH - ROW_INSET * 2.0,
                    ROW_HEIGHT - 2.0,
                ),
                SELECTED_FILL_ALPHA,
                &self.theme.primary,
                mtm,
            );
            row.addSubview(&selected_fill);
        }
        row.addSubview(&hover_fill);
        row.addSubview(&agent_mark);
        row.addSubview(&close_button);
        if let Some(chip) = &trailing_chip {
            row.addSubview(chip);
        }

        let title_alpha = if session.trailing == Some(TrailingStatus::Zzz) {
            0.55
        } else {
            0.90
        };
        // Same midY as the glyph; NSTextField draws slightly high, so nudge +1.
        let title_y = icon_y + 1.0;
        let title = label(
            &session.title,
            13.0,
            FontStyle::Regular,
            &self.theme.primary_alpha(title_alpha),
            rect(
                title_x - TEXT_INSET,
                title_y,
                (title_right - title_x).max(24.0),
                GLYPH_SIZE,
            ),
            mtm,
        );
        row.addSubview(&title);
        unsafe {
            let accessibility_label = NSString::from_str(&session.title);
            let _: () = msg_send![&*row, setAccessibilityLabel: &*accessibility_label];
        }
        self.body.addSubview(&row);
    }

    fn add_empty_state(&self, content_height: f64, mtm: MainThreadMarker) {
        let icon = glyph_view(
            "plus.circle",
            "waveform",
            20.0,
            &self.theme.tertiary,
            rect(
                (POPUP_WIDTH - 20.0) / 2.0,
                content_height - 36.0,
                20.0,
                20.0,
            ),
            mtm,
        );
        self.body.addSubview(&icon);

        let title = label(
            "No active sessions",
            13.0,
            FontStyle::Medium,
            &self.theme.secondary,
            rect(24.0, content_height - 58.0, POPUP_WIDTH - 48.0, 18.0),
            mtm,
        );
        title.setAlignment(NSTextAlignment::Center);
        self.body.addSubview(&title);

        let hint = label(
            "Start one from New Agent",
            11.0,
            FontStyle::Regular,
            &self.theme.tertiary,
            rect(24.0, content_height - 76.0, POPUP_WIDTH - 48.0, 16.0),
            mtm,
        );
        hint.setAlignment(NSTextAlignment::Center);
        self.body.addSubview(&hint);
    }
}

struct TrailingLabel {
    text: &'static str,
    width: f64,
    color: Retained<NSColor>,
}

fn trailing_label(session: &InboxSessionRow, theme: &MenuTheme) -> Option<TrailingLabel> {
    match session.trailing? {
        TrailingStatus::NeedsYou => Some(TrailingLabel {
            text: "needs you",
            width: 64.0,
            color: if session.destructive {
                rgba_ns(Ink::DANGER.r, Ink::DANGER.g, Ink::DANGER.b, 1.0)
            } else {
                rgba_ns(Ink::ATTENTION.r, Ink::ATTENTION.g, Ink::ATTENTION.b, 1.0)
            },
        }),
        TrailingStatus::Done => Some(TrailingLabel {
            text: "done",
            width: 36.0,
            color: rgba_ns(Ink::FRESH.r, Ink::FRESH.g, Ink::FRESH.b, 1.0),
        }),
        TrailingStatus::Zzz => Some(TrailingLabel {
            text: "Zzz",
            width: 28.0,
            color: Retained::clone(&theme.tertiary),
        }),
    }
}

fn content_height_for(model: &InboxModel) -> f64 {
    if model.rows.is_empty() {
        return EMPTY_BODY_HEIGHT;
    }
    let mut height = BODY_PADDING * 2.0;
    let mut projects = 0usize;
    for row in &model.rows {
        match row {
            InboxRow::Project { .. } => {
                if projects > 0 {
                    height += 6.0;
                }
                height += PROJECT_HEIGHT;
                projects += 1;
            }
            InboxRow::Session(_) => height += ROW_HEIGHT,
        }
    }
    height
}

fn panel_fingerprint(model: &InboxModel, selected: Option<&SessionId>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for row in &model.rows {
        match row {
            InboxRow::Project {
                id,
                name,
                collapsed,
            } => {
                0u8.hash(&mut hasher);
                id.hash(&mut hasher);
                name.hash(&mut hasher);
                collapsed.hash(&mut hasher);
            }
            InboxRow::Session(session) => {
                1u8.hash(&mut hasher);
                session.session_id.hash(&mut hasher);
                session.title.hash(&mut hasher);
                session.agent_id.hash(&mut hasher);
                session.depth.hash(&mut hasher);
                session.trailing.hash(&mut hasher);
                session.working.hash(&mut hasher);
                session.destructive.hash(&mut hasher);
            }
        }
    }
    selected.map(|id| id.0.as_str()).hash(&mut hasher);
    hasher.finish()
}

fn agent_mark_kind(agent_id: &str) -> Option<BrandMarkKind> {
    match agent_id {
        AgentKind::CLAUDE_CODE_ID => Some(BrandMarkKind::Claude),
        AgentKind::CODEX_ID => Some(BrandMarkKind::OpenAi),
        AgentKind::CURSOR_ID => Some(BrandMarkKind::Cursor),
        AgentKind::GEMINI_ID => Some(BrandMarkKind::Gemini),
        _ => None,
    }
}

fn glyph_tint(session: &InboxSessionRow, theme: &MenuTheme) -> Retained<NSColor> {
    match session.trailing {
        Some(TrailingStatus::NeedsYou) if session.destructive => {
            rgba_ns(Ink::DANGER.r, Ink::DANGER.g, Ink::DANGER.b, 1.0)
        }
        Some(TrailingStatus::NeedsYou) => {
            rgba_ns(Ink::ATTENTION.r, Ink::ATTENTION.g, Ink::ATTENTION.b, 1.0)
        }
        Some(TrailingStatus::Done) => rgba_ns(Ink::FRESH.r, Ink::FRESH.g, Ink::FRESH.b, 1.0),
        Some(TrailingStatus::Zzz) => theme.primary_alpha(0.36),
        None if session.working => match session.agent_id.as_str() {
            AgentKind::CLAUDE_CODE_ID => {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.851, 0.467, 0.341, 0.96)
            }
            AgentKind::GEMINI_ID => {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.306, 0.510, 0.933, 0.96)
            }
            _ => theme.primary_alpha(0.82),
        },
        None => theme.primary_alpha(0.42),
    }
}

fn agent_mark_view(
    agent_id: &str,
    color: &NSColor,
    x: f64,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    let y = (ROW_HEIGHT - GLYPH_SIZE) / 2.0;
    if let Some(kind) = agent_mark_kind(agent_id)
        && let Some(image) = brand_raster::template_ns_image(kind, GLYPH_SIZE as f32)
    {
        let view = NSImageView::imageViewWithImage(&image, mtm);
        view.setFrame(rect(x, y, GLYPH_SIZE, GLYPH_SIZE));
        view.setImageAlignment(NSImageAlignment::AlignCenter);
        view.setContentTintColor(Some(color));
        return Retained::into_super(Retained::into_super(view));
    }
    // Shell / unknown: caret bar matching StatusGlyph::shell_caret. A plain
    // NSView hosts it — an NSImageView here is an NSControl sitting in the glyph
    // column with no image and no action.
    let caret = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        rect(
            GLYPH_SIZE * 0.29,
            GLYPH_SIZE * 0.19,
            GLYPH_SIZE * 0.42,
            GLYPH_SIZE * 0.62,
        ),
    );
    caret.setBoxType(NSBoxType::Custom);
    caret.setBorderWidth(0.0);
    caret.setCornerRadius(1.0);
    caret.setFillColor(color);
    let host = NSView::initWithFrame(NSView::alloc(mtm), rect(x, y, GLYPH_SIZE, GLYPH_SIZE));
    host.addSubview(&caret);
    host
}

fn row_fill_box(
    frame: NSRect,
    alpha: f64,
    tone: &NSColor,
    mtm: MainThreadMarker,
) -> Retained<NSBox> {
    let fill = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    fill.setBoxType(NSBoxType::Custom);
    fill.setBorderWidth(0.0);
    fill.setCornerRadius(6.0);
    fill.setFillColor(&tone.colorWithAlphaComponent(alpha));
    fill
}

fn rgba_ns(r: f32, g: f32, b: f32, a: f32) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(r),
        f64::from(g),
        f64::from(b),
        f64::from(a),
    )
}

#[derive(Clone, Copy)]
enum FontStyle {
    Regular,
    Medium,
}

fn system_font(size: f64, style: FontStyle) -> Retained<NSFont> {
    match style {
        FontStyle::Regular => NSFont::systemFontOfSize(size),
        FontStyle::Medium => NSFont::systemFontOfSize_weight(size, unsafe { NSFontWeightMedium }),
    }
}

fn label(
    text: &str,
    size: f64,
    style: FontStyle,
    color: &NSColor,
    frame: NSRect,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(frame);
    label.setFont(Some(&system_font(size, style)));
    label.setTextColor(Some(color));
    label.setMaximumNumberOfLines(1);
    label.setUsesSingleLineMode(true);
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.setAllowsDefaultTighteningForTruncation(true);
    label
}

fn symbol_image(name: &str, fallback: &str) -> Retained<NSImage> {
    let description = NSString::from_str(name);
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&description),
    )
    .or_else(|| {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(fallback),
            Some(&description),
        )
    })
    .expect("macOS 15 always provides the fallback menu symbols");
    image.setTemplate(true);
    image
}

/// A glyph that fills `box_size`, the way `diri_ui::Icon` draws its 24×24 SVGs.
/// The app maps every legacy symbol point size onto `IconSize`, so an "8.5pt"
/// sidebar glyph is really a 14pt box; sizing an SF Symbol by point size
/// instead leaves it visibly small inside the same slot.
fn glyph_image(name: &str, fallback: &str, box_size: f64) -> Retained<NSImage> {
    let image = symbol_image(name, fallback);
    let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(box_size, unsafe {
        NSFontWeightRegular
    });
    image.imageWithSymbolConfiguration(&config).unwrap_or(image)
}

fn glyph_view(
    name: &str,
    fallback: &str,
    box_size: f64,
    color: &NSColor,
    center_in: NSRect,
    mtm: MainThreadMarker,
) -> Retained<NSImageView> {
    let view = NSImageView::imageViewWithImage(&glyph_image(name, fallback, box_size), mtm);
    view.setFrame(rect(
        center_in.origin.x + (center_in.size.width - box_size) / 2.0,
        center_in.origin.y + (center_in.size.height - box_size) / 2.0,
        box_size,
        box_size,
    ));
    view.setImageAlignment(NSImageAlignment::AlignCenter);
    view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    view.setContentTintColor(Some(color));
    view
}

fn close_xmark_image() -> Retained<NSImage> {
    glyph_image("xmark", "xmark.circle", CLOSE_GLYPH_PT)
}

fn activate_diri(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    // Status-item clicks do not activate the app on their own. Without this,
    // key equivalents keep going to whatever owns the menu bar (e.g. Finder).
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let running = NSRunningApplication::currentApplication();
    #[allow(deprecated)]
    let _ = running.activateWithOptions(
        NSApplicationActivationOptions::ActivateAllWindows
            | NSApplicationActivationOptions::ActivateIgnoringOtherApps,
    );
}

fn separator(frame: NSRect, theme: &MenuTheme, mtm: MainThreadMarker) -> Retained<NSBox> {
    // Custom rather than NSBoxType::Separator: the stock hairline is a system
    // color and would ignore the theme like everything else here used to.
    let separator = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    separator.setBoxType(NSBoxType::Custom);
    separator.setBorderWidth(0.0);
    separator.setFillColor(&theme.separator);
    separator
}

fn style_plain_button(button: &NSButton, prominent: bool, theme: &MenuTheme) {
    button.setFont(Some(&system_font(
        13.0,
        if prominent {
            FontStyle::Medium
        } else {
            FontStyle::Regular
        },
    )));
    button.setBordered(false);
    let tint = if prominent {
        Retained::clone(&theme.primary)
    } else {
        Retained::clone(&theme.secondary)
    };
    button.setContentTintColor(Some(&tint));
    button.setRefusesFirstResponder(true);
    button.setFocusRingType(NSFocusRingType::None);
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

/// Clip origin, in the non-flipped document space the panel scrolls, that is
/// `from_top` points below the first row. `None` means pin to the top.
fn clip_y_for_offset(doc_height: f64, clip_height: f64, from_top: Option<f64>) -> f64 {
    let top_y = (doc_height - clip_height).max(0.0);
    match from_top {
        Some(offset) => (top_y - offset.min(top_y)).max(0.0),
        None => top_y,
    }
}

/// Inverse of [`clip_y_for_offset`]: how far below the first row the viewport sits.
fn offset_from_top(doc_height: f64, clip_height: f64, clip_y: f64) -> f64 {
    ((doc_height - clip_height).max(0.0) - clip_y).max(0.0)
}

fn point_in_rect(point: NSPoint, frame: NSRect) -> bool {
    point.x >= frame.origin.x
        && point.x < frame.origin.x + frame.size.width
        && point.y >= frame.origin.y
        && point.y < frame.origin.y + frame.size.height
}

/// Keys the panel claims while it is open. ⇧⌘T is deliberately absent: the app
/// binds it to `ReopenSession`, so it falls through untouched.
fn menu_shortcut_from_event(event: &NSEvent) -> Option<MenuShortcut> {
    let modifiers = event.modifierFlags();
    if !modifiers.contains(NSEventModifierFlags::Command) {
        return None;
    }
    let shift = modifiers.contains(NSEventModifierFlags::Shift);
    let option = modifiers.contains(NSEventModifierFlags::Option);
    let key = event
        .charactersIgnoringModifiers()
        .map(|chars| chars.to_string().to_ascii_lowercase())
        .unwrap_or_default();
    match key.as_str() {
        "t" if option => Some(MenuShortcut::SpawnShell),
        "t" if !shift => Some(MenuShortcut::SpawnDefault),
        "n" if shift => Some(MenuShortcut::SpawnCodex),
        // Matches the app's own `cmd-n` → OpenLauncher binding.
        "n" => Some(MenuShortcut::NewAgent),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum MenuShortcut {
    NewAgent,
    SpawnDefault,
    SpawnShell,
    SpawnCodex,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FadeEdge {
    Top,
    Bottom,
}

struct MenuBarFadeIvars {
    edge: FadeEdge,
    color: RefCell<Retained<NSColor>>,
}

define_class!(
    // SAFETY: NSView requires MainThreadOnly; this class does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarFade"]
    #[ivars = MenuBarFadeIvars]
    struct MenuBarFade;

    impl MenuBarFade {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let ivars = self.ivars();
            let color = ivars.color.borrow();
            let clear = color.colorWithAlphaComponent(0.0);
            // Opaque against the bar, clear against the list, so a row leaves by
            // dissolving into the panel instead of being cut off mid-glyph.
            let (start, end) = match ivars.edge {
                FadeEdge::Top => (clear, Retained::clone(&color)),
                FadeEdge::Bottom => (Retained::clone(&color), clear),
            };
            let Some(gradient) =
                NSGradient::initWithStartingColor_endingColor(NSGradient::alloc(), &start, &end)
            else {
                return;
            };
            // 90° draws bottom-to-top in this non-flipped view.
            gradient.drawInRect_angle(self.bounds(), 90.0);
        }

        /// Purely decorative: clicks belong to the row underneath.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

impl MenuBarFade {
    fn new(mtm: MainThreadMarker, edge: FadeEdge, color: &NSColor) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuBarFadeIvars {
            edge,
            color: RefCell::new(color.retain()),
        });
        // SAFETY: designated NSView initializer.
        let this: Retained<Self> = unsafe {
            msg_send![super(this), initWithFrame: rect(0.0, 0.0, POPUP_WIDTH, FADE_HEIGHT)]
        };
        this.setHidden(true);
        this
    }

    fn set_color(&self, color: &NSColor) {
        self.ivars().color.replace(color.retain());
        self.setNeedsDisplay(true);
    }
}

struct MenuBarHoverChromeIvars {
    hover_fill: Retained<NSBox>,
}

define_class!(
    // SAFETY: NSView requires MainThreadOnly; this class does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarHoverChrome"]
    #[ivars = MenuBarHoverChromeIvars]
    struct MenuBarHoverChrome;

    impl MenuBarHoverChrome {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::ActiveAlways
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self as &AnyObject),
                    None,
                )
            };
            self.addTrackingArea(&area);
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(false);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(true);
        }
    }
);

impl MenuBarHoverChrome {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        corner_radius: f64,
        tone: &NSColor,
    ) -> Retained<Self> {
        let hover_fill = row_fill_box(
            rect(0.0, 0.0, frame.size.width, frame.size.height),
            HOVER_FILL_ALPHA,
            tone,
            mtm,
        );
        hover_fill.setCornerRadius(corner_radius);
        hover_fill.setHidden(true);
        hover_fill.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let this = Self::alloc(mtm).set_ivars(MenuBarHoverChromeIvars {
            hover_fill: hover_fill.clone(),
        });
        // SAFETY: designated NSView initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.addSubview(&hover_fill);
        this
    }

    fn attach_control(&self, control: &NSView) {
        let bounds = self.bounds();
        control.setFrame(rect(0.0, 0.0, bounds.size.width, bounds.size.height));
        control.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        self.addSubview(control);
    }
}

struct MenuBarBrandHitIvars {
    target: Retained<MenuBarTarget>,
    hover_fill: Retained<NSBox>,
}

define_class!(
    // SAFETY: NSView requires MainThreadOnly; this class does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarBrandHit"]
    #[ivars = MenuBarBrandHitIvars]
    struct MenuBarBrandHit;

    impl MenuBarBrandHit {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::ActiveAlways
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self as &AnyObject),
                    None,
                )
            };
            self.addTrackingArea(&area);
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(false);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if event.clickCount() < 1 {
                return;
            }
            self.ivars().target.show_main_window();
        }
    }
);

impl MenuBarBrandHit {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        target: Retained<MenuBarTarget>,
        tone: &NSColor,
    ) -> Retained<Self> {
        let hover_fill = row_fill_box(
            rect(0.0, 0.0, frame.size.width, frame.size.height),
            HOVER_FILL_ALPHA,
            tone,
            mtm,
        );
        hover_fill.setCornerRadius(6.0);
        hover_fill.setHidden(true);
        hover_fill.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let this = Self::alloc(mtm).set_ivars(MenuBarBrandHitIvars {
            target,
            hover_fill: hover_fill.clone(),
        });
        // SAFETY: designated NSView initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        unsafe {
            let accessibility_label = NSString::from_str("Open diri");
            let _: () = msg_send![&*this, setAccessibilityLabel: &*accessibility_label];
        }
        this.addSubview(&hover_fill);
        this
    }
}

struct MenuBarCloseHitIvars {
    target: Retained<MenuBarTarget>,
    tag: isize,
    hover_fill: Retained<NSBox>,
}

define_class!(
    // SAFETY: NSView requires MainThreadOnly; this class does not implement Drop.
    // Sidebar close control: 16×16, Radius::CHIP, secondary ✕, Fill::subtle on hover.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarCloseHit"]
    #[ivars = MenuBarCloseHitIvars]
    struct MenuBarCloseHit;

    impl MenuBarCloseHit {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::ActiveAlways
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self as &AnyObject),
                    None,
                )
            };
            self.addTrackingArea(&area);
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(false);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover_fill.setHidden(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if event.clickCount() < 1 {
                return;
            }
            let tag = self.ivars().tag;
            self.ivars().target.close_session_tag(tag);
        }
    }
);

impl MenuBarCloseHit {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        target: Retained<MenuBarTarget>,
        tag: isize,
        tone: &NSColor,
        tint: &NSColor,
    ) -> Retained<Self> {
        let hover_fill = row_fill_box(
            rect(0.0, 0.0, CLOSE_HIT, CLOSE_HIT),
            HOVER_FILL_ALPHA,
            tone,
            mtm,
        );
        hover_fill.setCornerRadius(CLOSE_RADIUS);
        hover_fill.setHidden(true);

        let this = Self::alloc(mtm).set_ivars(MenuBarCloseHitIvars {
            target,
            tag,
            hover_fill: hover_fill.clone(),
        });
        // SAFETY: designated NSView initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        unsafe {
            let accessibility_label = NSString::from_str("Close session");
            let _: () = msg_send![&*this, setAccessibilityLabel: &*accessibility_label];
        }
        this.addSubview(&hover_fill);
        let glyph = NSImageView::imageViewWithImage(&close_xmark_image(), mtm);
        glyph.setFrame(rect(0.0, 0.0, CLOSE_HIT, CLOSE_HIT));
        glyph.setImageAlignment(NSImageAlignment::AlignCenter);
        glyph.setContentTintColor(Some(tint));
        this.addSubview(&glyph);
        this
    }

    fn clear_hover_fill(&self) {
        self.ivars().hover_fill.setHidden(true);
    }
}

struct MenuBarHoverRowIvars {
    target: Retained<MenuBarTarget>,
    tag: isize,
    is_session: bool,
    selected: bool,
    hover_fill: Retained<NSBox>,
    close_button: Option<Retained<MenuBarCloseHit>>,
    trailing_chip: Option<Retained<NSBox>>,
}

define_class!(
    // SAFETY: NSView requires MainThreadOnly; this class does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarHoverRow"]
    #[ivars = MenuBarHoverRowIvars]
    struct MenuBarHoverRow;

    impl MenuBarHoverRow {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            // SAFETY: owner is self; tracking area is retained by the view.
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::ActiveAlways
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self as &AnyObject),
                    None,
                )
            };
            self.addTrackingArea(&area);
            // Scrolling slides rows under a stationary cursor. AppKit
            // synthesizes mouseEntered: for each row that passes beneath it but
            // does not reliably pair those with mouseExited:, so hover state
            // accumulates and every scrolled-past row stays filled. Re-derive it
            // from where the pointer actually is instead of trusting the events.
            self.sync_hover_to_pointer();
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            // Clearing siblings here is what makes "at most one row is filled"
            // an invariant instead of a consequence of AppKit pairing every
            // enter with an exit — which it does not do while the list scrolls
            // under a stationary pointer. Re-deriving from the pointer on scroll
            // was not enough on its own; this holds even if that never runs.
            // SAFETY: read-only walk of the row's siblings on the main thread.
            if let Some(parent) = unsafe { self.superview() } {
                for sibling in parent.subviews().iter() {
                    if let Some(row) = sibling.downcast_ref::<MenuBarHoverRow>()
                        && !std::ptr::eq(row, self)
                    {
                        row.set_hovered(false);
                    }
                }
            }
            self.set_hovered(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_hovered(false);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if event.clickCount() < 1 {
                return;
            }
            let ivars = self.ivars();
            if ivars.is_session {
                ivars.target.select_session_tag(ivars.tag);
            } else {
                ivars.target.toggle_project_tag(ivars.tag);
            }
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            // Middle button closes the hovered session, matching the sidebar.
            if event.buttonNumber() != 2 || !self.ivars().is_session {
                return;
            }
            let tag = self.ivars().tag;
            self.ivars().target.close_session_tag(tag);
        }
    }
);

impl MenuBarHoverRow {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        target: Retained<MenuBarTarget>,
        tag: isize,
        is_session: bool,
        selected: bool,
        hover_fill: Retained<NSBox>,
        close_button: Option<Retained<MenuBarCloseHit>>,
        trailing_chip: Option<Retained<NSBox>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuBarHoverRowIvars {
            target,
            tag,
            is_session,
            selected,
            hover_fill,
            close_button,
            trailing_chip,
        });
        // SAFETY: designated NSView initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// True when the pointer is over the part of this row the clip view is
    /// actually showing. `visibleRect` is what makes a row scrolled off the top
    /// or bottom of the panel stop counting as hovered.
    fn pointer_is_inside(&self) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        let on_screen = NSRect::new(NSEvent::mouseLocation(), NSSize::new(0.0, 0.0));
        let in_window = window.convertRectFromScreen(on_screen).origin;
        let point = self.convertPoint_fromView(in_window, None);
        point_in_rect(point, self.visibleRect())
    }

    fn sync_hover_to_pointer(&self) {
        self.set_hovered(self.pointer_is_inside());
    }

    fn set_hovered(&self, hovered: bool) {
        let ivars = self.ivars();
        ivars.hover_fill.setHidden(!hovered || ivars.selected);
        if let Some(close_button) = &ivars.close_button {
            close_button.setHidden(!hovered);
            if !hovered {
                close_button.clear_hover_fill();
            }
        }
        // Sidebar keeps chips visible; ✕ sits to their right on hover.
        if let Some(chip) = &ivars.trailing_chip {
            let width = chip.frame().size.width;
            let y = chip.frame().origin.y;
            let x = if hovered {
                CONTENT_RIGHT - CLOSE_HIT - CLOSE_CHIP_GAP - width
            } else {
                CONTENT_RIGHT - width
            };
            chip.setFrameOrigin(NSPoint::new(x, y));
        }
    }
}

define_class!(
    // SAFETY: NSPanel requires MainThreadOnly; MenuBarPanel does not implement Drop.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriMenuBarPanel"]
    #[ivars = ()]
    struct MenuBarPanel;

    impl MenuBarPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
    }
);

impl MenuBarPanel {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // NonactivatingPanel is what Swift menu-bar apps use: the panel takes key
        // input without activating the app, so clicking the status item never
        // brings the workbench windows forward.
        // SAFETY: designated NSWindow initializer.
        unsafe {
            msg_send![
                super(this),
                initWithContentRect: rect(0.0, 0.0, POPUP_WIDTH, 140.0),
                styleMask: NSWindowStyleMask::Borderless
                    | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        }
    }
}

struct MenuBarTargetIvars {
    panel: Retained<MenuBarPanel>,
    button: Retained<NSStatusBarButton>,
    store: Arc<RwLock<SessionStore>>,
    session_ids: RefCell<Vec<SessionId>>,
    project_ids: RefCell<Vec<ProjectId>>,
    collapsed_projects: RefCell<HashSet<String>>,
    /// Outside clicks in other apps.
    global_dismiss_monitor: RefCell<Option<Retained<AnyObject>>>,
    /// Outside clicks in diri's own windows (global monitors never see these).
    local_dismiss_monitor: RefCell<Option<Retained<AnyObject>>>,
    key_monitor: RefCell<Option<Retained<AnyObject>>>,
    /// Another app coming forward (⌘Tab, Dock, Spotlight). Mouse monitors never
    /// see those, so without this the panel floats over whatever you switched to.
    workspace_observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. AppKit invokes these
    // control actions on the main thread and the class does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuBarTargetIvars]
    struct MenuBarTarget;

    unsafe impl NSObjectProtocol for MenuBarTarget {}

    impl MenuBarTarget {
        #[unsafe(method(toggleDiriMenu:))]
        fn toggle_menu(&self, _sender: Option<&AnyObject>) {
            let panel = &self.ivars().panel;
            if panel.isVisible() {
                self.hide_panel();
                return;
            }
            self.show_panel();
        }

        #[unsafe(method(openDiri:))]
        fn open_diri(&self, _sender: Option<&AnyObject>) {
            self.show_main_window();
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.ivars()
                .store
                .write()
                .expect("session store lock poisoned")
                .request_open_settings();
            self.show_main_window();
        }

        #[unsafe(method(newAgent:))]
        fn new_agent(&self, _sender: Option<&AnyObject>) {
            self.ivars()
                .store
                .write()
                .expect("session store lock poisoned")
                .request_open_launcher();
            self.show_main_window();
        }

        // Row select / close / project collapse deliberately have no selector:
        // rows are custom views that call the `*_tag` helpers directly from
        // mouseUp. `define_class!` hides unused methods from the dead-code lint,
        // so these have to be kept out by hand.

        #[unsafe(method(quitDiri:))]
        fn quit_diri(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl MenuBarTarget {
    fn new(
        mtm: MainThreadMarker,
        panel: Retained<MenuBarPanel>,
        button: Retained<NSStatusBarButton>,
        store: Arc<RwLock<SessionStore>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuBarTargetIvars {
            panel,
            button,
            store,
            session_ids: RefCell::new(Vec::new()),
            project_ids: RefCell::new(Vec::new()),
            collapsed_projects: RefCell::new(HashSet::new()),
            global_dismiss_monitor: RefCell::new(None),
            local_dismiss_monitor: RefCell::new(None),
            key_monitor: RefCell::new(None),
            workspace_observer: RefCell::new(None),
        });
        // SAFETY: NSObject's init is its designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn collapsed_projects(&self) -> HashSet<String> {
        self.ivars().collapsed_projects.borrow().clone()
    }

    fn select_session_tag(&self, tag: isize) {
        let Some(id) = self.ivars().session_ids.borrow().get(tag as usize).cloned() else {
            return;
        };
        self.ivars()
            .store
            .write()
            .expect("session store lock poisoned")
            .select(id);
        self.show_main_window();
    }

    fn close_session_tag(&self, tag: isize) {
        let Some(id) = self.ivars().session_ids.borrow().get(tag as usize).cloned() else {
            return;
        };
        let needs_confirm = {
            let mut store = self
                .ivars()
                .store
                .write()
                .expect("session store lock poisoned");
            store.request_close(vec![id]);
            let pending = store.pending_close().is_some();
            store.request_snapshot_publish();
            pending
        };
        if needs_confirm {
            self.show_main_window();
        }
    }

    fn toggle_project_tag(&self, tag: isize) {
        let Some(id) = self.ivars().project_ids.borrow().get(tag as usize).cloned() else {
            return;
        };
        {
            let mut collapsed = self.ivars().collapsed_projects.borrow_mut();
            if !collapsed.remove(id.0.as_str()) {
                collapsed.insert(id.0.clone());
            }
        }
        self.ivars()
            .store
            .write()
            .expect("session store lock poisoned")
            .request_snapshot_publish();
    }

    fn set_session_ids(&self, ids: Vec<SessionId>) {
        self.ivars().session_ids.replace(ids);
    }

    fn set_project_ids(&self, ids: Vec<ProjectId>) {
        self.ivars().project_ids.replace(ids);
    }

    fn show_panel(&self) {
        self.ivars()
            .store
            .write()
            .expect("session store lock poisoned")
            .request_snapshot_publish();

        if let Some(window) = self.ivars().button.window() {
            let button_rect = self
                .ivars()
                .button
                .convertRect_toView(self.ivars().button.bounds(), None);
            let screen_rect = window.convertRectToScreen(button_rect);
            self.ivars().panel.setFrameTopLeftPoint(NSPoint::new(
                screen_rect.origin.x + (screen_rect.size.width - POPUP_WIDTH) / 2.0,
                screen_rect.origin.y - 4.0,
            ));
        }

        self.ivars().panel.makeKeyAndOrderFront(None);
        self.install_monitors();
    }

    fn hide_panel(&self) {
        self.remove_monitors();
        self.ivars().panel.orderOut(None);
    }

    fn install_monitors(&self) {
        self.remove_monitors();

        // Blocks hold a strong reference rather than a raw pointer: a global
        // monitor is process-wide and outlives whatever installed it, so a bare
        // `*const Self` would dangle if `NativeMenuBar` were ever dropped with
        // the panel open. The cycle it creates (target → block → target) is cut
        // by `remove_monitors`, which runs on hide and on drop.
        let this = self.retain();
        let panel = self.ivars().panel.clone();
        let button = self.ivars().button.clone();
        let should_dismiss = move || {
            let location = NSEvent::mouseLocation();
            if point_in_rect(location, panel.frame()) {
                return false;
            }
            if let Some(window) = button.window() {
                let button_rect = button.convertRect_toView(button.bounds(), None);
                let screen_rect = window.convertRectToScreen(button_rect);
                if point_in_rect(location, screen_rect) {
                    return false;
                }
            }
            true
        };

        let global_dismiss = RcBlock::new({
            let should_dismiss = should_dismiss.clone();
            let this = this.clone();
            move |_event: NonNull<NSEvent>| {
                if should_dismiss() {
                    this.hide_panel();
                }
            }
        });
        let global_dismiss_monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            &global_dismiss,
        );

        // Global monitors never see clicks delivered to this process. Without a
        // local twin, the panel stays open when the user clicks back into diri.
        let local_dismiss_monitor = unsafe {
            let local_dismiss = RcBlock::new({
                let this = this.clone();
                move |event: NonNull<NSEvent>| -> *mut NSEvent {
                    if should_dismiss() {
                        this.hide_panel();
                    }
                    event.as_ptr()
                }
            });
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
                &local_dismiss,
            )
        };

        let store = Arc::clone(&self.ivars().store);
        let panel_for_keys = self.ivars().panel.clone();
        // SAFETY: local monitor may swallow handled key events by returning null.
        let key_monitor = unsafe {
            let key_handler = RcBlock::new({
                let this = this.clone();
                move |event: NonNull<NSEvent>| -> *mut NSEvent {
                    if !panel_for_keys.isVisible() {
                        return event.as_ptr();
                    }
                    let event_ref = event.as_ref();
                    let chars = event_ref
                        .charactersIgnoringModifiers()
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    if chars == "\u{1b}" {
                        this.hide_panel();
                        return std::ptr::null_mut();
                    }
                    let Some(shortcut) = menu_shortcut_from_event(event_ref) else {
                        return event.as_ptr();
                    };
                    match shortcut {
                        // The panel owns key status while it is open, so gpui's
                        // own ⌘N binding never fires; route it to the same place
                        // the header's New Agent button goes.
                        MenuShortcut::NewAgent => {
                            store
                                .write()
                                .expect("session store lock poisoned")
                                .request_open_launcher();
                            this.show_main_window();
                        }
                        MenuShortcut::SpawnDefault
                        | MenuShortcut::SpawnShell
                        | MenuShortcut::SpawnCodex => {
                            let mut store = store.write().expect("session store lock poisoned");
                            match shortcut {
                                MenuShortcut::SpawnDefault => {
                                    store.spawn_default(SpawnOptions::default());
                                }
                                MenuShortcut::SpawnShell => {
                                    store.spawn_shell(SpawnOptions::default());
                                }
                                _ => store.spawn_kind(AgentKind::CODEX, SpawnOptions::default()),
                            }
                            store.request_snapshot_publish();
                        }
                    }
                    std::ptr::null_mut()
                }
            });
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown,
                &key_handler,
            )
        };

        // ⌘Tab, Spotlight, or a Dock click brings another app forward without
        // ever producing a click the mouse monitors can see.
        let workspace_observer = {
            let this = this.clone();
            let handler = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                this.hide_panel();
            });
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            // SAFETY: the observer is retained here and removed in `remove_monitors`.
            unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(NSWorkspaceDidActivateApplicationNotification),
                    None,
                    None,
                    &handler,
                )
            }
        };

        self.ivars()
            .global_dismiss_monitor
            .replace(global_dismiss_monitor);
        self.ivars()
            .local_dismiss_monitor
            .replace(local_dismiss_monitor);
        self.ivars().key_monitor.replace(key_monitor);
        self.ivars()
            .workspace_observer
            .replace(Some(workspace_observer));
    }

    fn remove_monitors(&self) {
        if let Some(monitor) = self.ivars().global_dismiss_monitor.take() {
            // SAFETY: monitor came from addGlobalMonitorForEventsMatchingMask.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        if let Some(monitor) = self.ivars().local_dismiss_monitor.take() {
            // SAFETY: monitor came from addLocalMonitorForEventsMatchingMask.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        if let Some(monitor) = self.ivars().key_monitor.take() {
            // SAFETY: monitor came from addLocalMonitorForEventsMatchingMask.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        if let Some(observer) = self.ivars().workspace_observer.take() {
            // SAFETY: observer came from addObserverForName on this same center,
            // and nothing else holds it once it is taken out of the ivar.
            unsafe {
                NSWorkspace::sharedWorkspace()
                    .notificationCenter()
                    .removeObserver(observer.as_ref());
            }
        }
    }

    fn show_main_window(&self) {
        self.hide_panel();
        activate_diri(self.mtm());
        let app = NSApplication::sharedApplication(self.mtm());
        for window in app.windows().iter() {
            if window.canBecomeMainWindow() {
                window.makeKeyAndOrderFront(None);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_ui::{Metrics, Space};

    /// Selecting a row rebuilds the body, so without an anchor every click threw
    /// the list back to the first project.
    #[test]
    fn rebuilding_the_list_keeps_the_viewport_where_the_user_left_it() {
        let (doc, clip) = (400.0, 360.0);
        let resting = clip_y_for_offset(doc, clip, None);
        assert_eq!(
            resting, 40.0,
            "no anchor pins to the top of a scrollable list"
        );

        // Scrolled 25pt down, then rebuilt with the same content.
        let scrolled = 15.0;
        let offset = offset_from_top(doc, clip, scrolled);
        assert_eq!(offset, 25.0);
        assert_eq!(clip_y_for_offset(doc, clip, Some(offset)), scrolled);
    }

    #[test]
    fn a_shrinking_list_clamps_instead_of_scrolling_past_its_own_end() {
        // Parked 100pt down, then all but one row goes away.
        let offset = offset_from_top(600.0, 360.0, 140.0);
        assert_eq!(offset, 100.0);
        assert_eq!(clip_y_for_offset(380.0, 360.0, Some(offset)), 0.0);

        // Content shorter than the viewport has nowhere to scroll at all.
        assert_eq!(clip_y_for_offset(120.0, 360.0, Some(offset)), 0.0);
        assert_eq!(offset_from_top(120.0, 360.0, 0.0), 0.0);
    }

    /// The panel used to paint itself out of `NSColor`'s label family, which
    /// tracks the macOS appearance rather than the workbench theme — so a
    /// Dracula or Solarized diri handed its menu a stock grey panel.
    #[test]
    fn the_panel_palette_follows_the_workbench_theme() {
        let dracula = MenuTheme::new("dracula".to_owned(), app_theme::sidebar_colors("dracula"));
        let solarized = MenuTheme::new(
            "solarized-dark".to_owned(),
            app_theme::sidebar_colors("solarized-dark"),
        );
        assert_ne!(
            app_theme::sidebar_colors("dracula").floating_surface(),
            app_theme::sidebar_colors("solarized-dark").floating_surface(),
            "two themes must not resolve to the same panel surface"
        );
        assert!(dracula.is_dark && solarized.is_dark);

        // Light themes have to carry the appearance too, or AppKit keeps
        // drawing a dark scroller over a light panel.
        let light = MenuTheme::new(
            "dirijor-light".to_owned(),
            app_theme::sidebar_colors("dirijor-light"),
        );
        assert!(!light.is_dark);
    }

    /// The panel hand-places what the sidebar lays out with flexbox, so the
    /// columns silently drift the moment a sidebar token changes.
    #[test]
    fn row_columns_match_the_sidebar() {
        assert_eq!(ROW_INSET, f64::from(Space::INSET));
        assert_eq!(ROW_PAD, f64::from(Space::ROW_H));
        assert_eq!(INDENT, f64::from(Space::INDENT));
        assert_eq!(ROW_HEIGHT, f64::from(Metrics::ROW_HEIGHT));
        assert_eq!(PROJECT_HEIGHT, f64::from(Metrics::ROW_HEIGHT));

        // Sidebar project header: INSET | ROW_H | badge | gap | name.
        assert_eq!(CONTENT_X, 18.0);
        assert_eq!(CONTENT_X + FOLDER_BADGE + ROW_GAP, 44.0);

        // Sidebar session row: content column | fold slot | gap | glyph | gap | title.
        let icon_x = |depth: f64| CONTENT_X + depth * DEPTH_STEP + INDENT + ROW_GAP;
        assert_eq!(icon_x(0.0), 38.0);
        assert_eq!(icon_x(0.0) + GLYPH_SIZE + ROW_GAP, 62.0);
        assert_eq!(
            icon_x(1.0) - icon_x(0.0),
            f64::from(Space::INDENT) + ROW_GAP
        );
    }
}
