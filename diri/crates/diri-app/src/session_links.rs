//! One contextual home for session links. Status is always attached by URL.
use super::*;
use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText};
use crate::palette_chrome::{PaletteTooltip, scroll_fades};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use diri_proto::{ArtifactKind, PrCheck, PullRequestStatus, SessionArtifact};
use diri_ui::{Icon, IconName};
use gpui::{
    Anchor, Animation, AnimationExt, ClickEvent, FontWeight, Pixels, Point, ScrollStrategy,
    UniformListScrollHandle, anchored, canvas, deferred, ease_out_quint, point, rgba, uniform_list,
};
use std::{cell::Cell, rc::Rc};
const ROW_HEIGHT: f32 = 44.0;
pub(super) struct SessionLinks {
    open: bool,
    pull_request: Option<String>,
    /// Filters the home page only; a PR's detail page is short and fixed.
    query: QueryEditor,
    selected: usize,
    parent_selection: usize,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    anchor: Rc<Cell<Point<Pixels>>>,
}
impl SessionLinks {
    pub(super) fn new(cx: &mut Context<TerminalPane>) -> Self {
        Self {
            open: false,
            pull_request: None,
            query: QueryEditor::default(),
            selected: 0,
            parent_selection: 0,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            anchor: Rc::new(Cell::new(point(px(0.0), px(0.0)))),
        }
    }
    pub(super) fn close(&mut self) {
        self.open = false;
        self.pull_request = None;
        self.query.clear();
        self.selected = 0;
    }
}
#[derive(Clone, Debug, PartialEq)]
enum LinkAction {
    Open(String),
    PullRequest(String),
    Account,
}
#[derive(Clone, Debug)]
struct LinkRow {
    title: String,
    subtitle: String,
    icon: IconName,
    status: Option<(String, gpui::Rgba)>,
    action: LinkAction,
    details: Option<String>,
}
fn pr_state(pr: &PullRequestStatus) -> &'static str {
    match pr.state.as_str() {
        "MERGED" => "Merged",
        "CLOSED" => "Closed",
        _ if pr.is_draft => "Draft",
        _ => "Open",
    }
}
fn pr_summary(pr: &PullRequestStatus) -> (String, gpui::Rgba) {
    match pr.state.as_str() {
        "MERGED" => ("Merged".into(), rgba(0xaf7cf7ff)),
        "CLOSED" => ("Closed".into(), Ink::DANGER),
        _ => check_summary(pr).unwrap_or_else(|| {
            (
                pr_state(pr).into(),
                if pr.is_draft {
                    Ink::GENERIC_WORKING
                } else {
                    Ink::FRESH
                },
            )
        }),
    }
}

fn check_summary(pr: &PullRequestStatus) -> Option<(String, gpui::Rgba)> {
    if pr.checks_failed > 0 {
        Some((
            format!(
                "{} check{} failed",
                pr.checks_failed,
                if pr.checks_failed == 1 { "" } else { "s" }
            ),
            Ink::DANGER,
        ))
    } else if pr.checks_pending > 0 {
        Some((
            format!(
                "{} check{} running",
                pr.checks_pending,
                if pr.checks_pending == 1 { "" } else { "s" }
            ),
            Ink::ATTENTION,
        ))
    } else if pr.checks_passed > 0 {
        Some(("Checks passed".into(), Ink::FRESH))
    } else {
        None
    }
}
fn active_check_attention(session: &SessionRecord) -> Option<(gpui::Rgba, String)> {
    let active = session
        .pull_requests
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|pr| !matches!(pr.state.as_str(), "MERGED" | "CLOSED"))
        .collect::<Vec<_>>();
    let failed = active
        .iter()
        .filter(|pr| pr.checks_failed > 0)
        .map(|pr| format!("#{}", pr.number))
        .collect::<Vec<_>>();
    if !failed.is_empty() {
        return Some((
            Ink::DANGER,
            format!("Checks need attention in {}", failed.join(", ")),
        ));
    }
    let running = active
        .iter()
        .filter(|pr| pr.checks_pending > 0)
        .map(|pr| format!("#{}", pr.number))
        .collect::<Vec<_>>();
    (!running.is_empty()).then(|| {
        (
            Ink::ATTENTION,
            format!("Checks running in {}", running.join(", ")),
        )
    })
}

