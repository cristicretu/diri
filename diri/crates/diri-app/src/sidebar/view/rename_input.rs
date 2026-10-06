//! The inline rename owns Cocoa text input while it is open, so an input
//! method can compose a title. Its handler is tied to the session being renamed
//! and the composition epoch; a callback that arrives after the edit ended or
//! moved to another row changes nothing.
use super::*;
use crate::query_editor::QueryEditor;
use crate::text_input::{self, Composition};

#[derive(Clone)]
struct Owner {
    sidebar: WeakEntity<Sidebar>,
    session: SessionId,
    epoch: u64,
}
impl Owner {
    fn with<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Sidebar, &mut Window, &mut Context<Sidebar>) -> T,
    ) -> Option<T> {
        self.sidebar
            .update(cx, |sidebar, cx| {
                if sidebar.ui.renaming.as_ref() != Some(&self.session) {
                    return None;
                }
                if !sidebar.focus_handle.is_focused(window) {
                    // Input callbacks can precede the frame which notices focus
                    // left; preedit never outlives its field.
                    sidebar.cancel_rename_composition(window, cx);
                    return None;
                }
                if sidebar.ui.rename_composition.epoch != self.epoch {
                    return None;
                }
                Some(f(sidebar, window, cx))
            })
            .ok()
            .flatten()
    }
}
impl text_input::Owner for Owner {
    fn read<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&QueryEditor, &Composition, &Window) -> T,
    ) -> Option<T> {
        self.with(window, cx, |sidebar, window, _| {
            f(
                &sidebar.ui.rename_draft,
                &sidebar.ui.rename_composition,
                window,
            )
        })
    }
    fn edit(
        &self,
        window: &mut Window,
        cx: &mut App,
        edit: impl FnOnce(&mut Composition, &mut QueryEditor),
    ) {
        self.with(window, cx, |sidebar, window, cx| {
            edit(
                &mut sidebar.ui.rename_composition,
                &mut sidebar.ui.rename_draft,
            );
            window.invalidate_character_coordinates();
            cx.notify();
        });
    }
}

impl Sidebar {
    pub(super) fn rename_field(
        &self,
        session: &SessionId,
        colors: SemanticColors,
        cx: &Context<Self>,
    ) -> AnyElement {
        text_input::render(
            self.ui.rename_draft.clone(),
            &self.ui.rename_composition,
            SharedString::default(),
            text_input::Style {
                colors,
                selection: Palette::CLAY.alpha(0.35),
            },
            self.focus_handle.clone(),
            Owner {
                sidebar: cx.entity().downgrade(),
                session: session.clone(),
                epoch: self.ui.rename_composition.epoch,
            },
        )
    }

    #[cfg(test)]
    pub(super) fn rename_input_handler(
        &self,
        cx: &Context<Self>,
    ) -> impl gpui::InputHandler + use<> {
        text_input::test_handler(
            Owner {
                sidebar: cx.entity().downgrade(),
                session: self.ui.renaming.clone().expect("renaming"),
                epoch: self.ui.rename_composition.epoch,
            },
            Bounds::new(point(px(40.0), px(60.0)), gpui::size(px(160.0), px(18.0))),
            SemanticColors::dark(),
        )
    }

    pub(super) fn cancel_rename_composition(&mut self, window: &mut Window, cx: &mut App) {
        self.ui.cancel_rename_composition();
        self.discard_rename_preedit(window, cx);
    }

    /// Clears preedit a cancelled composition left in the Cocoa input
    /// context, and drops preedit whose field lost focus. Renames end from
    /// paths without a window, so each render settles what they left behind.
    pub(super) fn discard_rename_preedit(&mut self, window: &mut Window, cx: &mut App) {
        if self.ui.rename_composition.is_composing() && !self.focus_handle.is_focused(window) {
            self.ui.cancel_rename_composition();
        }
        if std::mem::take(&mut self.ui.rename_discard_native) {
            text_input::discard_native(window, cx);
        }
    }

    /// Editing keys that reach the rename while an input method composes:
    /// Escape drops the preedit before it may cancel the rename, Enter accepts
    /// it, and any other edit first fixes the preedit as typed text.
    pub(super) fn rename_composition_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if !self.ui.rename_composition.is_composing() {
            return false;
        }
        match key {
            "escape" => {
                self.cancel_rename_composition(window, cx);
                true
            }
            "enter" => {
                self.ui.rename_composition.finish();
                true
            }
            _ => {
                self.ui.rename_composition.finish();
                false
            }
        }
    }
}
