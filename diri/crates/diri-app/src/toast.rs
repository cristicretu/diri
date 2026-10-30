//! The window's one transient notice primitive. Every toast — terminal
//! feedback, workspace rejections, Engine summaries, connection recovery —
//! is a [`Toast`]: one plain sentence, an optional second line, a tone and
//! at most two inline text actions. [`ToastSlot`] owns lifetime policy
//! (replacement, auto-dismiss per tone, hover-to-pause) as pure state so it
//! is testable without a window; [`toast_element`] paints it.

use std::time::Duration;

use diri_ui::{FloatingSurface, Ink, Radius, SemanticColors, Typo, rgba_f32};
use gpui::{
    Animation, AnimationExt, AnyElement, App, FontWeight, MouseButton, Rgba, SharedString, Window,
    div, ease_out_quint, prelude::*, px,
};

use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastTone {
    /// Plain confirmation or guidance. No glyph.
    Info,
    Success,
    Warning,
    Error,
    /// Something is in flight (connecting, retrying).
    Progress,
}

/// What an inline toast action does. `RootView` performs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastCommand {
    OpenPrivacySettings,
    /// Settings › Agents, where computer use is set up.
    OpenComputerSetup,
    RetryConnection,
    RetryAction,
    CopyDetails(String),
    /// Put back the terminal settings an import replaced.
    UndoTerminalImport,
    /// Never show the ⌘B-in-a-note hint again.
    DismissBoldHint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastAction {
    pub label: &'static str,
    pub command: ToastCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// One short sentence. Always present; never a category like "Terminal".
    pub message: String,
    /// Optional second line for runtime detail (an error, a host name).
    pub detail: Option<String>,
    pub tone: ToastTone,
    pub actions: Vec<ToastAction>,
    /// Persistent notices (connection recovery) stay until their state
    /// clears and may refuse a manual dismiss.
    pub dismissible: bool,
    /// Overrides the tone's default lifetime.
    pub hold: Option<Duration>,
    /// An ambient notice (an Agent changed the window): it never takes the
    /// pointer, offers no ✕, and a newer ambient notice rewrites its text in
    /// place instead of entering again.
    pub ambient: bool,
}

/// How long an ambient notice stays: long enough to read a few words.
pub const AMBIENT_HOLD: Duration = Duration::from_millis(2_500);

impl Toast {
    pub fn new(tone: ToastTone, message: impl Into<String>) -> Self {
        Self {
            message: sanitize(&message.into(), 160),
            detail: None,
            tone,
            actions: Vec::new(),
            dismissible: true,
            hold: None,
            ambient: false,
        }
    }

    /// An ambient notice: who changed the window, and how.
    pub fn ambient(message: impl Into<String>) -> Self {
        Self {
            dismissible: false,
            hold: Some(AMBIENT_HOLD),
            ambient: true,
            ..Self::new(ToastTone::Info, message)
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(ToastTone::Info, message)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(ToastTone::Success, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(ToastTone::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(ToastTone::Error, message)
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = sanitize(&detail.into(), 240);
        self.detail = (!detail.is_empty()).then_some(detail);
        self
    }

    #[must_use]
    pub fn action(mut self, label: &'static str, command: ToastCommand) -> Self {
        self.actions.push(ToastAction { label, command });
        self
    }

    #[must_use]
    pub const fn hold(mut self, hold: Duration) -> Self {
        self.hold = Some(hold);
        self
    }

    #[must_use]
    pub const fn persistent(mut self, dismissible: bool) -> Self {
        self.dismissible = dismissible;
        self.hold = Some(Duration::MAX);
        self
    }

    /// How long the toast stays up without a hover. Errors and anything
    /// that offers an action stay long enough to read and reach; a detail
    /// line buys a little more time. `None` = until its state clears.
    #[must_use]
    pub fn visible_for(&self) -> Option<Duration> {
        if let Some(hold) = self.hold {
            return (hold != Duration::MAX).then_some(hold);
        }
        let base = match self.tone {
            ToastTone::Info | ToastTone::Success => 4_000,
            ToastTone::Progress => 5_000,
            ToastTone::Warning => 7_000,
            ToastTone::Error => 8_000,
        };
        let detail = if self.detail.is_some() { 1_500 } else { 0 };
        let floor = if self.actions.is_empty() { 0 } else { 8_000 };
        Some(Duration::from_millis((base + detail).max(floor)))
    }
}

/// Toast copy is authored in code but may embed runtime text (an error or a
/// file name). Keep it to one visual paragraph.
fn sanitize(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit));
    let mut count = 0;
    let mut space = false;
    for character in text.trim().chars() {
        if character.is_whitespace() || character.is_control() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
            count += 1;
        }
        space = false;
        if count == limit {
            out.push('…');
            break;
        }
        out.push(character);
        count += 1;
    }
    out
}

