//! The Find field owns Cocoa text input while open. Its handler is tied to one
//! pane, residency and editing epoch; delayed native callbacks never retarget
//! whichever session happens to be selected later.
use super::{AttachmentGeneration, QueryEditor, SessionId, TerminalPane};
use crate::text_input::{self, Composition};
use diri_ui::SemanticColors;
use gpui::{AnyElement, App, Context, WeakEntity, Window};

pub(super) use crate::text_input::discard_native;

#[derive(Clone)]
struct Owner {
    pane: WeakEntity<TerminalPane>,
    session: SessionId,
    residency: AttachmentGeneration,
    epoch: u64,
}
impl Owner {
    fn with<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut TerminalPane, &mut Window, &mut Context<TerminalPane>) -> T,
    ) -> Option<T> {
        self.pane
            .update(cx, |pane, cx| {
                if pane.selected_id().as_ref() != Some(&self.session) {
                    return None;
                }
                if !pane.focus.is_focused(window) {
                    // Input callbacks can precede the frame which emits blur.
                    // Cancel preedit immediately when its logical owner is gone.
                    pane.cancel_find_composition(window, cx);
                    return None;
                }
                let resident = pane.residents.get(&self.session)?;
                if resident.attachment_generation != self.residency
                    || resident.find.is_none()
                    || resident.find_composition.epoch != self.epoch
                {
                    return None;
                }
                Some(f(pane, window, cx))
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
        self.with(window, cx, |pane, window, _| {
            let resident = &pane.residents[&self.session];
            f(&resident.find_query, &resident.find_composition, window)
        })
    }
    fn edit(
        &self,
        window: &mut Window,
        cx: &mut App,
        edit: impl FnOnce(&mut Composition, &mut QueryEditor),
    ) {
        self.with(window, cx, |pane, window, cx| {
            let resident = pane.residents.get_mut(&self.session).unwrap();
            edit(&mut resident.find_composition, &mut resident.find_query);
            if resident.find.as_mut().unwrap().set_query(
                resident.find_query.text().to_owned(),
                pane.started_at.elapsed(),
            ) {
                resident.element.set_find_highlights(Vec::new());
            }
            pane.schedule_query_search(self.session.clone(), window, cx);
            window.invalidate_character_coordinates();
            cx.notify();
        });
    }
}

pub(super) fn render(
    pane: &TerminalPane,
    session: &SessionId,
    colors: SemanticColors,
    cx: &Context<TerminalPane>,
) -> AnyElement {
    let resident = &pane.residents[session];
    let owner = Owner {
        pane: cx.entity().downgrade(),
        session: session.clone(),
        residency: resident.attachment_generation,
        epoch: resident.find_composition.epoch,
    };
    text_input::render(
        resident.find_query.clone(),
        &resident.find_composition,
        crate::i18n::t("terminal.find.placeholder").into(),
        text_input::Style {
            colors,
            selection: colors.primary.alpha(0.16),
        },
        pane.focus.clone(),
        owner,
    )
}
