//! Searchable local skill catalogue, with all disk work outside rendering.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use diri_ui::{Icon, IconName, IconSize, SemanticColors};
use gpui::{
    AnyElement, ClipboardItem, Context, FocusHandle, FontWeight, KeyDownEvent, Render,
    ScrollHandle, ScrollStrategy, Task, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::i18n::{t, tf};
use crate::markdown::MarkdownDocument;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::skills_catalog::{self, Catalog, Scope};
use crate::store::SessionStore;

pub struct SkillsPage {
    store: Arc<RwLock<SessionStore>>,
    focus: FocusHandle,
    keyboard_target: usize,
    query: QueryEditor,
    scope: Scope,
    catalog: Catalog,
    matches: Vec<usize>,
    highlighted: usize,
    project: Option<PathBuf>,
    loaded: bool,
    loading: bool,
    generation: u64,
    scan_task: Option<Task<()>>,
    selected: Option<usize>,
    document: Option<MarkdownDocument>,
    instructions: Option<String>,
    detail_error: Option<String>,
    detail_task: Option<Task<()>>,
    copied: bool,
    copy_feedback_task: Option<Task<()>>,
    list_scroll: UniformListScrollHandle,
    detail_scroll: ScrollHandle,
}

impl SkillsPage {
    #[cfg(test)]
    pub(crate) fn seed_preview(&mut self, detail: bool) {
        self.catalog.skills = [
            ("Interface craftsmanship", "Design and refine interfaces with careful typography, layout, color, and interaction.", "Shared", Scope::Personal),
            ("PostgreSQL", "Review queries, diagnose slow connections, and improve database performance.", "Project · Claude", Scope::Project),
            ("Release checklist", "Prepare release notes and verify the steps required before publishing a new version.", "Project · Codex", Scope::Project),
            ("Spreadsheets", "Create, analyze, and verify workbooks, charts, and financial models.", "Codex plugin cache", Scope::Plugins),
        ].into_iter().map(|(name, description, source, scope)| skills_catalog::Skill {
            name: name.into(), description: description.into(), path: PathBuf::from(format!("/Users/example/.agents/skills/{}/SKILL.md", name.to_lowercase().replace(' ', "-"))),
            sources: [source.to_owned()].into_iter().collect(), scopes: vec![scope],
        }).collect();
        self.loaded = true;
        self.loading = false;
        self.refilter();
        if detail {
            self.selected = Some(0);
            self.document = Some(MarkdownDocument::parse(
                "# Interface craftsmanship\n\nTreat interface quality as a system of related decisions.\n\n## Review by impact\n\n- Make the primary action obvious.\n- Give repeated elements consistent spacing.\n- Keep keyboard focus visible.\n\n## Verification\n\nInspect realistic content in both light and dark appearances.",
            ));
            self.instructions = self.document.as_ref().map(MarkdownDocument::plain_text);
        }
    }

    pub fn new(store: Arc<RwLock<SessionStore>>, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            focus: cx.focus_handle(),
            keyboard_target: 0,
            query: QueryEditor::default(),
            scope: Scope::All,
            catalog: Catalog::default(),
            matches: Vec::new(),
            highlighted: 0,
            project: None,
            loaded: false,
            loading: false,
            generation: 0,
            scan_task: None,
            selected: None,
            document: None,
            instructions: None,
            detail_error: None,
            detail_task: None,
            copied: false,
            copy_feedback_task: None,
            list_scroll: UniformListScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
        }
    }

    pub fn open(&mut self, project: Option<PathBuf>, cx: &mut Context<Self>) {
        if !self.loaded || self.project != project {
            self.project = project;
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.clear_copy_feedback();
        // Detail-view targets (Back/Copy/Show file) mean nothing in the list.
        if self.selected.take().is_some() {
            self.keyboard_target = 6;
        }
        self.document = None;
        self.instructions = None;
        self.detail_error = None;
        self.detail_task = None;
        let home = diri_platform::home_dir()
            .map(|p| p.into_os_string())
            .map(PathBuf::from);
        let project = self.project.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let catalog = cx
                .background_executor()
                .spawn(async move {
                    home.map(|home| {
                        skills_catalog::scan(&skills_catalog::roots(&home, project.as_deref()))
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.loaded = true;
                this.catalog = catalog.unwrap_or_else(|| Catalog {
                    unreadable: 1,
                    ..Catalog::default()
                });
                this.refilter();
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn refilter(&mut self) {
        self.matches = self
            .catalog
            .skills
            .iter()
            .enumerate()
            .filter(|(_, skill)| skill.matches(self.query.text(), self.scope))
            .map(|(index, _)| index)
            .collect();
        self.highlighted = 0;
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn select_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.refilter();
        cx.notify();
    }

    fn open_skill(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(skill) = self.catalog.skills.get(index) else {
            return;
        };
        let path = skill.path.clone();
        self.clear_copy_feedback();
        self.selected = Some(index);
        self.document = None;
        self.instructions = None;
        self.detail_error = None;
        self.keyboard_target = 0;
        self.detail_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        let generation = self.generation;
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    skills_catalog::read(&path).map(|text| {
                        let instructions = skills_catalog::body(&text).to_owned();
                        (MarkdownDocument::parse(&instructions), instructions)
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.selected != Some(index) {
                    return;
                }
                match result {
                    Ok((document, instructions)) => {
                        this.document = Some(document);
                        this.instructions = Some(instructions);
                    }
                    Err(_) => this.detail_error = Some(t("settings.skills.read_failed").into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn clear_copy_feedback(&mut self) {
        self.copied = false;
        self.copy_feedback_task = None;
    }

    fn copy_instructions(&mut self, cx: &mut Context<Self>) {
        let Some(instructions) = &self.instructions else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(instructions.clone()));
        self.keyboard_target = 1;
        self.copied = true;
        // Replacing the task cancels the previous reset, extending feedback
        // from the most recent copy without an old timer clearing a new one.
        self.copy_feedback_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.clear_copy_feedback();
        self.selected = None;
        self.document = None;
        self.instructions = None;
        self.detail_task = None;
        self.keyboard_target = 6;
        cx.notify();
    }

    pub(crate) fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let key = &event.keystroke;
        let handled = match key.key.as_str() {
            "tab" => {
                let count = if self.selected.is_some() { 3 } else { 7 };
                self.keyboard_target = (self.keyboard_target
                    + if key.modifiers.shift { count - 1 } else { 1 })
                    % count;
                true
            }
            "escape" if self.selected.is_some() => {
                self.back(cx);
                true
            }
            "escape" if !self.query.is_empty() => {
                self.query.clear();
                self.refilter();
                true
            }
            "enter" | "space" if self.selected.is_some() => {
                match self.keyboard_target {
                    0 => self.back(cx),
                    1 => self.copy_instructions(cx),
                    _ => {
                        if let Some(index) = self.selected {
                            cx.reveal_path(&self.catalog.skills[index].path);
                        }
                    }
                }
                true
            }
            "up" | "down" if self.selected.is_none() && self.keyboard_target == 6 => {
                if key.key == "up" {
                    self.highlighted = self.highlighted.saturating_sub(1);
                } else {
                    self.highlighted =
                        (self.highlighted + 1).min(self.matches.len().saturating_sub(1));
                }
                self.list_scroll
                    .scroll_to_item(self.highlighted, ScrollStrategy::Center);
                true
            }
            "enter" | "space"
                if self.selected.is_none() && (key.key == "enter" || self.keyboard_target > 0) =>
            {
                match self.keyboard_target {
                    1..=4 => self.select_scope(Scope::ALL[self.keyboard_target - 1], cx),
                    5 => self.refresh(cx),
                    _ => {
                        if let Some(&index) = self.matches.get(self.highlighted) {
                            self.open_skill(index, cx);
                        }
                    }
                }
                true
            }
            _ if self.selected.is_none() && self.keyboard_target == 0 => {
                if let Some(edit) = query_editor::edit_for(key) {
                    match edit {
                        Edit::Local(local) => {
                            self.query.apply(local);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&self.query, cx)
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(&mut self.query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                self.query.insert(&text);
                            }
                        }
                    }
                    self.refilter();
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
        handled
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::colors_in(&self.store.read().expect("session store lock poisoned"))
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        target: usize,
        colors: SemanticColors,
    ) -> gpui::Stateful<gpui::Div> {
        self.button_with(id, label, label, target, colors)
    }

    fn button_with(
        &self,
        id: &'static str,
        label: &'static str,
        content: impl IntoElement,
        target: usize,
        colors: SemanticColors,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(label)
            .px(px(10.0))
            .h(px(30.0))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .border_1()
            .border_color(if self.keyboard_target == target {
                colors.primary.alpha(0.28)
            } else {
                colors.primary.alpha(0.08)
            })
            .text_size(px(12.0))
            .text_color(colors.secondary)
            .cursor_pointer()
            .hover(move |style| style.bg(colors.primary.alpha(0.07)))
            .active(move |style| style.bg(colors.primary.alpha(0.11)))
            .child(content)
    }
}

impl Render for SkillsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors();
        let body_height = (f32::from(window.viewport_size().height) - 330.0).max(180.0);
        let mut page = div()
            .id("skills-catalog")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| {
                this.handle_key(event, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus.focus(window, cx)),
            )
            .px(px(24.0))
            .pt(px(18.0))
            .pb(px(24.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .text_color(colors.primary)
            .child(
                div()
                    .text_size(px(20.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t("settings.tab.skills")),
            );
        if let Some(index) = self.selected {
            let skill = &self.catalog.skills[index];
            let path = skill.path.clone();
            let controls = div()
                .flex()
                .gap(px(8.0))
                .child(
                    self.button("skills-back", t("settings.skills.back"), 0, colors)
                        .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                )
                .child(
                    // Both labels are laid out and the hidden one collapses to
                    // zero height, so the button is as wide as the longer one
                    // in any locale and never moves when the label swaps. The
                    // accessible name stays the action, not the feedback.
                    self.button_with(
                        "skills-copy",
                        t("settings.skills.copy"),
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(
                                div()
                                    .child(t("settings.skills.copy"))
                                    .when(self.copied, |label| label.invisible().h_0()),
                            )
                            .child(
                                div()
                                    .child(t("settings.skills.copied"))
                                    .when(!self.copied, |label| label.invisible().h_0()),
                            ),
                        1,
                        colors,
                    )
                    .debug_selector(|| "skills-copy".into())
                    .flex_shrink_0()
                    .justify_center()
                    .when(self.instructions.is_none(), |button| button.opacity(0.45))
                    .on_click(cx.listener(|this, _, _, cx| this.copy_instructions(cx))),
                )
                .child(
                    self.button("skills-reveal", t("settings.skills.show_file"), 2, colors)
                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                );
            page = page.child(controls).child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(skill.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(skill.description.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(skill.source_label()),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .child(skill.path.to_string_lossy().into_owned()),
                    ),
            );
            let content: AnyElement = if let Some(document) = &self.document {
                crate::markdown_view::render_markdown(document, colors)
            } else {
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child(
                        self.detail_error
                            .clone()
                            .unwrap_or_else(|| t("settings.skills.reading").into()),
                    )
                    .into_any_element()
            };
            return page.child(
                div()
                    .id("skill-instructions")
                    .h(px(body_height))
                    .overflow_y_scroll()
                    .track_scroll(&self.detail_scroll)
                    .child(content),
            );
        }
        page = page.child(
            div()
                .text_size(px(13.0))
                .text_color(colors.secondary)
                .child(tf(
                    "settings.skills.intro",
                    &[("machine", &crate::platform::local_machine_label_lowercase())],
                )),
        );
        let search_label = if self.query.is_empty() && self.keyboard_target != 0 {
            div()
                .text_color(colors.tertiary)
                .child(t("settings.skills.search_placeholder"))
                .into_any_element()
        } else {
            crate::navigation::query_label(&self.query)
        };
        page = page.child(
            div()
                .id("skills-search")
                .role(gpui::Role::TextInput)
                .aria_label(t("settings.skills.search"))
                .h(px(36.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(colors.primary.alpha(if self.keyboard_target == 0 {
                    0.25
                } else {
                    0.1
                }))
                .bg(colors.primary.alpha(0.025))
                .text_size(px(13.0))
                .child(Icon::new(
                    IconName::Search,
                    IconSize::REGULAR,
                    colors.secondary,
                ))
                .child(search_label)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.keyboard_target = 0;
                    this.focus.focus(window, cx);
                    cx.notify();
                })),
        );
        let mut filters = div().flex().flex_wrap().gap(px(5.0));
        for (index, scope) in Scope::ALL.into_iter().enumerate() {
            filters = filters.child(
                self.button(
                    match scope {
                        Scope::All => "skills-all",
                        Scope::Personal => "skills-personal",
                        Scope::Project => "skills-project",
                        Scope::Plugins => "skills-plugins",
                    },
                    scope.label(),
                    index + 1,
                    colors,
                )
                .when(self.scope == scope, |button| {
                    button
                        .bg(colors.primary.alpha(0.08))
                        .text_color(colors.primary)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.keyboard_target = index + 1;
                    this.select_scope(scope, cx);
                })),
            );
        }
        page = page.child(
            div()
                .flex()
                .flex_wrap()
                .justify_between()
                .gap(px(8.0))
                .child(filters)
                .child(
                    self.button(
                        "skills-refresh",
                        if self.loading {
                            t("settings.skills.refreshing")
                        } else {
                            t("settings.skills.refresh")
                        },
                        5,
                        colors,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.keyboard_target = 5;
                        this.refresh(cx);
                    })),
                ),
        );
        let message = if self.loading && !self.loaded {
            t("settings.skills.looking").to_owned()
        } else if self.matches.is_empty() && self.scope == Scope::Project && self.project.is_none()
        {
            t("settings.skills.select_project").into()
        } else if self.matches.is_empty() && (!self.query.is_empty() || self.scope != Scope::All) {
            t("settings.skills.no_match").into()
        } else if self.matches.is_empty() {
            t("settings.skills.none_found").into()
        } else {
            tf(
                if self.matches.len() == 1 {
                    "settings.skills.count_one"
                } else {
                    "settings.skills.count_other"
                },
                &[("count", &self.matches.len())],
            )
        };
        page = page.child(
            div()
                .text_size(px(12.0))
                .text_color(colors.secondary)
                .child(message),
        );
        if !self.matches.is_empty() {
            let entity = cx.entity();
            page = page.child(
                uniform_list("skills-list", self.matches.len(), move |range, _, cx| {
                    entity.update(cx, |this, cx| {
                        let colors = this.colors();
                        range
                            .map(|row| {
                                let index = this.matches[row];
                                let skill = &this.catalog.skills[index];
                                div()
                                    .id(("skill", index))
                                    .role(gpui::Role::Button)
                                    .aria_label(tf(
                                        "settings.skills.open_skill",
                                        &[
                                            ("name", &skill.name),
                                            ("description", &skill.description),
                                        ],
                                    ))
                                    .w_full()
                                    .h(px(82.0))
                                    .px(px(12.0))
                                    .py(px(10.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .rounded(px(8.0))
                                    .cursor_pointer()
                                    .when(
                                        this.keyboard_target == 6 && row == this.highlighted,
                                        |item| item.bg(colors.primary.alpha(0.08)),
                                    )
                                    .hover(move |style| style.bg(colors.primary.alpha(0.05)))
                                    .child(
                                        div()
                                            .flex()
                                            .justify_between()
                                            .gap(px(12.0))
                                            .child(
                                                div()
                                                    .min_w(px(0.0))
                                                    .truncate()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(skill.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .max_w(px(220.0))
                                                    .truncate()
                                                    .text_size(px(11.0))
                                                    .text_color(colors.tertiary)
                                                    .child(skill.source_label()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(12.0))
                                            .text_color(colors.secondary)
                                            .child(if skill.description.is_empty() {
                                                t("settings.skills.open_to_read").into()
                                            } else {
                                                skill.description.clone()
                                            }),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(10.0))
                                            .text_color(colors.tertiary)
                                            .child(skill.path.to_string_lossy().into_owned()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.highlighted = row;
                                        this.open_skill(index, cx);
                                    }))
                            })
                            .collect()
                    })
                })
                .h(px(body_height))
                .track_scroll(&self.list_scroll),
            );
        }
        if self.catalog.unreadable > 0 || self.catalog.limited {
            page = page.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(format!(
                        "{}{}",
                        tf(
                            "settings.skills.unreadable",
                            &[("count", &self.catalog.unreadable)],
                        ),
                        if self.catalog.limited {
                            t("settings.skills.size_limit")
                        } else {
                            ""
                        }
                    )),
            );
        }
        page.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(t("settings.skills.plugin_note")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, size};
    use std::time::Duration;

    fn preview(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<SkillsPage>, &mut gpui::VisualTestContext) {
        let (store, _) = SessionStore::headless(Default::default());
        let (page, cx) = cx.add_window_view(move |window, cx| {
            let mut page = SkillsPage::new(Arc::new(RwLock::new(store)), cx);
            page.seed_preview(true);
            page.focus.focus(window, cx);
            page
        });
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        (page, cx)
    }

    #[gpui::test]
    fn pointer_enter_and_space_copy_without_moving_the_control(cx: &mut TestAppContext) {
        let (page, cx) = preview(cx);
        let bounds = cx.debug_bounds("skills-copy").unwrap();
        let instructions = page.read_with(cx, |page, _| page.instructions.clone().unwrap());
        for activation in ["pointer", "enter", "space"] {
            if activation == "pointer" {
                cx.simulate_click(bounds.center(), Modifiers::default());
            } else {
                cx.simulate_keystrokes(activation);
            }
            cx.run_until_parked();
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(instructions.clone())
            );
            assert!(page.read_with(cx, |page, _| page.copied));
            assert_eq!(page.read_with(cx, |page, _| page.keyboard_target), 1);
            assert_eq!(cx.debug_bounds("skills-copy").unwrap(), bounds);
            // Each activation restarts the timer, rather than the first copy clearing it.
            cx.executor().advance_clock(Duration::from_millis(1500));
            cx.run_until_parked();
            assert!(page.read_with(cx, |page, _| page.copied));
        }
        cx.executor().advance_clock(Duration::from_millis(501));
        cx.run_until_parked();
        assert!(!page.read_with(cx, |page, _| page.copied));
        assert_eq!(cx.debug_bounds("skills-copy").unwrap(), bounds);
        // Focus stays on the copy action even after the feedback expires.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(page.read_with(cx, |page, _| page.copied));
    }

    #[gpui::test]
    fn navigation_clears_feedback_and_unavailable_instructions_never_copy(cx: &mut TestAppContext) {
        let (page, cx) = preview(cx);
        let copy = cx.debug_bounds("skills-copy").unwrap().center();
        cx.simulate_click(copy, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!page.read_with(cx, |page, _| page.copied));
        assert!(page.read_with(cx, |page, _| page.selected.is_none()));
        // A fixture-only path is never read: cancel the load before driving the UI.
        page.update(cx, |page, cx| {
            page.open_skill(1, cx);
            page.detail_task = None;
        });
        cx.run_until_parked();
        assert!(!page.read_with(cx, |page, _| page.copied));
        for error in [None, Some("Unreadable sample skill".to_owned())] {
            page.update(cx, |page, cx| {
                page.detail_error = error;
                cx.notify();
            });
            cx.write_to_clipboard(ClipboardItem::new_string("unchanged".into()));
            page.update(cx, |page, _| page.keyboard_target = 0);
            let copy = cx.debug_bounds("skills-copy").unwrap().center();
            cx.simulate_click(copy, Modifiers::default());
            cx.run_until_parked();
            // A dimmed Copy that copied nothing does not take the focus ring.
            assert_eq!(page.read_with(cx, |page, _| page.keyboard_target), 0);
            page.update(cx, |page, _| page.keyboard_target = 1);
            cx.simulate_keystrokes("enter space");
            cx.run_until_parked();
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("unchanged")
            );
            assert!(!page.read_with(cx, |page, _| page.copied));
            assert!(page.read_with(cx, |page, _| page.selected == Some(1)));
        }
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        assert!(!page.read_with(cx, |page, _| page.copied));
    }

    #[gpui::test]
    fn refresh_from_a_skill_returns_focus_to_the_list(cx: &mut TestAppContext) {
        let (page, cx) = preview(cx);
        page.update(cx, |page, cx| {
            page.keyboard_target = 1;
            page.detail_error = Some("Unreadable sample skill".to_owned());
            page.refresh(cx);
            // The fixture catalog is not on disk; keep it instead of scanning.
            page.scan_task = None;
        });
        page.read_with(cx, |page, _| {
            assert!(page.selected.is_none());
            assert!(page.detail_error.is_none());
            assert_eq!(page.keyboard_target, 6);
        });
    }
}