/// One transient toast at a time; a newer toast replaces the older one.
/// The generation keys the entry animation and every timer, so a stale timer
/// can never close a newer toast.
#[derive(Default)]
pub struct ToastSlot {
    toast: Option<Toast>,
    generation: u64,
    timer: u64,
    hovered: bool,
    expired_while_hovered: bool,
}

/// What `RootView` must schedule after a slot change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToastTimer {
    pub token: u64,
    pub after: Duration,
}

/// How long a toast lingers after the pointer leaves it.
pub const HOVER_GRACE: Duration = Duration::from_millis(1_500);

impl ToastSlot {
    pub fn current(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Show `toast`, replacing any current one. Repeating the visible toast
    /// only restarts its timer: no second entrance, no flicker. An ambient
    /// notice replacing an ambient notice keeps the capsule and swaps its
    /// text, so a burst reads as one notice that updates.
    pub fn show(&mut self, toast: Toast) -> Option<ToastTimer> {
        let in_place = toast.ambient && self.toast.as_ref().is_some_and(|current| current.ambient);
        if !in_place && self.toast.as_ref() != Some(&toast) {
            self.generation = self.generation.wrapping_add(1);
            self.hovered = false;
        }
        self.expired_while_hovered = false;
        let after = toast.visible_for();
        self.toast = Some(toast);
        self.arm(after)
    }

    pub fn dismiss(&mut self) {
        self.toast = None;
        self.hovered = false;
        self.expired_while_hovered = false;
        self.timer = self.timer.wrapping_add(1);
    }

    /// A timer fired. Returns true when the toast closed.
    pub fn expire(&mut self, token: u64) -> bool {
        if token != self.timer || self.toast.is_none() {
            return false;
        }
        if self.hovered {
            self.expired_while_hovered = true;
            return false;
        }
        self.dismiss();
        true
    }

    /// Hovering pauses the clock; leaving re-arms a short grace if the
    /// toast's time ran out underneath the pointer.
    pub fn set_hovered(&mut self, hovered: bool) -> Option<ToastTimer> {
        self.hovered = hovered;
        if hovered || !self.expired_while_hovered {
            return None;
        }
        self.expired_while_hovered = false;
        self.arm(Some(HOVER_GRACE))
    }

    fn arm(&mut self, after: Option<Duration>) -> Option<ToastTimer> {
        self.timer = self.timer.wrapping_add(1);
        after.map(|after| ToastTimer {
            token: self.timer,
            after,
        })
    }
}

/// Visual direction. `Card` ships (the owner's pick); `Capsule` and `Ink` are the rendered
/// alternatives from the redesign (docs/screenshots/toast-redesign), swapped
/// in by changing [`ToastStyle::SHIPPED`] or launching with
/// `DIRI_TOAST_STYLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastStyle {
    /// A: compact glass capsule, bottom-center of the work surface.
    Capsule,
    /// B: native-notification card, top-right under the toolbar.
    Card,
    /// C: inverted ink slab, bottom-left of the work surface.
    Ink,
}

impl ToastStyle {
    pub const SHIPPED: Self = Self::Card;

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "capsule" => Some(Self::Capsule),
            "card" => Some(Self::Card),
            "ink" => Some(Self::Ink),
            _ => None,
        }
    }

    /// `DIRI_TOAST_STYLE=capsule|ink` at launch tries an alternative direction
    /// in the real app without a rebuild.
    pub fn from_env() -> Self {
        std::env::var("DIRI_TOAST_STYLE")
            .ok()
            .and_then(|name| Self::parse(&name))
            .unwrap_or(Self::SHIPPED)
    }
}

pub type ActionHandler = Box<dyn Fn(ToastCommand, &mut Window, &mut App)>;
pub type DismissHandler = Box<dyn Fn(&mut Window, &mut App)>;
pub type HoverHandler = Box<dyn Fn(bool, &mut Window, &mut App)>;

/// Callbacks a painted toast reports to its owner.
pub struct ToastHandlers {
    pub on_action: ActionHandler,
    pub on_dismiss: DismissHandler,
    pub on_hover: HoverHandler,
}

