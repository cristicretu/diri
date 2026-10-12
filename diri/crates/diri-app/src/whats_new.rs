//! What's New: the highlights of a release, shown once after an update.
//!
//! An update never interrupts. The first launch on a release with highlights
//! puts one quiet line in the sidebar footer, where the update pill sits; the
//! sheet with the clips opens only when that line is clicked. Opening it, or
//! dismissing the line, marks the release seen. A new install starts with
//! everything seen, so a first launch shows none of this. Help › What's New,
//! the What's New command, and the thumbnails in Settings › What's New replay
//! the sheet any time.
//!
//! Each highlight carries a short clip recorded from the real window by the
//! `render_whats_new_clips` fixture and encoded by `scripts/whats-new-clips.sh`,
//! so the clips follow the app's look instead of going stale.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use diri_ui::{Appearance, FloatingSurface, Radius, SemanticColors, Typo};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, ObjectFit,
    RenderImage, SharedString, Task, Window, div, img, prelude::*, px,
};
use image::AnimationDecoder as _;

use crate::commands::{CommandId, command};
use crate::store::StoreRuntime;

/// One clip, recorded in each appearance.
pub(crate) struct Clip {
    pub dark: &'static [u8],
    pub light: &'static [u8],
}

impl Clip {
    pub(crate) fn for_appearance(&self, appearance: Appearance) -> &'static [u8] {
        match appearance {
            Appearance::Light => self.light,
            Appearance::Dark => self.dark,
        }
    }
}

pub(crate) struct Highlight {
    pub title: &'static str,
    /// One sentence: what it is and how to reach it.
    pub summary: &'static str,
    pub clip: Clip,
    /// The command "Try it" runs, labelled with the command's own title.
    pub action: Option<(&'static str, CommandId)>,
}

pub(crate) struct Release {
    /// The first version that ships these highlights.
    pub version: &'static str,
    /// The footer line's few words: "What's new · {headline}".
    pub headline: &'static str,
    pub highlights: &'static [Highlight],
}

macro_rules! clip {
    ($name:literal) => {
        Clip {
            dark: include_bytes!(concat!("../assets/whats-new/", $name, "-dark.webp")),
            light: include_bytes!(concat!("../assets/whats-new/", $name, "-light.webp")),
        }
    };
}

/// Newest first. A release's highlights appear once for anyone updating from
/// an earlier version; add an entry when a release has something worth a
/// clip, and record it with `scripts/whats-new-clips.sh`.
pub(crate) const RELEASES: &[Release] = &[Release {
    version: "0.9.0",
    headline: "Notes",
    highlights: &[
        Highlight {
            title: "Notes",
            summary: "Think next to your agents. Type / for blocks and @ to link a session; every note is plain Markdown on disk.",
            clip: clip!("notes"),
            action: Some(("New Note", CommandId::NewNote)),
        },
        Highlight {
            title: "To-dos that start agents",
            summary: "Press ⌃⌘↩ on a to-do to hand it to an agent with the note as context. Its progress shows up right under the to-do.",
            clip: clip!("todos"),
            action: Some(("Show To-dos", CommandId::ShowTodos)),
        },
        Highlight {
            title: "Search every note",
            summary: "⇧⌘F finds a note by anything it says, not just its title.",
            clip: clip!("search"),
            action: Some(("Search Notes", CommandId::SearchNotes)),
        },
    ],
}];

/// The version running now. Tests pretend to be a later release.
pub(crate) fn current_version() -> String {
    #[cfg(test)]
    if let Some(version) = TEST_VERSION.with(|v| v.borrow().clone()) {
        return version;
    }
    crate::updates::CURRENT_VERSION.to_owned()
}

#[cfg(test)]
thread_local! {
    static TEST_VERSION: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_current_version_for_test(version: Option<&str>) {
    TEST_VERSION.with(|v| *v.borrow_mut() = version.map(str::to_owned));
}

/// `major.minor.patch`; anything unreadable (an empty "seen") sorts first.
fn parse(version: &str) -> (u64, u64, u64) {
    let mut parts = version
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().ok());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Some(major)), Some(Some(minor)), Some(Some(patch))) => (major, minor, patch),
        _ => (0, 0, 0),
    }
}

