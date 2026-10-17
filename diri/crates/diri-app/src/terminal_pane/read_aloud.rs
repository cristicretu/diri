//! The title bar's speaker: whether "Talk with your agent" reads this tab.
use super::*;
use crate::palette_chrome::PaletteTooltip;
use crate::tooltip_warmth::WarmTooltip;
use diri_ui::{Icon, IconName};

impl TerminalPane {
    /// Shown for agent tabs while "Talk with your agent" is on. Off stops a
    /// reading of this tab mid-sentence and skips its next ones; other tabs
    /// and its notifications are unaffected.
    pub(super) fn render_read_aloud_toggle(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if *session.effective_kind() == ProtoAgentKind::SHELL {
            return None;
        }
        let (enabled, reading) = {
            let store = self.runtime.store.read().expect("session store lock poisoned");
            (
                store.preferences().talk_with_agent,
                store.tab_reads_aloud(&session.id),
            )
        };
        if !enabled {
            return None;
        }
        let help = t(if reading {
            "terminal.read_aloud.stop"
        } else {
            "terminal.read_aloud.start"
        })
        .to_owned();
        let id = session.id.clone();
        Some(
            div()
                .id("read-aloud-toggle")
                .debug_selector(|| "read-aloud-toggle".into())
                .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::ROW))
                .hover(move |el| el.bg(Fill::subtle(colors)))
                .cursor_pointer()
                .child(Icon::new(
                    if reading {
                        IconName::Speaker
                    } else {
                        IconName::SpeakerOff
                    },
                    14.0,
                    if reading {
                        colors.secondary
                    } else {
                        colors.tertiary
                    },
                ))
                .warm_tooltip(move |_, cx| cx.new(|_| PaletteTooltip(help.clone(), colors)).into())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.runtime
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .set_read_aloud(id.clone(), crate::store::ReadAloudChoice::Toggle);
                    cx.notify();
                }))
                .into_any_element(),
        )
    }
}