fn tone_glyph(tone: ToastTone, colors: SemanticColors) -> Option<(&'static str, Rgba)> {
    match tone {
        ToastTone::Info => None,
        ToastTone::Success => Some(("checkmark.circle", colors.secondary)),
        ToastTone::Warning => Some(("exclamationmark.triangle", Ink::ATTENTION)),
        ToastTone::Error => Some(("exclamationmark.triangle", Ink::DANGER)),
        ToastTone::Progress => Some(("arrow.triangle.2.circlepath", colors.secondary)),
    }
}

/// Where the stack sits inside the work surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastAnchor {
    BottomCenter,
    TopRight,
    BottomLeft,
}

impl ToastStyle {
    pub const fn anchor(self) -> ToastAnchor {
        match self {
            Self::Capsule => ToastAnchor::BottomCenter,
            Self::Card => ToastAnchor::TopRight,
            Self::Ink => ToastAnchor::BottomLeft,
        }
    }
}

struct Key {
    /// Stable test selector prefix (`toast`, `recovery`).
    name: &'static str,
    /// Element id; carries the slot generation so a replacement animates
    /// and a repaint does not.
    id: SharedString,
}

/// Paint one toast.
pub fn toast_element(
    toast: &Toast,
    style: ToastStyle,
    colors: SemanticColors,
    name: &'static str,
    generation: u64,
    reduce_motion: bool,
    handlers: ToastHandlers,
) -> AnyElement {
    let key = Key {
        name,
        id: SharedString::from(format!("{name}-{generation}")),
    };
    let handlers = std::rc::Rc::new(handlers);
    if toast.ambient {
        return ambient(toast, colors, &key, reduce_motion);
    }
    let body = match style {
        ToastStyle::Capsule => capsule(toast, colors, &key, &handlers),
        ToastStyle::Card => card(toast, colors, &key, &handlers),
        ToastStyle::Ink => ink(toast, colors, &key, &handlers),
    };
    let hover = handlers.clone();
    let body = div()
        .id(SharedString::from(format!("{}-hit", key.id)))
        .debug_selector(move || name.to_owned())
        .max_w_full()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_hover(move |hovered, window, cx| (hover.on_hover)(*hovered, window, cx))
        .child(body);
    if reduce_motion {
        return body.into_any_element();
    }
    // Rise toward the edge it sits on, fade in; 180 ms, ease-out.
    let rise = match style.anchor() {
        ToastAnchor::TopRight => -6.0,
        ToastAnchor::BottomCenter | ToastAnchor::BottomLeft => 8.0,
    };
    div()
        .relative()
        .max_w_full()
        .child(body)
        .with_animation(
            SharedString::from(format!("{}-entry", key.id)),
            Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
            move |element, delta| {
                element
                    .top(px(rise * (1.0 - delta)))
                    .opacity(delta.clamp(0.0, 1.0))
            },
        )
        .into_any_element()
}

