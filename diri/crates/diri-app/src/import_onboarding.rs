//! The first-launch import page: the five most recent Claude Code and Codex
//! conversations on this Mac, pre-checked, and one press to bring them into
//! the sidebar as sessions that resume when opened.
//!
//! It is the resting workbench's sibling and speaks its vocabulary: a 15 px
//! title, row-sized type, hairline borders, the same bordered button with
//! its key set inside, and quiet links for everything secondary. Rows carry
//! only what identifies a chat at a glance (agent mark, title, folder name,
//! age); the focused row adds its last message, one line, so the person
//! recognises it without opening anything.
//!
//! The zero-history variant keeps the same shape for a Mac with nothing to
//! import: the installed Agents, a folder to open, and a way to start.

use std::rc::Rc;

use diri_proto::AgentKind;
use diri_ui::{AgentLogo, Radius, SemanticColors, Typo};
use gpui::{AnyElement, App, Div, IntoElement, Role, SharedString, Window, div, prelude::*, px};

use crate::agent_setup::quiet_link;
use crate::i18n::{t, tf};
use crate::icons::sf_symbol;
use crate::store::{ConversationImportState, folder_name};

/// One row as drawn.
pub(crate) struct ImportRow {
    pub id: String,
    pub kind: AgentKind,
    pub title: SharedString,
    pub folder: SharedString,
    pub age: SharedString,
    pub preview: Option<SharedString>,
    pub checked: bool,
    pub focused: bool,
}

/// A snapshot of the store's import state, taken under its lock and
/// rendered without it.
pub(crate) struct ImportView {
    pub rows: Vec<ImportRow>,
    pub selected: usize,
    pub more: usize,
    pub more_truncated: bool,
    pub browsing: bool,
    pub query: String,
    pub importing: bool,
    /// Installed Agents for the quiet strip under the title.
    pub agents: Vec<(AgentKind, SharedString)>,
}

impl ImportView {
    pub(crate) fn from_state(
        state: &ConversationImportState,
        agents: Vec<(AgentKind, SharedString)>,
        now_ms: f64,
    ) -> Self {
        let rows = state
            .visible_rows()
            .into_iter()
            .map(|candidate| {
                let entry = &candidate.entry;
                let focused = state.focused.as_deref() == Some(entry.id.as_str());
                ImportRow {
                    id: entry.id.clone(),
                    kind: entry.kind.clone(),
                    title: entry
                        .title
                        .clone()
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or_else(|| t("import.untitled").to_owned())
                        .into(),
                    folder: folder_name(&entry.cwd).into(),
                    age: age_label(now_ms, entry.last_active_at.0).into(),
                    preview: focused
                        .then(|| candidate.preview.clone())
                        .flatten()
                        .map(Into::into),
                    checked: state.selected.contains(&entry.id),
                    focused,
                }
            })
            .collect();
        Self {
            rows,
            selected: state.selected_count(),
            more: state.more().len(),
            more_truncated: state
                .scan
                .as_ref()
                .is_some_and(|scan| scan.more_truncated),
            browsing: state.browsing,
            query: state.query.text().to_owned(),
            importing: state.importing,
            agents,
        }
    }
}

pub(crate) type IdHandler = Rc<dyn Fn(&str, &mut Window, &mut App)>;
pub(crate) type Handler = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type BrowseHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

pub(crate) struct ImportActions {
    pub toggle: IdHandler,
    pub focus: IdHandler,
    pub import: Handler,
    pub browse: BrowseHandler,
    pub start_fresh: Handler,
}

pub(crate) struct FreshActions {
    pub open_folder: Handler,
    pub moving: Handler,
}

pub(crate) fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

/// Every control on the page writes the canonical store, which owns the
/// import state for all windows, then repaints.
pub(crate) fn actions(
    store: &std::sync::Arc<std::sync::RwLock<crate::store::SessionStore>>,
) -> ImportActions {
    fn with(
        store: &std::sync::Arc<std::sync::RwLock<crate::store::SessionStore>>,
        apply: impl Fn(&mut crate::store::SessionStore) + 'static,
    ) -> Handler {
        let store = std::sync::Arc::clone(store);
        Rc::new(move |_, cx| {
            apply(&mut store.write().expect("session store lock poisoned"));
            cx.refresh_windows();
        })
    }
    let toggle_store = std::sync::Arc::clone(store);
    let focus_store = std::sync::Arc::clone(store);
    let browse_store = std::sync::Arc::clone(store);
    ImportActions {
        toggle: Rc::new(move |id, _, cx| {
            toggle_store
                .write()
                .expect("session store lock poisoned")
                .toggle_conversation(id);
            cx.refresh_windows();
        }),
        focus: Rc::new(move |id, _, cx| {
            let mut store = focus_store.write().expect("session store lock poisoned");
            if store.conversation_import().focused.as_deref() != Some(id) {
                store.focus_conversation(id);
                drop(store);
                cx.refresh_windows();
            }
        }),
        import: with(store, |store| {
            store.import_selected_conversations();
        }),
        browse: Rc::new(move |browsing, _, cx| {
            browse_store
                .write()
                .expect("session store lock poisoned")
                .set_conversation_browsing(browsing);
            cx.refresh_windows();
        }),
        start_fresh: with(store, crate::store::SessionStore::dismiss_conversation_import),
    }
}