fn repository_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            let mut segments = url.path_segments()?;
            Some(format!("{}/{}", segments.next()?, segments.next()?))
        })
        .unwrap_or_else(|| url_host(url))
}
fn pr_row(pr: &PullRequestStatus) -> LinkRow {
    LinkRow {
        title: pr
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Pull request #{}", pr.number)),
        subtitle: format!("#{} · {}", pr.number, repository_name(&pr.url)),
        icon: if pr.state == "MERGED" {
            IconName::Merge
        } else {
            IconName::PullRequest
        },
        status: Some(pr_summary(pr)),
        action: LinkAction::Open(pr.url.clone()),
        details: Some(pr.url.clone()),
    }
}
fn artifact_row(artifact: &SessionArtifact) -> LinkRow {
    let host = url_host(&artifact.url);
    let (title, icon) = match artifact.kind {
        ArtifactKind::PullRequest => (
            pr_number(&artifact.url)
                .map_or_else(|| "Pull request".into(), |n| format!("Pull request #{n}")),
            IconName::PullRequest,
        ),
        ArtifactKind::LinearIssue => (
            linear_key(&artifact.url).unwrap_or_else(|| "Linear issue".into()),
            IconName::Checklist,
        ),
        ArtifactKind::Preview => ("Preview".into(), IconName::Monitor),
        _ => (
            match host.as_str() {
                "notion.so" | "www.notion.so" | "notion.site" => "Notion page".into(),
                "docs.google.com" if artifact.url.contains("/document/") => "Google Doc".into(),
                "docs.google.com" if artifact.url.contains("/spreadsheets/") => {
                    "Google Sheet".into()
                }
                "figma.com" | "www.figma.com" => "Figma design".into(),
                _ if host.ends_with(".notion.site") => "Notion page".into(),
                _ => host,
            },
            IconName::ExternalLink,
        ),
    };
    LinkRow {
        title,
        subtitle: artifact
            .url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_owned(),
        icon,
        status: None,
        action: LinkAction::Open(artifact.url.clone()),
        details: None,
    }
}
// The closed toolbar counts borrowed URLs; it does not clone titles, checks,
// or build any menu rows on terminal updates.
fn link_count(session: &SessionRecord) -> usize {
    use std::borrow::Cow;
    let mut urls: HashSet<Cow<'_, str>> = session
        .artifacts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|a| Cow::Borrowed(a.url.as_str()))
        .chain(
            session
                .pull_requests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|pr| Cow::Borrowed(pr.url.as_str())),
        )
        .collect();
    for port in session.listening_ports.as_deref().unwrap_or_default() {
        urls.insert(Cow::Owned(format!("http://localhost:{}", port.port)));
    }
    urls.len()
}

