//! One contextual home for session links. Status is always attached by URL.
use super::*;
use crate::palette_chrome::{PaletteTooltip, keycap, scroll_fades};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::tooltip_warmth::WarmTooltip;
use diri_proto::{ArtifactKind, PrCheck, PullRequestStatus, SessionArtifact};
use diri_ui::{Icon, IconName};
use gpui::{
    Anchor, Animation, AnimationExt, ClickEvent, FontWeight, Pixels, Point, ScrollStrategy,
    UniformListScrollHandle, anchored, canvas, deferred, ease_out_quint, point, rgba, uniform_list,
};
use std::{cell::Cell, rc::Rc};
const ROW_HEIGHT: f32 = 30.0;
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
/// Where a row sits on the home page: work first, then what is running,
/// then everything the chat mentioned, newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    PullRequests,
    Running,
    Links,
}
impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::PullRequests => "Pull requests",
            Self::Running => "Running",
            Self::Links => "Links",
        }
    }
}
#[derive(Clone, Debug)]
struct LinkRow {
    title: String,
    subtitle: String,
    /// The short place a link lives (`posthog.com`, `node`), beside the title
    /// in one-line rows.
    site: String,
    section: Section,
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
        site: format!("#{}", pr.number),
        section: Section::PullRequests,
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
fn port_row(port: &diri_proto::PortInfo) -> LinkRow {
    LinkRow {
        title: format!("localhost:{}", port.port),
        subtitle: port.process_name.clone(),
        site: port.process_name.clone(),
        section: Section::Running,
        icon: IconName::Monitor,
        status: None,
        action: LinkAction::Open(format!("http://localhost:{}", port.port)),
        details: None,
    }
}
fn service_icon(service: diri_notes::links::Service) -> IconName {
    use diri_notes::links::Service;
    match service {
        Service::Notion => IconName::Notion,
        Service::GoogleDocs => IconName::GoogleDoc,
        Service::GoogleSheets => IconName::GoogleSheet,
        Service::GoogleSlides => IconName::GoogleSlides,
        Service::GoogleDrive => IconName::GoogleDrive,
        Service::Linear => IconName::Linear,
        Service::HubSpot => IconName::HubSpot,
        Service::Figma => IconName::Figma,
        Service::Slack => IconName::Slack,
        Service::GitHub => IconName::GitHub,
        Service::Dashboard => IconName::ChartBar,
    }
}
fn artifact_row(artifact: &SessionArtifact) -> LinkRow {
    let url = artifact.url.as_str();
    let recognized = diri_notes::links::recognize(url);
    let local = is_local(&url_host(url));
    let (title, icon) = match (&artifact.kind, &recognized) {
        (ArtifactKind::PullRequest, _) => (
            pr_number(url).map_or_else(|| "Pull request".into(), |n| format!("Pull request #{n}")),
            IconName::PullRequest,
        ),
        // A dashboard's service title is the same for all of them.
        (_, Some(r)) if r.service != diri_notes::links::Service::Dashboard => {
            (r.title.clone(), service_icon(r.service))
        }
        (_, Some(r)) => (link_title(url), service_icon(r.service)),
        _ if local => (site_label(url), IconName::Monitor),
        _ => (link_title(url), IconName::Globe),
    };
    LinkRow {
        title,
        subtitle: url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned(),
        site: site_label(url),
        section: Section::Links,
        icon,
        status: None,
        action: LinkAction::Open(artifact.url.clone()),
        details: None,
    }
}
fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]")
}
/// `posthog.com` for `us.posthog.com/…`, `localhost:3000` for a dev server.
fn site_label(url: &str) -> String {
    let authority = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = url_host(url);
    if is_local(&host) {
        return authority.replace("127.0.0.1", "localhost");
    }
    let labels: Vec<&str> = host.split('.').collect();
    // `example.co.uk` keeps three labels; `supabase.co` keeps two.
    let keep = if labels.len() >= 3
        && labels[labels.len() - 1].len() == 2
        && labels[labels.len() - 2].len() <= 3
    {
        3
    } else {
        2
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}
/// The name of the site a link is on: `Sentry` for `acme.sentry.io`.
fn brand(url: &str) -> String {
    let site = site_label(url);
    let name = site.split(['.', ':']).next().unwrap_or_default();
    match name {
        "github" => "GitHub".into(),
        "posthog" => "PostHog".into(),
        "localhost" => "localhost".into(),
        _ => capitalized(name),
    }
}
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
/// Path words that say what kind of page a link is, not which one.
const KIND_WORDS: &[&str] = &[
    "app",
    "artifact",
    "artifacts",
    "board",
    "chart",
    "charts",
    "d",
    "dashboard",
    "dashboards",
    "doc",
    "docs",
    "document",
    "documents",
    "edit",
    "experiment",
    "experiments",
    "file",
    "files",
    "issue",
    "issues",
    "item",
    "items",
    "log",
    "logs",
    "p",
    "page",
    "pages",
    "project",
    "projects",
    "report",
    "reports",
    "run",
    "runs",
    "trace",
    "traces",
    "view",
];
/// An id rather than a name: a number, a hash or UUID, or a random token.
fn looks_like_id(segment: &str) -> bool {
    let digits = segment.chars().any(|c| c.is_ascii_digit());
    (segment.len() >= 3 && segment.chars().all(|c| c.is_ascii_digit()))
        || (segment.len() >= 8
            && digits
            && segment.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        || (segment.len() >= 12
            && digits
            && segment.chars().all(|c| c.is_ascii_alphanumeric())
            && segment.chars().any(|c| c.is_ascii_uppercase())
            && segment.chars().any(|c| c.is_ascii_lowercase()))
}
/// `chat-v3` → `Chat v3`; a single word or a file name stays as the URL
/// spells it.
fn path_words(segment: &str) -> String {
    let segment = segment
        .strip_suffix(".html")
        .or_else(|| segment.strip_suffix(".htm"))
        .unwrap_or(segment);
    // A file name reads best as the file is called.
    if !segment.contains(['-', '_', '+']) || segment.contains('.') {
        return segment.to_owned();
    }
    capitalized(
        &segment
            .split(['-', '_', '+'])
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}
/// A title that tells links apart without repeating the address: the last
/// path segment that reads as a name (`perfectly_nineties.ttf`), else what
/// kind of page it is and where (`Sentry issue 5912034`, `Chat v3
/// dashboard`), else the host. Ids and random tokens never become titles.
fn link_title(url: &str) -> String {
    let host = url_host(url);
    let host = host.trim_start_matches("www.");
    let path = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let segments: Vec<&str> = path.split('/').skip(1).filter(|s| !s.is_empty()).collect();
    if matches!(host, "github.com") {
        return match segments.as_slice() {
            [owner, repo, "issues", number, ..] => format!("{owner}/{repo}#{number}"),
            [owner, repo, ..] => format!("{owner}/{repo}"),
            _ => host.to_owned(),
        };
    }
    let number = segments
        .last()
        .filter(|s| s.len() <= 12 && s.chars().all(|c| c.is_ascii_digit()));
    let mut names = segments
        .iter()
        .rev()
        .filter(|s| !looks_like_id(s) && s.len() <= 64);
    let Some(name) = names.next() else {
        // A deploy preview's subdomain is its name.
        let preview = [".vercel.app", ".netlify.app", ".pages.dev", ".fly.dev"]
            .iter()
            .any(|suffix| host.ends_with(suffix));
        return if preview {
            host.split('.').next().unwrap_or(host).to_owned()
        } else {
            host.to_owned()
        };
    };
    if !KIND_WORDS.contains(name) {
        return path_words(name);
    }
    let kind = name
        .strip_suffix('s')
        .filter(|_| name.len() > 3)
        .unwrap_or(name);
    let context = names.next().filter(|s| !KIND_WORDS.contains(s));
    match (context, number) {
        (Some(context), _) => format!("{} {kind}", capitalized(&path_words(context))),
        (None, Some(number)) => format!("{} {kind} {number}", brand(url)),
        (None, None) => format!("{} {kind}", brand(url)),
    }
}
/// An address as people read it: no scheme, no trailing slash.
fn display_address(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}
/// One destination, however the chat spelled it: `localhost:3000/` is the
/// dev server on port 3000.
fn link_key(url: &str) -> &str {
    url.trim_end_matches('/')
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
        .map(|a| Cow::Borrowed(link_key(&a.url)))
        .chain(
            session
                .pull_requests
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|pr| Cow::Borrowed(link_key(&pr.url))),
        )
        .collect();
    for port in session.listening_ports.as_deref().unwrap_or_default() {
        urls.insert(Cow::Owned(format!("http://localhost:{}", port.port)));
    }
    urls.len()
}

/// A PR is one destination, regardless of how many checks or comments it has.
/// Pull requests come first, then live dev servers, then the rest with the
/// most recently mentioned first.
fn session_rows(session: &SessionRecord) -> Vec<LinkRow> {
    let statuses = session.pull_requests.as_deref().unwrap_or_default();
    let mut seen = HashSet::new();
    let (mut prs, mut running, mut links) = (Vec::new(), Vec::new(), Vec::new());
    for port in session.listening_ports.as_deref().unwrap_or_default() {
        let row = port_row(port);
        if let LinkAction::Open(url) = &row.action
            && seen.insert(url.clone())
        {
            running.push(row);
        }
    }
    // The Engine keeps first mentions in chat order, and one scan stamps a
    // batch with one time; later in the list is newer within a batch.
    let mut artifacts: Vec<_> = session
        .artifacts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .collect();
    artifacts.sort_by(|(a_index, a), (b_index, b)| {
        b.first_seen_at
            .0
            .partial_cmp(&a.first_seen_at.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_index.cmp(a_index))
    });
    for (_, artifact) in artifacts {
        if !seen.insert(link_key(&artifact.url).to_owned()) {
            continue;
        }
        match statuses.iter().find(|pr| pr.url == artifact.url) {
            Some(pr) => prs.push(pr_row(pr)),
            None => {
                let mut row = artifact_row(artifact);
                if artifact.kind == ArtifactKind::PullRequest {
                    row.section = Section::PullRequests;
                    prs.push(row);
                } else {
                    links.push(row);
                }
            }
        }
    }
    for pr in statuses {
        if seen.insert(link_key(&pr.url).to_owned()) {
            prs.push(pr_row(pr));
        }
    }
    prs.into_iter().chain(running).chain(links).collect()
}
/// What the list shows: a section label or a row.
#[derive(Clone, Debug, PartialEq)]
enum ListItem {
    Label(&'static str),
    /// A row by its index in the rows.
    Row(usize),
}
/// The list for `rows` on the home page; a search or a PR's page is a flat
/// list.
fn list_items(rows: &[LinkRow], flat: bool) -> Vec<ListItem> {
    let mut sections: Vec<Section> = rows.iter().map(|row| row.section).collect();
    sections.dedup();
    let labelled = !flat && sections.len() > 1;
    let mut items = Vec::with_capacity(rows.len() + 3);
    for (index, row) in rows.iter().enumerate() {
        if labelled && (index == 0 || rows[index - 1].section != row.section) {
            items.push(ListItem::Label(row.section.label()));
        }
        items.push(ListItem::Row(index));
    }
    items
}
/// Rows matching `query`, best match first; ties keep the chat's order.
///
/// Every word of the query must appear somewhere in what the row shows, where
/// it goes, or what kind of link it is ("pr", "linear", "preview", "failed").
/// Substrings rather than fuzzy subsequences: a subsequence finds almost any
/// short query somewhere in a long URL, so a fuzzy search over URLs matched
/// nearly every row and appeared not to filter at all.
fn filter_rows(rows: Vec<LinkRow>, query: &str) -> Vec<LinkRow> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return rows;
    }
    let mut scored: Vec<_> = rows
        .into_iter()
        .filter_map(|row| {
            let score = row_score(&row, &words)?;
            Some((score, row))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, row)| row).collect()
}

/// How well `row` matches every one of `words`, or `None` when one is missing.
/// A word at the start of a title word scores highest, then anywhere in the
/// title, then the subtitle, then the destination and kind.
fn row_score(row: &LinkRow, words: &[String]) -> Option<u32> {
    let title = row.title.to_lowercase();
    let subtitle = row.subtitle.to_lowercase();
    let url = match &row.action {
        LinkAction::Open(url) | LinkAction::PullRequest(url) => url.to_lowercase(),
        LinkAction::Account => String::new(),
    };
    let kind = match row.icon {
        IconName::PullRequest | IconName::Merge => "pull request pr github",
        IconName::Checklist => "linear issue",
        IconName::Monitor => "preview localhost",
        _ => "link",
    };
    let status = row
        .status
        .as_ref()
        .map(|(label, _)| label.to_lowercase())
        .unwrap_or_default();
    words.iter().try_fold(0, |total, word| {
        let score = if title
            .split(|c: char| !c.is_alphanumeric())
            .any(|part| part.starts_with(word.as_str()))
        {
            4
        } else if title.contains(word.as_str()) {
            3
        } else if subtitle.contains(word.as_str()) {
            2
        } else if url.contains(word.as_str())
            || kind.contains(word.as_str())
            || status.contains(word.as_str())
        {
            1
        } else {
            return None;
        };
        Some(total + score)
    })
}
fn detail_rows(pr: &PullRequestStatus) -> Vec<LinkRow> {
    let mut rows = vec![LinkRow {
        site: String::new(),
        section: Section::Links,
        title: "Open on GitHub".into(),
        subtitle: format!(
            "{} · +{} −{} · {} files",
            pr_state(pr),
            pr.additions,
            pr.deletions,
            pr.changed_files
        ),
        icon: IconName::GitHub,
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
            site: String::new(),
            section: Section::Links,
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
            site: String::new(),
            section: Section::Links,
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
            site: String::new(),
            section: Section::Links,
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
    /// A search or a PR's page lists rows without labels.
    fn links_flat(&self) -> bool {
        self.session_links.pull_request.is_some() || !self.session_links.query.is_empty()
    }
    fn links_items(&self, rows: &[LinkRow]) -> Vec<ListItem> {
        list_items(rows, self.links_flat())
    }
    /// The list position of the selected row, for scrolling to it.
    fn links_selected_item(&self, rows: &[LinkRow]) -> usize {
        let selected = self.session_links.selected;
        self.links_items(rows)
            .iter()
            .position(|item| matches!(item, ListItem::Row(row) if *row == selected))
            .unwrap_or(selected)
    }
    /// The account row is a context action, not a link, so a search hides it.
    fn links_has_account(&self, session: &SessionRecord) -> bool {
        self.session_links.pull_request.is_none()
            && self.session_links.query.is_empty()
            && session.account_profile.is_some()
    }
    /// Applies a keystroke to the search field when it is a text edit.
    fn links_edit_query(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // `apply` mutates the query; keeping it out of a match guard keeps that visible.
        #[allow(clippy::collapsible_match)]
        match query_editor::edit_for(&event.keystroke) {
            Some(Edit::Local(local)) => {
                if self.session_links.query.apply(local) {
                    self.session_links.selected = 0;
                }
            }
            Some(Edit::Clipboard(edit)) => self.links_clipboard(edit, cx),
            None => {}
        }
    }
    /// A clipboard edit of the search field; the PR detail page has none.
    fn links_clipboard(&mut self, edit: ClipboardEdit, cx: &mut Context<Self>) {
        if self.session_links.pull_request.is_some() {
            return;
        }
        let query = &mut self.session_links.query;
        let changed = match edit {
            ClipboardEdit::Copy => {
                query_editor::copy_selection(query, cx);
                false
            }
            ClipboardEdit::Cut => query_editor::cut_selection(query, cx),
            ClipboardEdit::Paste => cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| query.insert(text.trim())),
        };
        if changed {
            self.session_links.selected = 0;
        }
        cx.notify();
    }
    pub(super) fn close_session_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_links.close();
        self.focus.focus(window, cx);
        cx.notify();
    }
    fn links_back(&mut self, cx: &mut Context<Self>) {
        self.session_links.pull_request = None;
        self.session_links.selected = self.session_links.parent_selection;
        let item = self.selected_session().map_or(0, |session| {
            self.links_selected_item(&self.links_rows(&session))
        });
        self.session_links
            .scroll
            .scroll_to_item(item, ScrollStrategy::Nearest);
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
        let item = self.selected_session().map_or(0, |session| {
            self.links_selected_item(&self.links_rows(&session))
        });
        self.session_links
            .scroll
            .scroll_to_item(item, ScrollStrategy::Nearest);
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
            .child(Icon::new(IconName::Link, 14.0, colors.secondary))
            .when(count > 0, |el| {
                el.child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(count.to_string()),
                )
            })
            .warm_tooltip(move |_, cx| cx.new(|_| PaletteTooltip(help.clone(), colors)).into())
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
    /// A row in the ⌘K palette's skin: solid selection, the address beside
    /// the title while selected, and the site otherwise.
    fn render_link_row(
        &self,
        index: usize,
        row: &LinkRow,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = index == self.session_links.selected;
        let ink = crate::navigation::row_ink(colors, selected);
        // A PR row's status already says what it is; its title gets the room.
        let address = match &row.action {
            _ if row.status.is_some() => String::new(),
            LinkAction::Open(url) | LinkAction::PullRequest(url) => display_address(url),
            LinkAction::Account => String::new(),
        };
        // A host-only link's title is its site already.
        let trailing = (row.status.is_none() && !row.title.ends_with(row.site.as_str()))
            .then(|| row.site.clone())
            .filter(|text| !text.is_empty() && !selected);
        let row_radius = Radius::inner(Radius::PANEL, crate::navigation::PALETTE_ROW_INSET);
        let element = div()
            .id(("session-link", index))
            .debug_selector(move || format!("session-link-{index}"))
            .h(px(ROW_HEIGHT))
            .px(px(crate::navigation::PALETTE_ROW_INSET))
            .child(
                div()
                    .h_full()
                    .px(px(10.0))
                    .rounded(px(row_radius))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .map(|el| {
                        if selected {
                            el.bg(crate::navigation::palette_selection())
                        } else {
                            el.hover(move |el| el.bg(Fill::hover(colors, true)))
                        }
                    })
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .text_color(ink.primary)
                    .child(
                        div()
                            .w(px(16.0))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(Icon::new(row.icon, 14.0, ink.secondary)),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    // The title keeps its width; the address
                                    // takes what is left.
                                    .when(selected && !address.is_empty(), |el| {
                                        el.flex_shrink_0().max_w(gpui::relative(0.7))
                                    })
                                    .truncate()
                                    .child(row.title.clone()),
                            )
                            .when(selected && !address.is_empty(), |el| {
                                el.child(
                                    div()
                                        .min_w(px(0.0))
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(ink.secondary)
                                        .child(address),
                                )
                            }),
                    )
                    .when_some(row.status.clone(), |el, (label, tone)| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(if selected { ink.primary } else { tone })
                                .child(label),
                        )
                    })
                    .when_some(trailing, |el, text| {
                        el.child(
                            div()
                                .flex_none()
                                .max_w(px(150.0))
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(ink.secondary)
                                .child(text),
                        )
                    })
                    .when(selected, |el| {
                        el.child(keycap(ink).child(Icon::new(
                            IconName::Return,
                            14.0,
                            ink.secondary,
                        )))
                    })
                    .when_some(row.details.clone(), |el, url| {
                        el.child(
                            div()
                                .id(("session-link-details", index))
                                .debug_selector(move || format!("session-link-details-{index}"))
                                .size(px(20.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .hover(move |el| el.bg(Fill::hover(colors, true)))
                                .child(Icon::new(IconName::ChevronRight, 12.0, ink.secondary))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.session_links.selected = index;
                                    let url = url.clone();
                                    this.in_main_window(window, cx, move |this, window, cx| {
                                        this.activate_link(
                                            &LinkAction::PullRequest(url),
                                            false,
                                            window,
                                            cx,
                                        );
                                    });
                                    cx.stop_propagation();
                                })),
                        )
                    }),
            );
        self.link_row_events(element, index, row.action.clone(), cx)
    }
    /// Hover moves the one highlight; a click opens (⌥ copies).
    fn link_row_events(
        &self,
        element: gpui::Stateful<gpui::Div>,
        index: usize,
        action: LinkAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        element
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.session_links.selected != index {
                    this.session_links.selected = index;
                    cx.notify();
                }
            }))
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
    /// The ⌘K palette's search field: an inset pill with an esc keycap.
    fn render_palette_links_header(
        &self,
        pull_request: Option<i64>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let inset = crate::navigation::PALETTE_ROW_INSET;
        let query = &self.session_links.query;
        let text = match pull_request {
            Some(number) => div()
                .text_color(colors.primary)
                .child(format!("Pull request #{number}"))
                .into_any_element(),
            None if query.is_empty() => div()
                .text_color(colors.secondary)
                .child("Search links…")
                .into_any_element(),
            None => div()
                .text_color(colors.primary)
                .child(crate::navigation::query_label(query))
                .into_any_element(),
        };
        div()
            .mt(px(inset))
            .mx(px(inset))
            .mb(px(inset))
            .h(px(34.0))
            .pl(px(4.0))
            .pr(px(10.0))
            .rounded(px(Radius::inner(Radius::PANEL, inset)))
            .bg(Fill::subtle(colors))
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .id("session-links-back")
                    .debug_selector(|| "session-links-back".into())
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .when(pull_request.is_some(), |el| {
                        el.cursor_pointer()
                            .hover(move |el| el.bg(Fill::hover(colors, true)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.links_back(cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child(Icon::new(
                        if pull_request.is_some() {
                            IconName::ChevronLeft
                        } else {
                            IconName::Search
                        },
                        13.0,
                        colors.secondary,
                    )),
            )
            .child(
                div()
                    .id("session-links-search")
                    .debug_selector(|| "session-links-search".into())
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_size(px(14.0))
                    .child(text),
            )
            .child(
                keycap(colors)
                    .id("session-links-close")
                    .cursor_pointer()
                    .hover(move |el| el.bg(Fill::hover(colors, true)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_session_links(window, cx);
                        cx.stop_propagation();
                    }))
                    .child("esc"),
            )
            .into_any_element()
    }
    fn render_list_label(text: &'static str, colors: SemanticColors) -> AnyElement {
        div()
            .h(px(ROW_HEIGHT))
            .px(px(14.0))
            .pb(px(5.0))
            .flex()
            .items_end()
            .text_size(px(Typo::SECTION_HEADER.size))
            .font_weight(Typo::SECTION_HEADER.weight)
            .text_color(colors.tertiary)
            .child(text)
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
        let header = self.render_palette_links_header(pr.map(|pr| pr.number), colors, cx);
        let items = self.links_items(&rows);
        let list_height = (items.len() as f32 * ROW_HEIGHT)
            .min(420.0)
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
                    uniform_list("session-links-list", items.len(), move |range, _, cx| {
                        entity.update(cx, |this, cx| {
                            range
                                .map(|index| match &items[index] {
                                    ListItem::Row(row) => {
                                        this.render_link_row(*row, &rows[*row], colors, cx)
                                    }
                                    ListItem::Label(text) => Self::render_list_label(text, colors),
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
                                .warm_tooltip(move |_, cx| {
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
            // ⌘V and ⌘C are terminal bindings, and bindings run before key
            // listeners: without these the search field pasted into the agent.
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                this.links_clipboard(ClipboardEdit::Paste, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.links_clipboard(ClipboardEdit::Copy, cx);
                cx.stop_propagation();
            }))
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
        // Newest mention first: one scan stamps every link with one time.
        assert_eq!(rows[0].status.as_ref().unwrap().0, "Checks passed");
        assert_eq!(rows[1].status.as_ref().unwrap().0, "1 check failed");
        assert_eq!(
            rows[0].action,
            LinkAction::Open("https://github.com/diri/app/pull/180".into())
        );
        assert_eq!(
            rows[1].action,
            LinkAction::Open("https://github.com/diri/app/pull/181".into())
        );
        assert_eq!(rows[3].title, "Workspace notes");
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
        assert_eq!(rows[1].status.as_ref().unwrap().0, "Merged");
    }

    #[test]
    fn status_upgrades_a_generic_artifact_by_url() {
        let mut session = fixture();
        for kind in [ArtifactKind::Link, ArtifactKind::Unknown] {
            session.artifacts.as_mut().unwrap()[0].kind = kind;
            let rows = session_rows(&session);
            assert_eq!(rows.len(), 4);
            assert_eq!(rows[1].status.as_ref().unwrap().0, "1 check failed");
            assert_eq!(
                rows[1].action,
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
        // A PR known only from its status still sits with the other PRs.
        assert_eq!(
            rows[1].action,
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
            Some("https://github.com/diri/app/pull/181")
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
            Some("https://github.com/diri/app/pull/180")
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
            Some("https://github.com/diri/app/pull/180")
        );
        assert!(!pane.read_with(cx, |p, _| p.session_links.open));
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("down enter");
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/diri/app/pull/181")
        );
        assert!(!pane.read_with(cx, |p, _| p.session_links.open));
    }

    #[test]
    fn search_matches_titles_and_urls_and_hides_the_rest() {
        let session = fixture();
        let rows = filter_rows(session_rows(&session), "notion");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Workspace notes");
        // The PR row's title says nothing about the repo; its URL does.
        let rows = filter_rows(session_rows(&session), "pull/180");
        assert_eq!(
            rows[0].action,
            LinkAction::Open("https://github.com/diri/app/pull/180".into())
        );
        assert!(filter_rows(session_rows(&session), "zzzz-nothing").is_empty());
        assert_eq!(filter_rows(session_rows(&session), "  ").len(), 4);
    }

    #[test]
    fn links_on_one_host_get_distinct_titles() {
        let title = link_title;
        assert_eq!(
            title("https://static.anara.com/fonts/perfectly_nineties.ttf"),
            "perfectly_nineties.ttf"
        );
        assert_eq!(title("https://static.anara.com/"), "static.anara.com");
        assert_eq!(title("https://docs.dev/items/12345?x=1"), "Docs item 12345");
        assert_eq!(title("https://github.com/o/r/issues/12#top"), "o/r#12");
        assert_eq!(title("https://github.com/o/r"), "o/r");
    }

    #[test]
    fn search_words_must_all_appear_rather_than_scatter_through_urls() {
        let session = fixture();
        let titles = |query: &str| -> Vec<String> {
            filter_rows(session_rows(&session), query)
                .into_iter()
                .map(|row| row.title)
                .collect()
        };
        // A fuzzy subsequence finds "w", "o", "r", "k" in every long URL.
        assert_eq!(
            titles("work"),
            ["Make the workspace feel at home", "Workspace notes"]
        );
        assert_eq!(
            titles("pull request"),
            [
                "Keep conversations close",
                "Make the workspace feel at home"
            ]
        );
        assert_eq!(titles("failed"), ["Make the workspace feel at home"]);
        assert_eq!(titles("preview"), ["preview.example.com"]);
        assert_eq!(titles("181"), ["Make the workspace feel at home"]);
        assert_eq!(titles("home 181"), ["Make the workspace feel at home"]);
        assert!(titles("home 180").is_empty());
        // Title hits rank above URL hits.
        assert_eq!(titles("keep")[0], "Keep conversations close");
    }

    #[gpui::test]
    fn command_v_pastes_into_the_search_not_the_terminal(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            crate::commands::bind_keys(cx, &Default::default());
        });
        let (runtime, tokio) = runtime(fixture());
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        cx.simulate_resize(size(px(900.0), px(700.0)));
        cx.run_until_parked();
        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        cx.write_to_clipboard(ClipboardItem::new_string(" notion \n".into()));
        cx.simulate_keystrokes("cmd-v");
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(cx, |p, _| p.session_links.query.text().to_owned()),
            "notion"
        );
        assert!(cx.debug_bounds("session-link-0").is_some());
        assert!(cx.debug_bounds("session-link-1").is_none());
    }
    /// The app's key context, where ⌘1–⌘9 are bound.
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
    fn titles_name_the_page_never_an_id() {
        let title = link_title;
        assert_eq!(
            title("https://claude.ai/artifact/QquGRTGC4XqV32G1EGUR3c"),
            "Claude artifact"
        );
        assert_eq!(
            title("https://us.posthog.com/project/344763/dashboard/2180383"),
            "PostHog dashboard 2180383"
        );
        assert_eq!(
            title(
                "https://www.braintrust.dev/app/Acme/p/chat-v3/dashboards/5a245a5b-31a2-4bec-aaff-e43de7e82fda"
            ),
            "Chat v3 dashboard"
        );
        assert_eq!(
            title("https://acme.sentry.io/issues/5912034/"),
            "Sentry issue 5912034"
        );
        assert_eq!(
            title("https://acme-git-onboarding.vercel.app"),
            "acme-git-onboarding"
        );
        assert_eq!(title("https://api.acme.test"), "api.acme.test");
        assert_eq!(
            title("https://docs.rs/gpui/latest/gpui/struct.Window.html"),
            "struct.Window"
        );
        assert_eq!(site_label("https://us.posthog.com/x"), "posthog.com");
        assert_eq!(site_label("https://a.example.co.uk/x"), "example.co.uk");
        assert_eq!(site_label("http://127.0.0.1:5173/"), "localhost:5173");
    }

    #[test]
    fn a_dev_server_is_one_row_however_the_chat_spelled_it() {
        let mut session = fixture();
        session.pull_requests = None;
        session.artifacts = Some(vec![SessionArtifact {
            kind: ArtifactKind::Preview,
            url: "http://localhost:3000/".into(),
            first_seen_at: DateMillis(1.0),
        }]);
        session.listening_ports = Some(vec![diri_proto::PortInfo {
            port: 3000,
            process_name: "node".into(),
        }]);
        let rows = session_rows(&session);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].title.as_str(), rows[0].site.as_str()),
            ("localhost:3000", "node")
        );
        assert_eq!(link_count(&session), 1);
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

    /// A long agent session the way they really look: two PRs, three dev
    /// servers, a dozen dashboards on the same two hosts, ids for titles.
    #[cfg(target_os = "macos")]
    fn busy_session(session: &mut SessionRecord) {
        let urls = [
            "https://linear.app/acme/issue/PRD-8201/bring-extraction-to-the-reader",
            "https://abcdxyzproj.supabase.co/auth/v1/oauth/authorize?client_id=reader",
            "http://localhost:3000/",
            "https://api.acme.test",
            "https://github.com/diri/app/pull/181",
            "https://docs.rs/gpui/latest/gpui/struct.Window.html",
            "https://us.posthog.com/project/344763/dashboard/2180383",
            "https://www.braintrust.dev/app/Acme/p/dev/dashboards/40d80dc6-506a-4fb7-85d3-808d7e074267",
            "https://developer.apple.com/documentation/appkit/nsvisualeffectview",
            "https://www.braintrust.dev/app/Acme/p/chat-v3/dashboards/5a245a5b-31a2-4bec-aaff-e43de7e82fda",
            "https://us.posthog.com/project/344763/dashboard/1970912",
            "https://acme.sentry.io/issues/5912034/",
            "https://www.figma.com/design/x7Hq2Lw/Reader-docked-chat",
            "https://github.com/diri/app/pull/180",
            "https://us.posthog.com/project/344763/dashboard/1957976",
            "https://www.notion.so/acme/Reader-launch-plan-1f2e3d4c5b6a79881f2e3d4c5b6a7988",
            "https://us.posthog.com/project/344763/dashboard/2134737",
            "https://acme-git-guide-onboarding-cursor.vercel.app",
            "https://claude.ai/artifact/QquGRTGC4XqV32G1EGUR3c",
        ];
        session.git_branch = Some("feature/reader-docked-chat".into());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        session.artifacts = Some(
            urls.iter()
                .enumerate()
                .map(|(i, url)| SessionArtifact {
                    kind: if url.contains("/pull/") {
                        ArtifactKind::PullRequest
                    } else if url.contains("linear.app") {
                        ArtifactKind::LinearIssue
                    } else if url.contains("localhost") {
                        ArtifactKind::Preview
                    } else {
                        ArtifactKind::Link
                    },
                    url: (*url).into(),
                    first_seen_at: DateMillis(now - (urls.len() - i) as f64 * 7.0 * 60_000.0),
                })
                .collect(),
        );
        session.listening_ports = Some(
            [(3000, "node"), (3001, "Python"), (42069, "node")]
                .into_iter()
                .map(|(port, name)| diri_proto::PortInfo {
                    port,
                    process_name: name.into(),
                })
                .collect(),
        );
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
        // A JSON list of artifacts, as a daemon scan produced them, replaces
        // the fixture's links, so two builds can render the same scan input.
        if let Some(path) = std::env::var_os("DIRI_VISUAL_ARTIFACTS") {
            session.artifacts =
                Some(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap());
            session.pull_requests = None;
            session.git_branch = Some("fix/email-serif".into());
        }
        if std::env::var_os("DIRI_VISUAL_BUSY").is_some() {
            busy_session(&mut session);
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
        let height = if std::env::var_os("DIRI_VISUAL_BUSY").is_some() {
            720.0
        } else {
            560.0
        };
        let window = cx
            .open_window(size(px(width), px(height)), move |window, cx| {
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
        // `DIRI_VISUAL_POINTER=x,y` rests the pointer there until a tooltip opens.
        if let Some((x, y)) = std::env::var("DIRI_VISUAL_POINTER").ok().and_then(|value| {
            let (x, y) = value.split_once(',')?;
            Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
        }) {
            cx.update_window(window.into(), |_, window, cx| {
                window.simulate_mouse_move(point(px(x), px(y)), cx);
            })
            .unwrap();
            for _ in 0..8 {
                cx.advance_clock(Duration::from_millis(120));
                cx.run_until_parked();
                cx.update_window(window.into(), |_, window, _| window.refresh())
                    .unwrap();
                cx.run_until_parked();
            }
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }
}