/// `12m`, `2h`, `Yesterday`, `3d`: the sidebar's compact clock.
pub(crate) fn age_label(now_ms: f64, then_ms: f64) -> String {
    let seconds = ((now_ms - then_ms).max(0.0) / 1000.0) as u64;
    let count = |key: &'static str, value: u64| tf(key, &[("count", &value)]);
    match seconds {
        0..=59 => t("import.age.now").to_owned(),
        60..=3_599 => count("import.age.minutes", seconds / 60),
        3_600..=86_399 => count("import.age.hours", seconds / 3_600),
        86_400..=172_799 => t("import.age.yesterday").to_owned(),
        172_800..=604_799 => count("import.age.days", seconds / 86_400),
        604_800..=2_591_999 => count("import.age.weeks", seconds / 604_800),
        _ => count("import.age.months", (seconds / 2_592_000).max(1)),
    }
}

const COLUMN_WIDTH: f32 = 460.0;
const ROW_HEIGHT: f32 = 32.0;
/// Rows visible before the browse list scrolls.
const BROWSE_VISIBLE_ROWS: f32 = 9.5;

fn column() -> Div {
    div()
        .w_full()
        .max_w(px(COLUMN_WIDTH))
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
}

fn title(text: &'static str, colors: SemanticColors) -> Div {
    div()
        .text_size(px(Typo::DISPLAY_TITLE.size))
        .font_weight(Typo::DISPLAY_TITLE.weight)
        .text_color(colors.primary)
        .child(text)
}

/// `✳ Claude Code · ◎ Codex`, whisper-quiet: what is ready on this Mac.
fn agent_strip(agents: &[(AgentKind, SharedString)], colors: SemanticColors) -> Option<Div> {
    if agents.is_empty() {
        return None;
    }
    let mut strip = div()
        .flex()
        .flex_wrap()
        .justify_center()
        .items_center()
        .gap_x(px(6.0))
        .gap_y(px(4.0))
        .text_size(px(11.0))
        .text_color(colors.tertiary);
    for (index, (kind, name)) in agents.iter().enumerate() {
        if index > 0 {
            strip = strip.child(div().child("·"));
        }
        strip = strip.child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(AgentLogo::new(crate::surface_shell::ui_agent(kind), 14.0, colors).badged(false))
                .child(name.clone()),
        );
    }
    Some(strip)
}

fn header(view_agents: &[(AgentKind, SharedString)], text: &'static str, colors: SemanticColors) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(8.0))
        .child(title(text, colors))
        .when_some(agent_strip(view_agents, colors), |header, strip| {
            header.child(strip)
        })
}

/// The conversations page.
pub(crate) fn render(view: ImportView, actions: ImportActions, colors: SemanticColors) -> Div {
    let list = list(&view, &actions, colors);
    column()
        .gap(px(18.0))
        .child(header(&view.agents, t("import.title"), colors))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .when(view.browsing, |stack| stack.child(search_field(&view, colors)))
                .child(list)
                .when_some(browse_link(&view, &actions, colors), |stack, link| {
                    stack.child(div().pl(px(2.0)).child(link))
                }),
        )
        .child(controls(&view, &actions, colors))
}