/// A PR is one destination, regardless of how many checks or comments it has.
fn session_rows(session: &SessionRecord) -> Vec<LinkRow> {
    let statuses = session.pull_requests.as_deref().unwrap_or_default();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for artifact in session.artifacts.as_deref().unwrap_or_default() {
        if !seen.insert(artifact.url.clone()) {
            continue;
        }
        rows.push(
            statuses
                .iter()
                .find(|pr| pr.url == artifact.url)
                .map_or_else(|| artifact_row(artifact), pr_row),
        );
    }
    for pr in statuses {
        if seen.insert(pr.url.clone()) {
            rows.push(pr_row(pr));
        }
    }
    for port in session.listening_ports.as_deref().unwrap_or_default() {
        let url = format!("http://localhost:{}", port.port);
        if seen.insert(url.clone()) {
            rows.push(LinkRow {
                title: "Local preview".into(),
                subtitle: format!("{} · localhost:{}", port.process_name, port.port),
                icon: IconName::Monitor,
                status: None,
                action: LinkAction::Open(url),
                details: None,
            });
        }
    }
    rows
}
/// Rows matching `query` across what the row shows and where it goes, best
/// match first; ties keep the chat's order.
fn filter_rows(rows: Vec<LinkRow>, query: &str) -> Vec<LinkRow> {
    let query = FuzzyQuery::new(query);
    if query.is_empty() {
        return rows;
    }
    let mut matcher = FuzzyMatcher::text();
    let mut scored: Vec<_> = rows
        .into_iter()
        .filter_map(|row| {
            let url = match &row.action {
                LinkAction::Open(url) | LinkAction::PullRequest(url) => url.as_str(),
                LinkAction::Account => "",
            };
            let haystack = PreparedText::new(&format!("{} {} {url}", row.title, row.subtitle));
            query
                .score(&haystack, &mut matcher)
                .map(|score| (score, row))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, row)| row).collect()
}
fn detail_rows(pr: &PullRequestStatus) -> Vec<LinkRow> {
    let mut rows = vec![LinkRow {
        title: "Open on GitHub".into(),
        subtitle: format!(
            "{} · +{} −{} · {} files",
            pr_state(pr),
            pr.additions,
            pr.deletions,
            pr.changed_files
        ),
        icon: IconName::ExternalLink,
        status: None,
        action: LinkAction::Open(pr.url.clone()),
        details: None,
    }];
    for check in sorted_checks(pr) {
        let (label, icon, tone) = match check.result.as_str() {
            "pass" => ("Passed", IconName::CheckCircle, Ink::FRESH),
            "fail" => ("Failed", IconName::CloseCircle, Ink::DANGER),
            "pending" => ("Running", IconName::Clock, Ink::ATTENTION),
            _ => ("Unknown", IconName::Clock, Ink::ATTENTION),
        };
        rows.push(LinkRow {
            title: check.name,
            subtitle: check.detail.filter(|s| !s.is_empty()).unwrap_or_default(),
            icon,
            status: Some((label.into(), tone)),
            details: None,
            action: LinkAction::Open(
                check
                    .url
                    .unwrap_or_else(|| format!("{}/checks", pr.url.trim_end_matches('/'))),
            ),
        });
    }
    if pr.checks.as_ref().is_none_or(Vec::is_empty) {
        rows.push(LinkRow {
            title: "Checks".into(),
            subtitle: check_summary(pr)
                .map_or_else(|| "No checks reported".into(), |(label, _)| label),
            icon: IconName::CheckCircle,
            status: None,
            details: None,
            action: LinkAction::Open(format!("{}/checks", pr.url.trim_end_matches('/'))),
        });
    }
    if pr.comment_count + pr.review_count > 0 || pr.total_threads.unwrap_or(0) > 0 {
        rows.push(LinkRow {
            title: "Discussion".into(),
            subtitle: comments_help(pr),
            icon: IconName::Comment,
            status: None,
            details: None,
            action: LinkAction::Open(format!(
                "{}#discussion_bucket",
                pr.url.trim_end_matches('/')
            )),
        });
    }
    rows
}
impl TerminalPane {
    fn links_rows(&self, session: &SessionRecord) -> Vec<LinkRow> {
        if let Some(url) = &self.session_links.pull_request
            && let Some(pr) = session
                .pull_requests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|pr| &pr.url == url)
        {
            return detail_rows(pr);
        }
        filter_rows(session_rows(session), self.session_links.query.text())
    }
    /// The account row is a context action, not a link, so a search hides it.
    fn links_has_account(&self, session: &SessionRecord) -> bool {
        self.session_links.pull_request.is_none()
            && self.session_links.query.is_empty()
            && session.account_profile.is_some()
    }
    /// Applies a keystroke to the search field when it is a text edit.
    fn links_edit_query(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(edit) = query_editor::edit_for(&event.keystroke) else {
            return;
        };
        let query = &mut self.session_links.query;
        let changed = match edit {
            Edit::Local(local) => query.apply(local),
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(query, cx);
                false
            }
            Edit::Clipboard(ClipboardEdit::Cut) => query_editor::cut_selection(query, cx),
            Edit::Clipboard(ClipboardEdit::Paste) => cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| query.insert(&text)),
        };
        if changed {
            self.session_links.selected = 0;
        }
    }
    pub(super) fn close_session_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_links.close();
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn links_back(&mut self, cx: &mut Context<Self>) {
        self.session_links.pull_request = None;
        self.session_links.selected = self.session_links.parent_selection;
        self.session_links
            .scroll
            .scroll_to_item(self.session_links.selected, ScrollStrategy::Nearest);
        cx.notify();
    }
    fn activate_link(
        &mut self,
        action: &LinkAction,
        copy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if copy {
            if let LinkAction::Open(url) | LinkAction::PullRequest(url) = action {
                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
            }
            return;
        }
        match action {
            LinkAction::PullRequest(url) => {
                self.session_links.pull_request = Some(url.clone());
                self.session_links.parent_selection = self.session_links.selected;
                self.session_links.selected = 0;
                self.session_links
                    .scroll
                    .scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
            LinkAction::Open(url) => {
                cx.open_url(url);
                self.close_session_links(window, cx);
            }
            LinkAction::Account => {
                self.close_session_links(window, cx);
                self.open_account_continuation(cx);
            }
        }
    }
    fn links_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let rows = self.links_rows(&session);
        let has_account = self.links_has_account(&session);
        let action_count = rows.len() + usize::from(has_account);
        let searching = self.session_links.pull_request.is_none();
        let query = &self.session_links.query;
        // → drills into a PR only once the caret has nowhere left to go.
        let caret_at_end = query.selection().is_none() && query.cursor() == query.text().len();
        let plain = !event.keystroke.modifiers.modified();
        match event.keystroke.key.as_str() {
            "escape" if searching && !self.session_links.query.is_empty() => {
                self.session_links.query.clear();
                self.session_links.selected = 0;
            }
            "escape" => self.close_session_links(window, cx),
            "left" | "backspace" if self.session_links.pull_request.is_some() => {
                self.links_back(cx)
            }
            "down" => {
                self.session_links.selected =
                    (self.session_links.selected + 1).min(action_count.saturating_sub(1))
            }
            "up" => self.session_links.selected = self.session_links.selected.saturating_sub(1),
            "right"
                if plain
                    && (!searching || caret_at_end)
                    && rows
                        .get(self.session_links.selected)
                        .is_some_and(|row| row.details.is_some()) =>
            {
                let url = rows[self.session_links.selected].details.as_ref().unwrap();
                self.activate_link(&LinkAction::PullRequest(url.clone()), false, window, cx);
            }
            "enter" => {
                if let Some(row) = rows.get(self.session_links.selected) {
                    self.activate_link(&row.action, event.keystroke.modifiers.alt, window, cx);
                } else if has_account {
                    self.activate_link(&LinkAction::Account, false, window, cx);
                }
            }
            _ if searching => self.links_edit_query(event, cx),
            _ => {}
        }
        self.session_links
            .scroll
            .scroll_to_item(self.session_links.selected, ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
    }
    pub(super) fn render_session_links_trigger(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = link_count(session);
        let attention = active_check_attention(session);
        let help = attention.as_ref().map_or_else(
            || format!("Links and session details · {count} links"),
            |(_, help)| help.clone(),
        );
        let open = self.session_links.open;
        let anchor = self.session_links.anchor.clone();
        div()
            .relative()
            .child(
                canvas(
                    move |bounds, _, _| anchor.set(bounds.bottom_right()),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .id("session-links-trigger")
            .debug_selector(|| "session-links-trigger".into())
            .h(px(Metrics::TOOLBAR_CONTROL_SIZE))
            .px(px(6.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(Radius::ROW))
            .when(open, |el| el.bg(Fill::selected(colors, true)))
            .hover(move |el| el.bg(Fill::subtle(colors)))
            .cursor_pointer()
            .text_size(px(Typo::ROW.size))
            .text_color(colors.secondary)
            .when_some(attention, |el, (tone, _)| {
                el.child(div().size(px(5.0)).flex_none().rounded_full().bg(tone))
            })
            .child(Icon::new(IconName::ExternalLink, 14.0, colors.secondary))
            .when(count > 0, |el| {
                el.child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(count.to_string()),
                )
            })
            .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(help.clone(), colors)).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| {
                if this.session_links.open {
                    this.close_session_links(window, cx);
                } else {
                    this.session_links.open = true;
                    if let Some(session) = this.selected_session() {
                        this.runtime
                            .store
                            .read()
                            .unwrap()
                            .refresh_session_links(session.id.clone());
                    }
                    this.session_links.selected = 0;
                    this.session_links
                        .scroll
                        .scroll_to_item(0, ScrollStrategy::Top);
                    this.session_links.focus.focus(window, cx);
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }
    fn render_link_row(
        &self,
        index: usize,
        row: &LinkRow,
        row_height: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let action = row.action.clone();
        let help = match &action {
            LinkAction::Open(url) | LinkAction::PullRequest(url) => {
                format!("Open link\n{}\n{}\n{}", row.title, row.subtitle, url)
            }
            LinkAction::Account => row.subtitle.clone(),
        };
        let selected = index == self.session_links.selected;
        let metadata = div()
            .h(px(14.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(Typo::META.size))
            .line_height(px(14.0))
            .text_color(colors.secondary)
            .when(!row.subtitle.is_empty(), |el| {
                el.child(div().min_w(px(0.0)).truncate().child(row.subtitle.clone()))
            })
            .when_some(row.status.clone(), |el, (label, tone)| {
                el.when(!row.subtitle.is_empty(), |el| {
                    el.child(div().flex_none().text_color(colors.tertiary).child("·"))
                })
                .child(div().flex_none().text_color(tone).child(label))
            });
        div()
            .id(("session-link", index))
            .debug_selector(move || format!("session-link-{index}"))
            .h(px(row_height))
            .px(px(6.0))
            .child(
                div()
                    .h_full()
                    .rounded(px(Radius::ROW))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(selected, |el| el.bg(Fill::subtle(colors)))
                    .hover(move |el| el.bg(Fill::selected(colors, true)))
                    .cursor_pointer()
                    .child(div().w(px(16.0)).flex_none().child(Icon::new(
                        row.icon,
                        16.0,
                        colors.secondary,
                    )))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(Typo::ROW.size))
                                    .line_height(px(16.0))
                                    .text_color(colors.primary)
                                    .child(row.title.clone()),
                            )
                            .when(!row.subtitle.is_empty() || row.status.is_some(), |el| {
                                el.child(metadata)
                            }),
                    )
                    .child(
                        div()
                            .w(px(24.0))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when_some(row.details.clone(), |el, url| {
                                el.child(
                                    div()
                                        .id(("session-link-details", index))
                                        .debug_selector(move || {
                                            format!("session-link-details-{index}")
                                        })
                                        .size(px(24.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(Radius::CHIP))
                                        .hover(move |el| el.bg(Fill::selected(colors, true)))
                                        .child(Icon::new(
                                            IconName::ChevronRight,
                                            12.0,
                                            colors.secondary,
                                        ))
                                        .tooltip(move |_, cx| {
                                            cx.new(|_| {
                                                PaletteTooltip(
                                                    "Checks and discussion · →".into(),
                                                    colors,
                                                )
                                            })
                                            .into()
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.session_links.selected = index;
                                            let url = url.clone();
                                            this.in_main_window(
                                                window,
                                                cx,
                                                move |this, window, cx| {
                                                    this.activate_link(
                                                        &LinkAction::PullRequest(url),
                                                        false,
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            );
                                            cx.stop_propagation();
                                        })),
                                )
                            })
                            .when(row.details.is_none(), |el| {
                                el.child(Icon::new(IconName::ExternalLink, 12.0, colors.tertiary))
                            }),
                    ),
            )
            .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(help.clone(), colors)).into())
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.session_links.selected = index;
                let (action, alt) = (action.clone(), event.modifiers().alt);
                this.in_main_window(window, cx, move |this, window, cx| {
                    this.activate_link(&action, alt, window, cx);
                });
                cx.stop_propagation();
            }))
            .into_any_element()
    }
    /// The Links popover's rows for `session`, without any host chrome.
    fn links_content(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.session_links.open {
            return None;
        }
        // Resource updates can remove a PR while its detail page is open.
        if self.session_links.pull_request.as_ref().is_some_and(|url| {
            !session
                .pull_requests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|pr| &pr.url == url)
        }) {
            self.session_links.pull_request = None;
            self.session_links.selected = self.session_links.parent_selection;
        }
        let rows = self.links_rows(session);
        let has_account = self.links_has_account(session);
        let action_count = rows.len() + usize::from(has_account);
        self.session_links.selected = self
            .session_links
            .selected
            .min(action_count.saturating_sub(1));
        let row_count = rows.len();
        let pr = self.session_links.pull_request.as_ref().and_then(|url| {
            session
                .pull_requests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|pr| &pr.url == url)
        });
        let query = &self.session_links.query;
        let title = match pr {
            Some(pr) => div()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.secondary)
                .child(format!("Pull request #{}", pr.number))
                .into_any_element(),
            None if query.is_empty() => div()
                .truncate()
                .text_color(colors.tertiary)
                .child("Search links")
                .into_any_element(),
            None => div()
                .truncate()
                .text_color(colors.primary)
                .child(crate::navigation::query_label(query))
                .into_any_element(),
        };
        let header = div()
            .h(px(34.0))
            .pl(px(14.0))
            .pr(px(14.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .when(pr.is_none(), |el| {
                el.child(
                    div()
                        .w(px(20.0))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(Icon::new(IconName::Search, 12.0, colors.tertiary)),
                )
            })
            .when(pr.is_some(), |el| {
                el.child(
                    div()
                        .id("session-links-back")
                        .debug_selector(|| "session-links-back".into())
                        .size(px(20.0))
                        .rounded(px(Radius::CHIP))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(move |el| el.bg(Fill::subtle(colors)))
                        .child(Icon::new(IconName::ChevronLeft, 12.0, colors.secondary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.links_back(cx);
                            cx.stop_propagation();
                        })),
                )
            })
            .child(
                div()
                    .id("session-links-search")
                    .debug_selector(|| "session-links-search".into())
                    .min_w(px(0.0))
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .when(pr.is_none(), |el| el.cursor_text())
                    .text_size(px(Typo::META.size))
                    .line_height(px(14.0))
                    .child(title),
            )
            .child(
                div()
                    .id("session-links-close")
                    .size(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .hover(move |el| el.bg(Fill::subtle(colors)))
                    .child(Icon::new(IconName::Close, 12.0, colors.tertiary))
                    .tooltip(move |_, cx| {
                        cx.new(|_| PaletteTooltip("Close · Esc".into(), colors))
                            .into()
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_session_links(window, cx);
                        cx.stop_propagation();
                    })),
            );
        let row_height = ROW_HEIGHT;
        let list_height = (row_count as f32 * row_height)
            .min(364.0)
            .min((f32::from(self.main_viewport.height) - 230.0).max(ROW_HEIGHT));
        let body = if rows.is_empty() {
            let empty = if query.is_empty() || pr.is_some() {
                "Links shared in this chat appear here.".to_owned()
            } else {
                format!("No links match \u{201c}{}\u{201d}", query.text().trim())
            };
            div()
                .px(px(14.0))
                .pt(px(6.0))
                .pb(px(14.0))
                .text_size(px(13.0))
                .text_color(colors.secondary)
                .child(empty)
                .into_any_element()
        } else {
            let entity = cx.entity();
            div()
                .relative()
                .h(px(list_height))
                .overflow_hidden()
                .child(
                    uniform_list("session-links-list", row_count, move |range, _, cx| {
                        entity.update(cx, |this, cx| {
                            range
                                .map(|index| {
                                    this.render_link_row(
                                        index,
                                        &rows[index],
                                        row_height,
                                        colors,
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .h(px(list_height))
                    .track_scroll(&self.session_links.scroll),
                )
                .child(scroll_fades(self.session_links.scroll.clone(), colors))
                .into_any_element()
        };
        let body = div()
            .id(SharedString::from(
                self.session_links
                    .pull_request
                    .clone()
                    .unwrap_or_else(|| "links-home".into()),
            ))
            .child(body);
        let body = if cx.reduce_motion() {
            body.into_any_element()
        } else {
            body.with_animation(
                "links-page",
                Animation::new(Duration::from_millis(140)).with_easing(ease_out_quint()),
                move |el, value| el.opacity(value),
            )
            .into_any_element()
        };
        let mut content = div()
            .font_weight(FontWeight::NORMAL)
            .line_height(px(16.0))
            .flex()
            .flex_col()
            .child(header)
            .child(div().pb(px(6.0)).child(body));
        if pr.is_none() && query.is_empty() {
            let host = session
                .host
                .as_ref()
                .map(|host| self.runtime.store.read().unwrap().host_display_name(host));
            if session.git_branch.is_some() || host.is_some() || session.account_profile.is_some() {
                let mut context = div()
                    .mx(px(14.0))
                    .py(px(10.0))
                    .border_t_1()
                    .border_color(colors.floating_stroke())
                    .flex()
                    .flex_col()
                    .gap(px(6.0));
                for (context_index, (icon, label, value)) in [
                    (IconName::Branch, "Branch", session.git_branch.clone()),
                    (IconName::Server, "Running on", host),
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(value) = value {
                        context = context.child(
                            div()
                                .id(("session-links-context", context_index))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(colors.secondary)
                                .child(
                                    div()
                                        .w(px(16.0))
                                        .flex_none()
                                        .flex()
                                        .justify_center()
                                        .child(Icon::new(icon, 14.0, colors.tertiary)),
                                )
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .flex_1()
                                        .truncate()
                                        .text_color(colors.secondary)
                                        .child(value.clone()),
                                )
                                .tooltip(move |_, cx| {
                                    cx.new(|_| PaletteTooltip(format!("{label} · {value}"), colors))
                                        .into()
                                }),
                        );
                    }
                }
                if let Some(profile) = &session.account_profile {
                    context = context.child(
                        div()
                            .id("session-links-account")
                            .h(px(28.0))
                            .rounded(px(Radius::ROW))
                            .when(self.session_links.selected == row_count, |el| {
                                el.bg(Fill::selected(colors, true))
                            })
                            .hover(move |el| el.bg(Fill::subtle(colors)))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(11.0))
                            .line_height(px(14.0))
                            .text_color(colors.secondary)
                            .cursor_pointer()
                            .child(div().w(px(16.0)).flex().justify_center().child(Icon::new(
                                IconName::Account,
                                14.0,
                                colors.tertiary,
                            )))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(colors.primary)
                                    .child(profile.label.clone()),
                            )
                            .child(Icon::new(IconName::ChevronRight, 14.0, colors.tertiary))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.in_main_window(window, cx, |this, window, cx| {
                                    this.activate_link(&LinkAction::Account, false, window, cx);
                                });
                                cx.stop_propagation();
                            })),
                    );
                }
                content = content.child(context);
            }
        }
        Some(content.into_any_element())
    }

    /// How wide the Links popover may be inside `viewport`.
    fn links_width(viewport: gpui::Size<Pixels>) -> f32 {
        380.0_f32.min(f32::from(viewport.width) - 24.0).max(120.0)
    }

    /// The Links popover's pixels for its floating panel.
    pub(super) fn links_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.selected_session()?;
        let colors = self.panel_colors();
        let content = self.links_content(&session, colors, cx)?;
        let width = Self::links_width(self.main_viewport);
        Some(crate::floating::surface(colors, Radius::PANEL, width, content).into_any_element())
    }

    pub(super) fn render_session_links(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let content = self.links_content(session, colors, cx)?;
        let position = self.session_links.anchor.get() + point(px(0.0), px(8.0));
        let shell = div()
            .absolute()
            .inset_0()
            .track_focus(&self.session_links.focus)
            .on_key_down(cx.listener(Self::links_key_down))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(div().absolute().inset_0().occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_session_links(window, cx);
                    cx.stop_propagation();
                }),
            ));
        if crate::floating::uses_panels(false, colors, cx) {
            // Focus, keys, and the dismiss scrim stay here; the panel paints
            // the surface where the in-window one would anchor.
            let width = Self::links_width(window.viewport_size());
            let probe =
                crate::floating::surface(colors, Radius::PANEL, width, content).into_any_element();
            let measure = crate::floating::host_element(
                LINKS_PANEL,
                probe,
                width,
                position,
                Anchor::TopRight,
                12.0,
                window,
                cx,
            );
            return Some(shell.child(measure).into_any_element());
        }
        Some(
            shell
                .child(deferred(
                    anchored()
                        .anchor(Anchor::TopRight)
                        .position(position)
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id("session-links-panel")
                                .debug_selector(|| "session-links-panel".into())
                                .w(px(380.0))
                                .max_w(window.viewport_size().width - px(24.0))
                                .occlude()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(FloatingSurface::new(colors, content).radius(Radius::PANEL)),
                        ),
                ))
                .into_any_element(),
        )
    }
}

