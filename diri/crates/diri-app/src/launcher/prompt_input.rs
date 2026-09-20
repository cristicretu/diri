//! Native text input for the prompt composers. Without a registered handler
//! AppKit has nowhere to send marked or committed text, so an input method
//! could only ever deliver the raw keys behind a composition.
//!
//! The handler is registered under the launcher's focus handle while a prompt
//! is on screen, and carries the composition epoch it was registered under:
//! a callback AppKit delivers late cannot write into a draft that has since
//! been cancelled, switched or sent.
use super::{CARET, COMPOSER_LINE_HEIGHT, COMPOSER_PAD_TOP, COMPOSER_PADDING, LauncherOverlay};
use gpui::{
    App, Bounds, Context, InputHandler, Pixels, Point, UTF16Selection, WeakEntity, Window, canvas,
    div, prelude::*, px,
};
use std::ops::Range;

pub(super) struct PromptInputHandler {
    pub(super) launcher: WeakEntity<LauncherOverlay>,
    pub(super) epoch: u64,
    /// Where the first visual line starts when nothing is scrolled.
    pub(super) origin: Point<Pixels>,
}

impl PromptInputHandler {
    fn with<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut LauncherOverlay, &mut Window, &mut Context<LauncherOverlay>) -> T,
    ) -> Option<T> {
        self.launcher
            .update(cx, |launcher, cx| {
                if !launcher.focus.is_focused(window) {
                    // Input callbacks can precede the frame that notices the
                    // blur. Preedit never outlives the focus it was typed in.
                    launcher.cancel_prompt_composition(cx);
                    return None;
                }
                if !launcher.prompt_accepts_input()
                    || launcher.prompt.composition_epoch() != self.epoch
                {
                    return None;
                }
                Some(f(launcher, window, cx))
            })
            .ok()
            .flatten()
    }

    fn edit(&self, window: &mut Window, cx: &mut App, edit: impl FnOnce(&mut LauncherOverlay)) {
        self.with(window, cx, |launcher, window, cx| {
            edit(launcher);
            launcher.prompt_text_changed();
            window.invalidate_character_coordinates();
            cx.notify();
        });
    }

    fn caret(launcher: &LauncherOverlay, window: &Window) -> Option<&'static str> {
        launcher.focus.is_focused(window).then_some(CARET)
    }
}

impl InputHandler for PromptInputHandler {
    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.with(window, cx, |launcher, _, _| {
            launcher.prompt.utf16_selection()
        })
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.with(window, cx, |launcher, _, _| {
            launcher.prompt.utf16_marked_range()
        })
        .flatten()
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.with(window, cx, |launcher, _, _| {
            let (text, range) = launcher.prompt.text_for_utf16_range(range);
            *adjusted = Some(range);
            text
        })
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.edit(window, cx, |launcher| {
            launcher.prompt.replace_from_input(range, text, None, false);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.edit(window, cx, |launcher| {
            launcher
                .prompt
                .replace_from_input(range, text, selected, true);
        });
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.edit(window, cx, |launcher| launcher.prompt.finish_composition());
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let origin = self.origin;
        self.with(window, cx, |launcher, window, _| {
            launcher.prompt.bounds_for_utf16_range(
                range,
                origin,
                px(COMPOSER_LINE_HEIGHT),
                Self::caret(launcher, window),
                window,
            )
        })
        .flatten()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.with(window, cx, |launcher, window, _| {
            let hit = launcher.prompt_hit(point, window)?;
            Some(launcher.prompt.text()[..hit.caret].encode_utf16().count())
        })
        .flatten()
    }

    fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
        // Only while the prompt would take the text: the recipe editor and
        // the folder step read keys through the same focus handle.
        self.with(window, cx, |_, _, _| ()).is_some()
    }
}

/// A zero-sized element at the first line's origin whose only job is to
/// register the handler during paint, which is the one phase GPUI accepts it
/// in. Registration is per frame, so a prompt that is not drawn has none.
pub(super) fn register(
    launcher: &LauncherOverlay,
    cx: &Context<LauncherOverlay>,
) -> impl IntoElement + use<> {
    let focus = launcher.focus.clone();
    let epoch = launcher.prompt.composition_epoch();
    let launcher = cx.entity().downgrade();
    div()
        .absolute()
        .top(px(COMPOSER_PAD_TOP))
        .left(px(COMPOSER_PADDING))
        .size_0()
        .debug_selector(|| "launcher-prompt-input".into())
        .child(canvas(
            |bounds, _, _| bounds.origin,
            move |_, origin, window, cx| {
                window.handle_input(
                    &focus,
                    PromptInputHandler {
                        launcher: launcher.clone(),
                        epoch,
                        origin,
                    },
                    cx,
                );
            },
        ))
}