/// Releases newer than `seen` that this build already includes, newest first.
pub(crate) fn unseen(seen: &str, current: &str) -> Vec<&'static Release> {
    let (seen, current) = (parse(seen), parse(current));
    RELEASES
        .iter()
        .filter(|release| {
            let version = parse(release.version);
            seen < version && version <= current
        })
        .collect()
}

/// What the sheet shows when opened by hand with nothing unseen: the newest
/// release this build includes.
pub(crate) fn latest(current: &str) -> Vec<&'static Release> {
    let current = parse(current);
    RELEASES
        .iter()
        .find(|release| parse(release.version) <= current)
        .into_iter()
        .collect()
}

// ---------------------------------------------------------------------------
// Clip playback

/// Plays an animated WebP by decoding one frame at a time on its own thread.
///
/// GPUI's `img()` decodes every frame of an animation up front and keeps them
/// all, which for a 1440x900 clip is hundreds of megabytes. Here two frames
/// exist at once: the one painted and the one decoded next, and each frame's
/// atlas texture is released once it is replaced.
pub(crate) struct ClipPlayer {
    frame: Option<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    _playback: Task<()>,
}

type Decoded = (image::Frame, Duration);

impl ClipPlayer {
    pub(crate) fn new(bytes: &'static [u8], still: bool, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Decoded>(1);
        let spawned = std::thread::Builder::new()
            .name("whats-new-clip".into())
            .spawn(move || decode(bytes, still, &tx));
        let playback = cx.spawn(async move |this, cx| {
            if spawned.is_err() {
                return;
            }
            let mut rx = rx;
            loop {
                let (back, next) = cx
                    .background_spawn(async move {
                        let next = rx.recv();
                        (rx, next)
                    })
                    .await;
                rx = back;
                let Ok((frame, delay)) = next else {
                    return;
                };
                let image = Arc::new(RenderImage::new(smallvec::smallvec![frame]));
                let alive = this
                    .update(cx, |player, cx| {
                        if let Some(old) = player.frame.replace(image) {
                            player.retired.push(old);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive || still {
                    return;
                }
                cx.background_executor().timer(delay).await;
            }
        });
        Self {
            frame: None,
            retired: Vec::new(),
            _playback: playback,
        }
    }

    /// Frees the painted frame's texture; call before dropping the player.
    pub(crate) fn release(&mut self, window: &mut Window, cx: &mut App) {
        for image in self.retired.drain(..).chain(self.frame.take()) {
            cx.drop_image(image, Some(window));
        }
    }
}

/// Decodes `bytes` in a loop, sending each frame with its hold time, until
/// the player hangs up. `still` sends only the last frame: the finished state.
fn decode(bytes: &'static [u8], still: bool, tx: &std::sync::mpsc::SyncSender<Decoded>) {
    loop {
        let Ok(decoder) = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)) else {
            return;
        };
        let mut last = None;
        for frame in decoder.into_frames() {
            let Ok(mut frame) = frame else {
                return;
            };
            // GPUI paints BGRA.
            for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            let delay = Duration::from(frame.delay()).max(Duration::from_millis(16));
            if still {
                last = Some(frame);
                continue;
            }
            if tx.send((frame, delay)).is_err() {
                return;
            }
        }
        if still {
            if let Some(frame) = last {
                let _ = tx.send((frame, Duration::ZERO));
            }
            return;
        }
    }
}

impl Render for ClipPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired.drain(..) {
            cx.drop_image(image, Some(window));
        }
        match &self.frame {
            Some(frame) => img(frame.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div().size_full().into_any_element(),
        }
    }
}

// ---------------------------------------------------------------------------
// The sheet

pub(crate) enum WhatsNewEvent {
    Close,
    Run(CommandId),
    ReleaseNotes,
}