/// The Links popover as a panel target (see `crate::floating::Target`).
pub(super) const LINKS_PANEL: crate::floating::Target<TerminalPane> = crate::floating::Target {
    key: "links",
    radius: Radius::PANEL,
    content: TerminalPane::links_panel_content,
    dismiss: |pane, window, cx| pane.close_session_links(window, cx),
};

fn pr_number(url: &str) -> Option<String> {
    let parts: Vec<_> = url.split('/').filter(|part| !part.is_empty()).collect();
    if let Some(index) = parts.iter().position(|part| *part == "pull") {
        return parts
            .get(index + 1)
            .map(|part| part.chars().take_while(char::is_ascii_digit).collect())
            .filter(|part: &String| !part.is_empty());
    }
    parts
        .last()
        .filter(|part| part.chars().all(|character| character.is_ascii_digit()))
        .map(|part| (*part).to_owned())
}

fn linear_key(url: &str) -> Option<String> {
    let parts: Vec<_> = url.split('/').collect();
    let index = parts.iter().position(|part| *part == "issue")?;
    parts.get(index + 1).map(|part| (*part).to_owned())
}

fn url_host(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()
        .unwrap_or(url)
        .split(':')
        .next()
        .unwrap_or(url)
        .to_owned()
}

fn comments_help(pr: &PullRequestStatus) -> String {
    let mut parts = Vec::new();
    if let Some(total) = pr.total_threads.filter(|total| *total > 0) {
        parts.push(format!(
            "{} of {total} threads resolved",
            pr.resolved_threads.unwrap_or(0)
        ));
    }
    parts.push(format!(
        "{} comment{}",
        pr.comment_count,
        if pr.comment_count == 1 { "" } else { "s" }
    ));
    parts.push(format!(
        "{} review{}",
        pr.review_count,
        if pr.review_count == 1 { "" } else { "s" }
    ));
    parts.join(" · ")
}