fn action_button(
    action: &ToastAction,
    index: usize,
    key: &Key,
    text: Rgba,
    hover: Rgba,
    handlers: &std::rc::Rc<ToastHandlers>,
) -> AnyElement {
    let command = action.command.clone();
    let handlers = handlers.clone();
    div()
        .id(SharedString::from(format!("{}-action-{index}", key.id)))
        .debug_selector({
            let name = key.name;
            move || format!("{name}-action-{index}")
        })
        .flex_none()
        .h(px(24.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(Radius::BADGE))
        .cursor_pointer()
        .text_size(px(Typo::ROW.size))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(text)
        .hover(move |button| button.bg(hover))
        .child(action.label)
        .on_click(move |_, window, cx| {
            (handlers.on_action)(command.clone(), window, cx);
            cx.stop_propagation();
        })
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn close_button(
    key: &Key,
    group: &SharedString,
    color: Rgba,
    hover: Rgba,
    surface: Rgba,
    stroke: Rgba,
    always: bool,
    handlers: &std::rc::Rc<ToastHandlers>,
) -> AnyElement {
    let handlers = handlers.clone();
    let group = group.clone();
    let mut button = div()
        .id(SharedString::from(format!("{}-dismiss", key.id)))
        .absolute()
        .top(px(-7.0))
        .left(px(-7.0))
        .bg(surface)
        .border_1()
        .border_color(stroke)
        .shadow_sm()
        .debug_selector({
            let name = key.name;
            move || format!("{name}-dismiss")
        })
        .flex_none()
        .size(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .hover(move |button| button.bg(hover))
        .child(sf_symbol_weighted("xmark", 8.0, SymbolWeight::Bold, color))
        .on_click(move |_, window, cx| {
            (handlers.on_dismiss)(window, cx);
            cx.stop_propagation();
        });
    if !always {
        // Quiet until the pointer is on the toast: the ✕ is for the person
        // already reaching for it, not part of the message.
        button = button
            .opacity(0.0)
            .group_hover(group, |button| button.opacity(1.0));
    }
    button.into_any_element()
}

/// A: one line, hugging its text, centered over the work surface.
fn capsule(
    toast: &Toast,
    colors: SemanticColors,
    key: &Key,
    handlers: &std::rc::Rc<ToastHandlers>,
) -> AnyElement {
    let group: SharedString = format!("{}-group", key.id).into();
    let mut row = div()
        .max_w(px(560.0))
        .min_h(px(36.0))
        .pl(px(if tone_glyph(toast.tone, colors).is_some() {
            12.0
        } else {
            16.0
        }))
        .pr(px(if toast.actions.is_empty() { 16.0 } else { 6.0 }))
        .py(px(6.0))
        .flex()
        .items_center()
        .gap(px(8.0));
    if let Some((glyph, color)) = tone_glyph(toast.tone, colors) {
        row = row.child(div().flex_none().child(sf_symbol(glyph, 13.0, color)));
    }
    let mut text = div()
        .min_w(px(0.0))
        .flex_shrink(1.0)
        .text_size(px(Typo::ROW.size))
        .line_height(px(18.0))
        .text_color(colors.primary)
        .child(toast.message.clone());
    if let Some(detail) = &toast.detail {
        text = text.child(
            div()
                .text_size(px(Typo::META.size + 1.0))
                .line_height(px(16.0))
                .text_color(colors.secondary)
                .child(detail.clone()),
        );
    }
    row = row.child(text);
    for (index, action) in toast.actions.iter().enumerate() {
        row = row.child(action_button(
            action,
            index,
            key,
            colors.primary,
            colors.primary.alpha(0.08),
            handlers,
        ));
    }
    let mut corner = None;
    if toast.dismissible {
        corner = Some(close_button(
            key,
            &group,
            colors.secondary,
            colors.primary.alpha(0.08),
            colors.floating_fill(),
            colors.floating_stroke(),
            false,
            handlers,
        ));
    }
    div()
        .group(group)
        .relative()
        .max_w_full()
        .child(
            FloatingSurface::new(colors, row)
                .radius(18.0)
                .animate_entry(false),
        )
        .children(corner)
        .into_any_element()
}

/// An ambient notice: the capsule's shape and type, nothing to hit. The
/// terminal under it keeps every click, hover and scroll.
fn ambient(toast: &Toast, colors: SemanticColors, key: &Key, reduce_motion: bool) -> AnyElement {
    let name = key.name;
    let row = div()
        .max_w(px(480.0))
        .h(px(30.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(Typo::ROW.size))
                .line_height(px(18.0))
                .text_color(colors.primary)
                .child(toast.message.clone()),
        );
    let body = div()
        .debug_selector(move || name.to_owned())
        .max_w_full()
        .child(
            FloatingSurface::new(colors, row)
                .radius(15.0)
                .animate_entry(false),
        );
    if reduce_motion {
        return body.into_any_element();
    }
    // The same rise as the capsule; an in-place text swap keeps the key, so
    // only the first notice of a burst animates.
    div()
        .relative()
        .max_w_full()
        .child(body)
        .with_animation(
            SharedString::from(format!("{}-entry", key.id)),
            Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
            move |element, delta| {
                element
                    .top(px(8.0 * (1.0 - delta)))
                    .opacity(delta.clamp(0.0, 1.0))
            },
        )
        .into_any_element()
}

/// B: a small notification card — bold one-line title, one-line detail,
/// actions as text under it.
fn card(
    toast: &Toast,
    colors: SemanticColors,
    key: &Key,
    handlers: &std::rc::Rc<ToastHandlers>,
) -> AnyElement {
    let group: SharedString = format!("{}-group", key.id).into();
    let glyph = tone_glyph(toast.tone, colors).unwrap_or(("info.circle", colors.secondary));
    let mut text = div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .text_size(px(Typo::TITLE.size))
                .font_weight(Typo::TITLE.weight)
                .line_height(px(17.0))
                .text_color(colors.primary)
                .child(toast.message.clone()),
        );
    if let Some(detail) = &toast.detail {
        text = text.child(
            div()
                .text_size(px(Typo::META.size + 1.0))
                .line_height(px(16.0))
                .text_color(colors.secondary)
                .child(detail.clone()),
        );
    }
    if !toast.actions.is_empty() {
        let mut actions = div().mt(px(4.0)).ml(px(-8.0)).flex().items_center();
        for (index, action) in toast.actions.iter().enumerate() {
            actions = actions.child(action_button(
                action,
                index,
                key,
                colors.primary,
                colors.primary.alpha(0.08),
                handlers,
            ));
        }
        text = text.child(actions);
    }
    let row = div()
        .w(px(330.0))
        .p(px(11.0))
        .pl(px(12.0))
        .flex()
        .items_start()
        .gap(px(10.0))
        .child(
            div()
                .flex_none()
                .pt(px(1.0))
                .child(sf_symbol(glyph.0, 15.0, glyph.1)),
        )
        .child(text);
    let mut corner = None;
    if toast.dismissible {
        corner = Some(close_button(
            key,
            &group,
            colors.secondary,
            colors.primary.alpha(0.08),
            colors.floating_fill(),
            colors.floating_stroke(),
            false,
            handlers,
        ));
    }
    div()
        .group(group)
        .relative()
        .max_w_full()
        .child(
            FloatingSurface::new(colors, row)
                .radius(Radius::FLOATING_MENU)
                .animate_entry(false),
        )
        .children(corner)
        .into_any_element()
}

/// C: an inverted slab of type. The ink is the theme's own foreground, so it
/// is the highest-contrast thing on screen without adding a colour.
fn ink(
    toast: &Toast,
    colors: SemanticColors,
    key: &Key,
    handlers: &std::rc::Rc<ToastHandlers>,
) -> AnyElement {
    let group: SharedString = format!("{}-group", key.id).into();
    let paper = colors.primary;
    let ink = composite_opaque(colors.background);
    let mut row = div()
        .group(group.clone())
        .max_w(px(600.0))
        .min_h(px(38.0))
        .relative()
        .pl(px(14.0))
        .pr(px(if toast.actions.is_empty() { 14.0 } else { 6.0 }))
        .py(px(7.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .rounded(px(Radius::ROW))
        .bg(paper)
        .shadow(vec![gpui::BoxShadow {
            color: rgba_f32(0.0, 0.0, 0.0, 0.28).into(),
            offset: gpui::point(px(0.0), px(10.0)),
            blur_radius: px(24.0),
            spread_radius: px(0.0),
            inset: false,
        }]);
    if let Some((glyph, color)) = tone_glyph(toast.tone, colors) {
        let color = if matches!(toast.tone, ToastTone::Success | ToastTone::Progress) {
            ink.alpha(0.7)
        } else {
            color
        };
        row = row.child(
            div()
                .flex_none()
                .mr(px(-4.0))
                .child(sf_symbol(glyph, 13.0, color)),
        );
    }
    let mut text = div()
        .min_w(px(0.0))
        .flex_shrink(1.0)
        .text_size(px(Typo::ROW.size))
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(px(18.0))
        .text_color(ink)
        .child(toast.message.clone());
    if let Some(detail) = &toast.detail {
        text = text.child(
            div()
                .font_weight(FontWeight::NORMAL)
                .text_size(px(Typo::META.size + 1.0))
                .line_height(px(16.0))
                .text_color(ink.alpha(0.66))
                .child(detail.clone()),
        );
    }
    row = row.child(text);
    if !toast.actions.is_empty() {
        let mut actions = div()
            .flex_none()
            .flex()
            .items_center()
            .pl(px(6.0))
            .border_l_1()
            .border_color(ink.alpha(0.18));
        for (index, action) in toast.actions.iter().enumerate() {
            actions = actions.child(action_button(
                action,
                index,
                key,
                ink,
                ink.alpha(0.10),
                handlers,
            ));
        }
        row = row.child(actions);
    }
    if toast.dismissible {
        row = row.child(close_button(
            key,
            &group,
            ink,
            ink.alpha(0.10),
            paper,
            ink.alpha(0.25),
            false,
            handlers,
        ));
    }
    row.into_any_element()
}

fn composite_opaque(color: Rgba) -> Rgba {
    Rgba { a: 1.0, ..color }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_is_one_paragraph_and_bounded() {
        let toast = Toast::info("  Copied\n\tto   clipboard \u{7} ").detail("");
        assert_eq!(toast.message, "Copied to clipboard");
        assert_eq!(toast.detail, None);
        let long = Toast::error("x".repeat(400));
        assert_eq!(long.message.chars().count(), 161);
        assert!(long.message.ends_with('…'));
    }

    #[test]
    fn lifetime_follows_tone_detail_and_actions() {
        assert_eq!(
            Toast::info("Copied").visible_for(),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            Toast::warning("w").detail("d").visible_for(),
            Some(Duration::from_millis(8_500))
        );
        assert_eq!(
            Toast::info("a")
                .action("Retry", ToastCommand::RetryAction)
                .visible_for(),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            Toast::info("a").hold(Duration::from_secs(20)).visible_for(),
            Some(Duration::from_secs(20))
        );
        assert_eq!(Toast::info("a").persistent(false).visible_for(), None);
    }

    #[test]
    fn a_newer_toast_replaces_and_stale_timers_do_nothing() {
        let mut slot = ToastSlot::default();
        let first = slot.show(Toast::info("one")).unwrap();
        let generation = slot.generation();
        let second = slot.show(Toast::info("two")).unwrap();
        assert_ne!(slot.generation(), generation);
        assert!(!slot.expire(first.token), "stale timer");
        assert_eq!(slot.current().unwrap().message, "two");
        assert!(slot.expire(second.token));
        assert!(slot.current().is_none());
    }

    #[test]
    fn repeating_the_visible_toast_restarts_its_clock_without_reentering() {
        let mut slot = ToastSlot::default();
        let first = slot.show(Toast::error("Input rejected")).unwrap();
        let generation = slot.generation();
        let again = slot.show(Toast::error("Input rejected")).unwrap();
        assert_eq!(slot.generation(), generation, "no second entrance");
        assert!(!slot.expire(first.token));
        assert!(slot.expire(again.token));
    }

    #[test]
    fn hover_pauses_until_the_pointer_leaves_then_grants_a_grace() {
        let mut slot = ToastSlot::default();
        let timer = slot.show(Toast::info("Copied")).unwrap();
        assert_eq!(slot.set_hovered(true), None);
        assert!(!slot.expire(timer.token), "held while hovered");
        assert!(slot.current().is_some());
        let grace = slot.set_hovered(false).expect("re-armed");
        assert_eq!(grace.after, HOVER_GRACE);
        assert!(slot.expire(grace.token));
        assert!(slot.current().is_none());
    }

    #[test]
    fn leaving_before_expiry_keeps_the_original_timer() {
        let mut slot = ToastSlot::default();
        let timer = slot.show(Toast::info("Copied")).unwrap();
        slot.set_hovered(true);
        assert_eq!(slot.set_hovered(false), None);
        assert!(slot.expire(timer.token));
    }

    #[test]
    fn a_burst_of_ambient_notices_updates_one_capsule_in_place() {
        let mut slot = ToastSlot::default();
        let first = slot.show(Toast::ambient("fix-auth: Split pane")).unwrap();
        let generation = slot.generation();
        let second = slot.show(Toast::ambient("fix-auth: Focus pane")).unwrap();
        let third = slot.show(Toast::ambient("docs: Zoom pane")).unwrap();
        assert_eq!(slot.generation(), generation, "no second entrance");
        assert_eq!(slot.current().unwrap().message, "docs: Zoom pane");
        assert_eq!(third.after, AMBIENT_HOLD);
        assert!(!slot.expire(first.token), "the burst keeps it up");
        assert!(!slot.expire(second.token));
        assert!(slot.expire(third.token));
        assert!(slot.current().is_none());
        // After it closed, the next notice enters again.
        slot.show(Toast::ambient("docs: New tab"));
        assert_ne!(slot.generation(), generation);
    }

    #[test]
    fn an_ambient_notice_is_quiet_and_short() {
        let notice = Toast::ambient("fix-auth: Zoom pane");
        assert!(notice.ambient && !notice.dismissible && notice.actions.is_empty());
        assert_eq!(notice.visible_for(), Some(AMBIENT_HOLD));
        assert!(!Toast::info("Copied").ambient);
        // A regular toast replacing it still enters as itself.
        let mut slot = ToastSlot::default();
        slot.show(notice);
        let generation = slot.generation();
        slot.show(Toast::info("Copied"));
        assert_ne!(slot.generation(), generation);
    }

    #[test]
    fn persistent_toasts_arm_no_timer() {
        let mut slot = ToastSlot::default();
        assert_eq!(slot.show(Toast::warning("x").persistent(false)), None);
        assert!(slot.current().is_some());
    }
}