pub(crate) struct WhatsNewSheet {
    pages: Vec<&'static Highlight>,
    /// The newest release shown, for the eyebrow.
    version: &'static str,
    index: usize,
    player: Option<gpui::Entity<ClipPlayer>>,
    appearance: Option<Appearance>,
    store: Arc<StoreRuntime>,
    focus: FocusHandle,
}

impl EventEmitter<WhatsNewEvent> for WhatsNewSheet {}

impl Focusable for WhatsNewSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The largest clip the sheet shows, in points; 16:10 like the recordings.
const CLIP_WIDTH: f32 = 720.0;
const CLIP_RATIO: f32 = 0.625;
const PAD: f32 = 14.0;

impl WhatsNewSheet {
    pub(crate) fn new(
        releases: &[&'static Release],
        store: Arc<StoreRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            pages: releases
                .iter()
                .flat_map(|release| release.highlights.iter())
                .collect(),
            version: releases.first().map_or("", |release| release.version),
            index: 0,
            player: None,
            appearance: None,
            store,
            focus: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn page(&self) -> usize {
        self.index
    }

    #[cfg(test)]
    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::colors_in(
            &self
                .store
                .store
                .read()
                .expect("session store lock poisoned"),
        )
    }

    pub(crate) fn go(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let index = index.min(self.pages.len().saturating_sub(1));
        if index != self.index {
            self.index = index;
            self.drop_player(window, cx);
            cx.notify();
        }
    }

    fn drop_player(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(player) = self.player.take() {
            player.update(cx, |player, cx| player.release(window, cx));
        }
    }

    /// Frees the clip's textures; the root calls this as the sheet closes.
    pub(crate) fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_player(window, cx);
    }

    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.index + 1 < self.pages.len() {
            self.go(self.index + 1, window, cx);
        } else {
            cx.emit(WhatsNewEvent::Close);
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => cx.emit(WhatsNewEvent::Close),
            "right" | "enter" | "space" => self.next(window, cx),
            "left" => self.go(self.index.saturating_sub(1), window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn player(
        &mut self,
        appearance: Appearance,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Entity<ClipPlayer>> {
        if self.appearance != Some(appearance) {
            // The theme changed under the sheet: the other recording.
            self.player = None;
            self.appearance = Some(appearance);
        }
        let page = *self.pages.get(self.index)?;
        let still = cx.reduce_motion();
        Some(
            self.player
                .get_or_insert_with(|| {
                    let bytes = page.clip.for_appearance(appearance);
                    cx.new(|cx| ClipPlayer::new(bytes, still, cx))
                })
                .clone(),
        )
    }
}

fn sheet_button(
    id: &'static str,
    label: SharedString,
    shortcut: Option<String>,
    emphasized: bool,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .h(px(26.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(if emphasized { 0.14 } else { 0.10 }))
        .bg(colors.primary.alpha(if emphasized { 0.08 } else { 0.04 }))
        .text_size(px(12.0))
        .text_color(colors.primary)
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(if emphasized { 0.13 } else { 0.09 })))
        .child(label)
        .when_some(shortcut, |button, shortcut| {
            button.child(div().text_color(colors.tertiary).child(shortcut))
        })
}

impl Render for WhatsNewSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let viewport = window.viewport_size();
        let clip_width = CLIP_WIDTH
            .min(f32::from(viewport.width) - 2.0 * PAD - 64.0)
            .min((f32::from(viewport.height) - 2.0 * PAD - 200.0) / CLIP_RATIO)
            .max(320.0);
        let clip_height = clip_width * CLIP_RATIO;
        let player = self.player(colors.appearance, cx);
        let count = self.pages.len();
        let index = self.index;
        let Some(page) = self.pages.get(index).copied() else {
            return div().into_any_element();
        };
        let last = index + 1 == count;
        let eyebrow = if count > 1 {
            crate::i18n::tf(
                "settings.whats_new.new_in_page",
                &[
                    ("version", &self.version),
                    ("page", &(index + 1)),
                    ("count", &count),
                ],
            )
        } else {
            crate::i18n::tf("settings.whats_new.new_in", &[("version", &self.version)])
        };

        let dots = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .children((0..count).map(|dot| {
                div()
                    .id(("whats-new-dot", dot))
                    .size(px(6.0))
                    .rounded_full()
                    .bg(colors.primary.alpha(if dot == index { 0.65 } else { 0.16 }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.go(dot, window, cx)))
            }));

        let try_it = page.action.map(|(label, id)| {
            sheet_button(
                "whats-new-try",
                label.into(),
                command(id).shortcut_label(),
                false,
                colors,
            )
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(WhatsNewEvent::Run(id))))
        });
        let advance = sheet_button(
            "whats-new-next",
            crate::i18n::t(if last {
                "settings.whats_new.done"
            } else {
                "settings.whats_new.next"
            })
            .into(),
            None,
            true,
            colors,
        )
        .on_click(cx.listener(|this, _, window, cx| this.next(window, cx)));

        let sheet = div()
            .id("whats-new-sheet")
            .w(px(clip_width + 2.0 * PAD))
            .p(px(PAD))
            .flex()
            .flex_col()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(clip_width))
                    .h(px(clip_height))
                    .rounded(px(Radius::PANEL - PAD / 2.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(colors.primary.alpha(0.08))
                    .bg(colors.background)
                    .children(player),
            )
            .child(
                div()
                    .pt(px(14.0))
                    .px(px(2.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .font_weight(Typo::META.weight)
                            .text_color(colors.tertiary)
                            .child(eyebrow),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::DISPLAY_TITLE.size))
                            .font_weight(Typo::DISPLAY_TITLE.weight)
                            .text_color(colors.primary)
                            .child(page.title),
                    )
                    .child(
                        div()
                            .max_w(px(560.0))
                            .text_size(px(Typo::ROW.size))
                            .text_color(colors.secondary)
                            .child(page.summary),
                    ),
            )
            .child(
                div()
                    .pt(px(16.0))
                    .px(px(2.0))
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .when(count > 1, |row| row.child(dots))
                    .child(
                        div()
                            .id("whats-new-release-notes")
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .cursor_pointer()
                            .hover(move |style| style.text_color(colors.secondary))
                            .child(crate::i18n::t("settings.whats_new.release_notes"))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(WhatsNewEvent::ReleaseNotes)),
                            ),
                    )
                    .child(div().flex_1())
                    .children(try_it)
                    .child(advance),
            );

        div()
            .id("whats-new")
            .key_context("WhatsNew")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000055))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(WhatsNewEvent::Close);
                    cx.stop_propagation();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(FloatingSurface::new(colors, sheet))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_numerically() {
        assert!(parse("0.8.12") > parse("0.8.9"));
        assert!(parse("0.9.0") > parse("0.8.12"));
        assert_eq!(parse("v1.2.3"), (1, 2, 3));
        assert_eq!(parse(""), (0, 0, 0));
        assert_eq!(parse("garbage"), (0, 0, 0));
    }

    #[test]
    fn a_release_is_unseen_once_and_only_in_builds_that_ship_it() {
        let newest = RELEASES[0].version;
        // Updated from before it: shown.
        assert_eq!(unseen("0.0.1", newest).len(), 1);
        // Prefs written before the field existed read as "".
        assert_eq!(unseen("", newest).len(), 1);
        // Already seen, or a build that does not include it yet: hidden.
        assert!(unseen(newest, newest).is_empty());
        assert!(unseen("0.0.1", "0.0.2").is_empty());
        // Opened by hand with nothing unseen: the newest shipped release.
        assert_eq!(latest(newest).len(), 1);
        assert!(latest("0.0.1").is_empty());
    }

    #[test]
    fn every_highlight_has_both_clips_and_one_sentence_of_copy() {
        for release in RELEASES {
            assert!(!release.highlights.is_empty());
            for highlight in release.highlights {
                for clip in [highlight.clip.dark, highlight.clip.light] {
                    assert_eq!(
                        image::guess_format(clip).ok(),
                        Some(image::ImageFormat::WebP),
                        "{}",
                        highlight.title
                    );
                }
                assert!(highlight.summary.len() < 140, "{}", highlight.title);
            }
        }
    }
}