fn list(view: &ImportView, actions: &ImportActions, colors: SemanticColors) -> AnyElement {
    let rows = view.rows.iter().enumerate().map(|(index, row)| {
        render_row(row, index > 0, actions, colors).into_any_element()
    });
    let empty = view.rows.is_empty().then(|| {
        div()
            .h(px(ROW_HEIGHT * 2.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(Typo::META.size))
            .text_color(colors.tertiary)
            .child(t("import.no_matches"))
            .into_any_element()
    });
    let frame = div()
        .id("import-list")
        .w_full()
        .rounded(px(Radius::CARD))
        .border_1()
        .border_color(colors.primary.alpha(0.08))
        .bg(colors.primary.alpha(0.015))
        .overflow_hidden();
    if view.browsing {
        frame
            .max_h(px(ROW_HEIGHT * BROWSE_VISIBLE_ROWS))
            .overflow_y_scroll()
            .children(rows)
            .children(empty)
            .into_any_element()
    } else {
        frame.children(rows).children(empty).into_any_element()
    }
}

fn render_row(
    row: &ImportRow,
    divided: bool,
    actions: &ImportActions,
    colors: SemanticColors,
) -> impl IntoElement {
    let toggle = Rc::clone(&actions.toggle);
    let focus = Rc::clone(&actions.focus);
    let id = row.id.clone();
    let hover_id = row.id.clone();
    let selector = format!("import-row-{}", row.id);
    let fill = if row.focused {
        colors.primary.alpha(0.05)
    } else {
        colors.primary.alpha(0.0)
    };
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .role(Role::CheckBox)
        .aria_label(row.title.clone())
        .w_full()
        .px(px(12.0))
        .flex()
        .flex_col()
        .justify_center()
        .min_h(px(ROW_HEIGHT))
        .when(row.preview.is_some(), |row| row.pb(px(8.0)))
        .when(divided, |row| row.border_t_1().border_color(colors.primary.alpha(0.06)))
        .bg(fill)
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.04)))
        .on_hover(move |hovered, window, cx| {
            if *hovered {
                focus(&hover_id, window, cx);
            }
        })
        .on_click(move |_, window, cx| toggle(&id, window, cx))
        .child(
            div()
                .h(px(ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(9.0))
                .child(checkbox(row.checked, colors))
                .child(
                    AgentLogo::new(crate::surface_shell::ui_agent(&row.kind), 16.0, colors)
                        .badged(false),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.primary)
                        .child(row.title.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(120.0))
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(row.folder.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .min_w(px(30.0))
                        .flex()
                        .justify_end()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(row.age.clone()),
                ),
        )
        .when_some(row.preview.clone(), |row, preview| {
            // Indented under the title, past the checkbox and the mark.
            row.child(
                div()
                    .pl(px(48.0))
                    .truncate()
                    .text_size(px(Typo::META.size))
                    .line_height(px(14.0))
                    .text_color(colors.secondary)
                    .child(preview),
            )
        })
}

fn checkbox(checked: bool, colors: SemanticColors) -> Div {
    div()
        .size(px(14.0))
        .flex_none()
        .rounded(px(4.0))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .map(|box_| {
            if checked {
                box_.border_color(colors.primary.alpha(0.55))
                    .bg(colors.primary.alpha(0.82))
                    .child(sf_symbol("checkmark", 8.5, colors.background))
            } else {
                box_.border_color(colors.primary.alpha(0.22))
            }
        })
}