pub(super) fn sorted_checks(pr: &PullRequestStatus) -> Vec<PrCheck> {
    let mut checks = pr.checks.clone().unwrap_or_default();
    checks.sort_by_key(|check| match check.result.as_str() {
        "fail" => 0,
        "pending" => 1,
        "pass" => 2,
        _ => 3,
    });
    checks
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_session, pull_request};
    use super::*;
    use diri_proto::DateMillis;
    use gpui::{Modifiers, TestAppContext, point, size};

    fn fixture() -> SessionRecord {
        let mut session = fixture_session();
        session.title = "Polish the workspace".into();
        session.git_branch = Some("polish/workspace".into());
        session.host = None;
        session.account_profile = None;
        session.listening_ports = None;
        let first = "https://github.com/diri/app/pull/181";
        let second = "https://github.com/diri/app/pull/180";
        let mut first_pr = pull_request(first);
        first_pr.number = 181;
        first_pr.title = Some("Make the workspace feel at home".into());
        let mut second_pr = pull_request(second);
        second_pr.number = 180;
        second_pr.title = Some("Keep conversations close".into());
        second_pr.checks_failed = 0;
        second_pr.checks_pending = 0;
        second_pr.checks_passed = 5;
        second_pr.checks = Some(vec![PrCheck {
            name: "Release build".into(),
            result: "pass".into(),
            detail: None,
            url: Some(format!("{second}/checks")),
        }]);
        session.artifacts = Some(
            [
                first,
                second,
                "https://www.notion.so/Workspace-notes",
                "https://preview.example.com",
            ]
            .into_iter()
            .enumerate()
            .map(|(i, url)| SessionArtifact {
                kind: if i < 2 {
                    ArtifactKind::PullRequest
                } else if i == 3 {
                    ArtifactKind::Preview
                } else {
                    ArtifactKind::Link
                },
                url: url.into(),
                first_seen_at: DateMillis(1.0),
            })
            .collect(),
        );
        session.pull_requests = Some(vec![second_pr, first_pr]); // Deliberately not artifact order.
        session
    }
    fn runtime(session: SessionRecord) -> (Arc<StoreRuntime>, Arc<tokio::runtime::Runtime>) {
        let runtime = Arc::new(StoreRuntime::inert());
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session.clone());
            store.select(session.id);
        }
        (
            runtime,
            Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
        )
    }
    #[test]
    fn multiple_pull_requests_keep_their_own_checks_and_urls() {
        let session = fixture();
        let rows = session_rows(&session);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].status.as_ref().unwrap().0, "1 check failed");
        assert_eq!(rows[1].status.as_ref().unwrap().0, "Checks passed");
        assert_eq!(
            rows[0].action,
            LinkAction::Open("https://github.com/diri/app/pull/181".into())
        );
        assert_eq!(
            rows[1].action,
            LinkAction::Open("https://github.com/diri/app/pull/180".into())
        );
        assert_eq!(rows[2].title, "Notion page");
        let details = detail_rows(&session.pull_requests.as_ref().unwrap()[0]);
        assert_eq!(details[1].title, "Release build");
        assert_eq!(
            details[1].action,
            LinkAction::Open("https://github.com/diri/app/pull/180/checks".into())
        );
    }
    #[test]
    fn merged_state_takes_priority_over_old_checks() {
        let mut session = fixture();
        session.pull_requests.as_mut().unwrap()[1].state = "MERGED".into();
        let rows = session_rows(&session);
        assert_eq!(rows[0].status.as_ref().unwrap().0, "Merged");
    }

    #[test]
    fn status_upgrades_a_generic_artifact_by_url() {
        let mut session = fixture();
        for kind in [ArtifactKind::Link, ArtifactKind::Unknown] {
            session.artifacts.as_mut().unwrap()[0].kind = kind;
            let rows = session_rows(&session);
            assert_eq!(rows.len(), 4);
            assert_eq!(rows[0].status.as_ref().unwrap().0, "1 check failed");
            assert_eq!(
                rows[0].action,
                LinkAction::Open("https://github.com/diri/app/pull/181".into())
            );
        }
    }

    #[test]
    fn status_only_prs_survive_and_duplicate_destinations_count_once() {
        let mut session = fixture();
        session.artifacts.as_mut().unwrap().remove(1);
        let duplicate = session.artifacts.as_ref().unwrap()[0].clone();
        session.artifacts.as_mut().unwrap().push(duplicate);
        let rows = session_rows(&session);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.last().unwrap().action,
            LinkAction::Open("https://github.com/diri/app/pull/180".into())
        );
    }
    #[gpui::test]
    fn mouse_and_keyboard_keep_explicit_detail_navigation_inside_the_menu(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (runtime, tokio) = runtime(fixture());
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        let trigger = cx.debug_bounds("session-links-trigger").unwrap();
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.session_links.open));
        let second = cx.debug_bounds("session-link-details-1").unwrap();
        cx.simulate_click(second.center(), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.session_links.pull_request.clone())
                .as_deref(),
            Some("https://github.com/diri/app/pull/180")
        );
        let back = cx.debug_bounds("session-links-back").unwrap();
        cx.simulate_click(back.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.session_links.open
            && pane.session_links.pull_request.is_none()));
        cx.simulate_keystrokes("up right");
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.session_links.pull_request.clone())
                .as_deref(),
            Some("https://github.com/diri/app/pull/181")
        );
        cx.simulate_keystrokes("left escape");
        assert!(!pane.read_with(cx, |pane, _| pane.session_links.open));
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(10.0), px(300.0)), Modifiers::default());
        assert!(!pane.read_with(cx, |pane, _| pane.session_links.open));
    }
    #[gpui::test]
    fn menu_rows_open_github_directly_without_a_toolbar_pr(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (runtime, tokio) = runtime(fixture());
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("session-primary-pr").is_none());
        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        let position = cx.debug_bounds("session-link-0").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/diri/app/pull/181")
        );
        assert!(!pane.read_with(cx, |p, _| p.session_links.open));
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("down enter");
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/diri/app/pull/180")
        );
        assert!(!pane.read_with(cx, |p, _| p.session_links.open));
    }

    #[test]
    fn search_matches_titles_and_urls_and_hides_the_rest() {
        let session = fixture();
        let rows = filter_rows(session_rows(&session), "notion");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Notion page");
        // The PR row's title says nothing about the repo; its URL does.
        let rows = filter_rows(session_rows(&session), "pull/180");
        assert_eq!(
            rows[0].action,
            LinkAction::Open("https://github.com/diri/app/pull/180".into())
        );
        assert!(filter_rows(session_rows(&session), "zzzz-nothing").is_empty());
        assert_eq!(filter_rows(session_rows(&session), "  ").len(), 4);
    }
    #[gpui::test]
    fn typing_filters_links_and_escape_clears_before_closing(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (runtime, tokio) = runtime(fixture());
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("p r e v i e w");
        cx.run_until_parked();
        assert!(cx.debug_bounds("session-link-0").is_some());
        assert!(cx.debug_bounds("session-link-1").is_none());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(pane.read_with(cx, |p, _| p.session_links.open
            && p.session_links.query.is_empty()));
        assert!(cx.debug_bounds("session-link-3").is_some());
        cx.simulate_keystrokes("n o t i o n enter");
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://www.notion.so/Workspace-notes")
        );
        assert!(!pane.read_with(cx, |p, _| p.session_links.open));
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        assert!(pane.read_with(cx, |p, _| p.session_links.query.is_empty()));
    }

    #[test]
    fn closed_toolbar_only_flags_active_pull_requests() {
        let mut session = fixture();
        assert_eq!(link_count(&session), 4);
        assert_eq!(
            active_check_attention(&session).unwrap().1,
            "Checks need attention in #181"
        );
        session.pull_requests.as_mut().unwrap()[1].state = "MERGED".into();
        assert!(active_check_attention(&session).is_none());
        session.pull_requests.as_mut().unwrap()[0].checks_pending = 2;
        assert_eq!(
            active_check_attention(&session).unwrap().1,
            "Checks running in #180"
        );
    }

    struct WheelHarness {
        pane: Entity<TerminalPane>,
        wheel_events: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Render for WheelHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let wheel_events = self.wheel_events.clone();
            div()
                .size_full()
                .on_scroll_wheel(move |_, _, _| {
                    wheel_events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                })
                .child(self.pane.clone())
        }
    }
    #[gpui::test]
    fn long_link_lists_scroll_without_reaching_the_surface_below(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let mut session = fixture();
        session.artifacts = Some(
            (0..200)
                .map(|index| SessionArtifact {
                    kind: ArtifactKind::Link,
                    url: format!("https://example.com/document/{index}"),
                    first_seen_at: DateMillis(1.0),
                })
                .collect(),
        );
        session.pull_requests = None;
        let (runtime, tokio) = runtime(session);
        let wheel_events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = wheel_events.clone();
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            let pane = cx.new(|cx| TerminalPane::new(runtime, tokio, window, cx));
            cx.observe(&pane, |_, _, cx| cx.notify()).detach();
            WheelHarness {
                pane,
                wheel_events: events,
            }
        });
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        let pane = harness.read_with(cx, |h, _| h.pane.clone());
        let panel = cx.debug_bounds("session-links-panel").unwrap();
        let position = point(panel.left() + px(80.0), panel.top() + px(80.0));
        for _ in 0..3 {
            cx.simulate_event(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.0), px(-10_000.0))),
                ..Default::default()
            });
            cx.run_until_parked();
        }
        assert!(pane.read_with(
            cx,
            |p, _| p.session_links.scroll.0.borrow().base_handle.offset().y < px(0.0)
        ));
        // Also exercise the non-scrolling header and the bottom boundary.
        cx.simulate_event(ScrollWheelEvent {
            position: point(panel.left() + px(80.0), panel.top() + px(20.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            ..Default::default()
        });
        assert_eq!(wheel_events.load(std::sync::atomic::Ordering::Relaxed), 0);
        cx.simulate_resize(size(px(340.0), px(400.0)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("session-links-panel").unwrap();
        assert!(panel.left() >= px(12.0) && panel.right() <= px(328.0));
        assert!(panel.bottom() <= px(388.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a deterministic session-links screenshot"]
    fn render_session_links_preview() {
        let output = std::path::PathBuf::from(std::env::var_os("DIRI_VISUAL_OUTPUT").unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut session = fixture();
        if std::env::var_os("DIRI_VISUAL_DOCUMENTS").is_some() {
            session
                .artifacts
                .as_mut()
                .unwrap()
                .retain(|a| a.kind == ArtifactKind::Link);
            session.pull_requests = None;
            session.git_branch = None;
            session.title = "Plan the autumn launch".into();
        }
        if std::env::var_os("DIRI_VISUAL_MERGED").is_some() {
            session.pull_requests.as_mut().unwrap()[0].state = "MERGED".into();
        }
        let (runtime, tokio) = runtime(session);
        if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
            runtime
                .store
                .write()
                .unwrap()
                .update_preferences(|prefs| prefs.terminal_theme = "dirijor-light".into())
                .unwrap();
        }
        let width = if std::env::var_os("DIRI_VISUAL_NARROW").is_some() {
            340.0
        } else {
            900.0
        };
        let window = cx
            .open_window(size(px(width), px(560.0)), move |window, cx| {
                cx.new(|cx| {
                    let mut pane = TerminalPane::new(runtime, tokio, window, cx);
                    pane.session_links.open = true;
                    if let Some(query) = std::env::var_os("DIRI_VISUAL_QUERY") {
                        pane.session_links.query.insert(&query.to_string_lossy());
                    }
                    if std::env::var_os("DIRI_VISUAL_DETAIL").is_some() {
                        pane.session_links.pull_request =
                            Some("https://github.com/diri/app/pull/181".into());
                    }
                    pane
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }
}