fn search_field(view: &ImportView, colors: SemanticColors) -> gpui::Stateful<Div> {
    let empty = view.query.is_empty();
    div()
        .id("import-search")
        .debug_selector(|| "import-search".into())
        .w_full()
        .h(px(28.0))
        .px(px(10.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(0.12))
        .bg(colors.primary.alpha(0.03))
        .flex()
        .items_center()
        .gap(px(7.0))
        .child(sf_symbol("magnifyingglass", 11.0, colors.tertiary))
        .child(
            div()
                .flex()
                .items_center()
                .text_size(px(12.0))
                .when(empty, |field| {
                    field
                        .child(div().w(px(1.0)).h(px(14.0)).bg(colors.primary.alpha(0.8)))
                        .child(
                            div()
                                .text_color(colors.tertiary)
                                .child(t("import.search_placeholder")),
                        )
                })
                .when(!empty, |field| {
                    field
                        .child(div().text_color(colors.primary).child(view.query.clone()))
                        .child(div().w(px(1.0)).h(px(14.0)).bg(colors.primary.alpha(0.8)))
                }),
        )
}

fn browse_link(
    view: &ImportView,
    actions: &ImportActions,
    colors: SemanticColors,
) -> Option<AnyElement> {
    let browse = Rc::clone(&actions.browse);
    if view.browsing {
        return Some(quiet_link(
            "import-show-recent",
            t("import.show_recent"),
            None,
            colors,
            move |window, cx| browse(false, window, cx),
        ));
    }
    if view.more == 0 {
        return None;
    }
    let args: &[(&str, &dyn std::fmt::Display)] = &[("count", &view.more)];
    let label = match (view.more_truncated, view.more) {
        (true, _) => tf("import.browse_more_many", args),
        (false, 1) => tf("import.browse_more_one", args),
        (false, _) => tf("import.browse_more_other", args),
    };
    Some(quiet_link(
        "import-browse-more",
        label,
        None,
        colors,
        move |window, cx| browse(true, window, cx),
    ))
}

fn controls(view: &ImportView, actions: &ImportActions, colors: SemanticColors) -> Div {
    let start_fresh = Rc::clone(&actions.start_fresh);
    div()
        .flex()
        .flex_wrap()
        .justify_center()
        .items_center()
        .gap_x(px(16.0))
        .gap_y(px(8.0))
        .child(import_button(view, actions, colors))
        .child(quiet_link(
            "import-start-fresh",
            t("import.start_fresh"),
            None,
            colors,
            move |window, cx| start_fresh(window, cx),
        ))
}

fn import_button(view: &ImportView, actions: &ImportActions, colors: SemanticColors) -> AnyElement {
    let enabled = view.selected > 0 && !view.importing;
    let args: &[(&str, &dyn std::fmt::Display)] = &[("count", &view.selected)];
    let label: SharedString = if view.importing {
        t("import.importing").into()
    } else {
        match view.selected {
            0 => t("import.button_none").into(),
            1 => tf("import.button_one", args).into(),
            _ => tf("import.button_other", args).into(),
        }
    };
    let import = Rc::clone(&actions.import);
    bordered_button("import-confirm", label, Some("↩"), enabled, colors)
        .when(enabled, |button| {
            button.on_click(move |_, window, cx| import(window, cx))
        })
        .into_any_element()
}

/// The app's standard bordered control (the welcome's start button), with
/// its key set inside in tertiary.
fn bordered_button(
    id: &'static str,
    label: SharedString,
    key: Option<&'static str>,
    enabled: bool,
    colors: SemanticColors,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .role(Role::Button)
        .aria_label(label.clone())
        .h(px(28.0))
        .px(px(12.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(0.12))
        .bg(colors.primary.alpha(0.06))
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_size(px(12.0))
        .font_weight(Typo::ROW_EMPHASIZED.weight)
        .text_color(if enabled {
            colors.primary
        } else {
            colors.tertiary
        })
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                .active(move |button| button.bg(colors.primary.alpha(0.14)))
        })
        .child(label)
        .when_some(key, |button, key| {
            button.child(
                div()
                    .pl(px(2.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(key),
            )
        })
}

/// A Mac with no Claude Code or Codex history: the Agents that are ready, a
/// folder to open, and a way to start.
pub(crate) fn render_fresh(
    agents: &[(AgentKind, SharedString)],
    actions: FreshActions,
    colors: SemanticColors,
) -> Div {
    let open_folder = Rc::clone(&actions.open_folder);
    let moving = Rc::clone(&actions.moving);
    let start_label: SharedString = t("import.fresh.start").into();
    let shortcut = crate::commands::command(crate::commands::CommandId::NewDefaultSession)
        .shortcut_label();
    column()
        .gap(px(18.0))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.0))
                .child(sf_symbol("folder", 28.0, colors.tertiary))
                .child(header(agents, t("import.fresh.title"), colors)),
        )
        .child(
            bordered_button(
                "import-open-folder",
                t("import.fresh.open_folder").into(),
                None,
                true,
                colors,
            )
            .on_click(move |_, window, cx| open_folder(window, cx)),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_center()
                .items_center()
                .gap_x(px(16.0))
                .gap_y(px(6.0))
                .child(
                    div()
                        .id("import-start-session")
                        .debug_selector(|| "import-start-session".into())
                        .role(Role::Button)
                        .aria_label(start_label.clone())
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .cursor_pointer()
                        .hover(move |link| link.text_color(colors.primary))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(crate::commands::NewDefaultSession), cx)
                        })
                        .child(start_label)
                        .when_some(shortcut, |link, shortcut| {
                            link.child(div().text_color(colors.tertiary).child(shortcut))
                        }),
                )
                .child(quiet_link(
                    "import-moving",
                    t("import.fresh.moving"),
                    None,
                    colors,
                    move |window, cx| moving(window, cx),
                )),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_like_the_sidebar() {
        let now = 1_000_000_000_000.0;
        let ago = |seconds: f64| age_label(now, now - seconds * 1000.0);
        assert_eq!(ago(5.0), "now");
        assert_eq!(ago(12.0 * 60.0), "12m");
        assert_eq!(ago(2.0 * 3_600.0), "2h");
        assert_eq!(ago(30.0 * 3_600.0), "Yesterday");
        assert_eq!(ago(3.0 * 86_400.0), "3d");
        assert_eq!(ago(15.0 * 86_400.0), "2w");
        assert_eq!(ago(70.0 * 86_400.0), "2mo");
    }
}
