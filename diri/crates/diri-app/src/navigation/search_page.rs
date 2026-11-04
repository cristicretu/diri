//! "Search everything" (⇧⌘A): one box over every session.
//!
//! A query finds, in this order: sessions by name, folder or branch; the
//! newest line holding every word on each terminal's screen or scrollback;
//! the newest matching message of each live Claude Code or Codex
//! conversation; and commands. With no query the page lists the sessions
//! that need you first.
//!
//! Screens and conversations are searched by the Engine on demand
//! (`search.global`): nothing is indexed and nothing runs while the page is
//! closed. A reply comes back within its time budget with cursors for the
//! history it did not reach; the page streams those pages while the query
//! stands, and drops them when it changes. The highlighted hit is previewed
//! below the list. Choosing a screen hit selects its pane and opens Find
//! there on the newest match.
use super::*;
use diri_proto::search::{ConversationHit, GlobalSearchParams, GlobalSearchResult, ScreenHit};
use diri_ui::{AgentLogo, Typo};

/// Two lines: a title, then the matching line. Tighter than a note row so
/// the list and the preview share the palette's one list height.
pub(super) const SEARCH_ROW_HEIGHT: f32 = 48.0;
/// The preview below the list: five context lines and a caption.
pub(super) const PREVIEW_HEIGHT: f32 = 100.0;
const PREVIEW_LINE_HEIGHT: f32 = 14.0;
/// Typing settles before the Engine is asked.
const DEBOUNCE: Duration = Duration::from_millis(60);
/// Each page's scan budget: a page lands within ~100 ms.
const PAGE_BUDGET_MS: u32 = 90;
const MAX_SCREEN_SESSIONS: u32 = 24;
const LINES_PER_SESSION: u32 = 6;
const SESSION_ROWS: usize = 5;
const ACTION_ROWS: usize = 3;
/// Sessions the empty page lists, those needing you first.
const EMPTY_ROWS: usize = 12;
/// Asks again for conversations the Engine was still reading.
const TRANSCRIPT_RETRIES: u8 = 3;
const TRANSCRIPT_RETRY_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Debug)]
pub(super) enum SearchRow {
    /// Matched by name, or listed on the empty page.
    Session(Box<Ranked<SessionRecord>>),
    /// Index into [`SearchPage::screens`].
    Screen(usize),
    /// Index into [`SearchPage::conversations`].
    Conversation(usize),
    Action(Box<Ranked<PaletteAction>>),
}

impl SearchRow {
    /// What the row is about, so a rebuild keeps the highlight on it.
    fn key(&self, page: &SearchPage) -> String {
        match self {
            Self::Session(ranked) => format!("session:{}", ranked.item.id.0),
            Self::Screen(index) => format!("screen:{}", page.screens[*index].session_id.0),
            Self::Conversation(index) => {
                format!("chat:{}", page.conversations[*index].session_id.0)
            }
            Self::Action(ranked) => format!("action:{}", ranked.item.id),
        }
    }
}

#[derive(Default)]
pub(super) struct SearchPage {
    pub(super) rows: Vec<SearchRow>,
    screens: Vec<ScreenHit>,
    conversations: Vec<ConversationHit>,
    /// Bumped by every query; replies to an older one are dropped.
    generation: u64,
    /// More pages are on their way for the current query.
    pub(super) searching: bool,
    /// Conversations the Engine had not read yet when it last answered.
    transcripts_pending: bool,
    error: Option<String>,
    task: Option<Task<()>>,
}

/// What follows a reply.
enum Next {
    /// The next page of screens the last one did not reach.
    Page(GlobalSearchParams),
    /// Conversations again, once the Engine has read the ones it was missing.
    Conversations(GlobalSearchParams),
    Done,
}

impl NavigationOverlay {
    pub(crate) fn toggle_global_search(
        &mut self,
        _: &crate::commands::GlobalSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay == Some(Overlay::Search) {
            self.close_overlay(window, cx);
        } else {
            self.open_overlay(Overlay::Search, window, cx);
        }
    }

    /// The page opened: list who needs you, and have the Engine start
    /// reading live conversations so the first query finds them.
    pub(super) fn open_search(&mut self, cx: &mut Context<Self>) {
        self.search = SearchPage::default();
        self.rebuild_search_rows();
        let client = Arc::clone(self._runtime.client());
        let warm = GlobalSearchParams {
            query: String::new(),
            max_sessions: 1,
            max_lines_per_session: 1,
            budget_ms: 1,
            screens: false,
            transcripts: true,
            resume: Vec::new(),
        };
        self.tokio.spawn(async move {
            let _ = client.search_global(&warm).await;
        });
        cx.notify();
    }

    pub(super) fn close_search(&mut self) {
        self.search = SearchPage::default();
    }

    /// A new query: local rows now, Engine results as they stream in.
    pub(super) fn search_query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.query.text().trim().to_owned();
        let page = &mut self.search;
        page.generation += 1;
        page.screens.clear();
        page.conversations.clear();
        page.error = None;
        page.transcripts_pending = false;
        page.task = None;
        page.searching = !query.is_empty();
        let generation = page.generation;
        self.rebuild_search_rows();
        if query.is_empty() {
            return;
        }
        let client = Arc::clone(self._runtime.client());
        let runtime = Arc::clone(&self.tokio);
        let first = GlobalSearchParams {
            query,
            max_sessions: MAX_SCREEN_SESSIONS,
            max_lines_per_session: LINES_PER_SESSION,
            budget_ms: PAGE_BUDGET_MS,
            screens: true,
            transcripts: true,
            resume: Vec::new(),
        };
        self.search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let mut params = first;
            let mut retries = 0;
            loop {
                let request = params.clone();
                let client = Arc::clone(&client);
                let reply = runtime
                    .spawn(async move { client.search_global(&request).await })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|reply| reply.map_err(|error| error.to_string()));
                let next = this
                    .update(cx, |this, cx| {
                        this.apply_search_reply(generation, params, reply, cx)
                    })
                    .unwrap_or(Next::Done);
                params = match next {
                    Next::Page(next) => next,
                    Next::Conversations(next) if retries < TRANSCRIPT_RETRIES => {
                        retries += 1;
                        cx.background_executor().timer(TRANSCRIPT_RETRY_DELAY).await;
                        next
                    }
                    Next::Conversations(_) | Next::Done => return,
                };
            }
        }));
    }

    fn apply_search_reply(
        &mut self,
        generation: u64,
        mut params: GlobalSearchParams,
        reply: Result<GlobalSearchResult, String>,
        cx: &mut Context<Self>,
    ) -> Next {
        if self.overlay != Some(Overlay::Search) || self.search.generation != generation {
            return Next::Done;
        }
        let result = match reply {
            Ok(result) => result,
            Err(_) => {
                // The reason (a disconnected Engine, usually) is not the
                // user's to act on; the page says search is unavailable.
                self.search.error = Some(t("nav.search.unavailable").to_owned());
                self.search.searching = false;
                cx.notify();
                return Next::Done;
            }
        };
        let highlighted = self.highlighted_search_key();
        let page = &mut self.search;
        if params.screens {
            merge_screens(&mut page.screens, result.screens);
        }
        if params.transcripts {
            page.conversations = result.conversations;
            page.transcripts_pending = result.incomplete.transcripts_pending > 0;
        }
        // Sessions that already show their lines stop; the rest go on, until
        // the page holds as many sessions as it lists.
        let full = page.screens.len() >= MAX_SCREEN_SESSIONS as usize;
        let resume: Vec<_> = result
            .incomplete
            .resume
            .into_iter()
            .filter(|cursor| {
                page.screens
                    .iter()
                    .find(|hit| hit.session_id == cursor.session_id)
                    .map_or(!full, |hit| hit.lines.len() < LINES_PER_SESSION as usize)
            })
            .collect();
        let next = if params.screens && !resume.is_empty() {
            params.resume = resume;
            params.transcripts = false;
            Next::Page(params)
        } else if page.transcripts_pending {
            params.screens = false;
            params.transcripts = true;
            params.resume = Vec::new();
            Next::Conversations(params)
        } else {
            Next::Done
        };
        self.search.searching = matches!(next, Next::Page(_));
        self.rebuild_search_rows();
        self.restore_search_highlight(highlighted.as_deref());
        cx.notify();
        next
    }

    fn highlighted_search_key(&self) -> Option<String> {
        self.search
            .rows
            .get(self.highlight)
            .map(|row| row.key(&self.search))
    }

    fn restore_search_highlight(&mut self, key: Option<&str>) {
        let found = key.and_then(|key| {
            self.search
                .rows
                .iter()
                .position(|row| row.key(&self.search) == key)
        });
        self.highlight = found
            .unwrap_or(self.highlight)
            .min(self.search.rows.len().saturating_sub(1));
    }

    /// Rows for the current query from the store and the latest replies.
    pub(super) fn rebuild_search_rows(&mut self) {
        let query = self.query.text().trim().to_owned();
        let (sessions, actions) = if query.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let (actions, _, _) = self.palette_catalog();
            (self.open_sessions(), actions)
        };
        let mut rows = Vec::new();
        if query.is_empty() {
            let mut open = self.open_sessions();
            open.sort_by_key(attention_order);
            rows.extend(open.into_iter().take(EMPTY_ROWS).map(|session| {
                SearchRow::Session(Box::new(Ranked {
                    item: session,
                    title_matches: Vec::new(),
                    score: 0,
                }))
            }));
            self.search.rows = rows;
            return;
        }
        let fuzzy = FuzzyQuery::new(&query);
        rows.extend(
            palette::rank_sessions(sessions, &fuzzy, &mut self.matcher)
                .into_iter()
                .take(SESSION_ROWS)
                .map(|ranked| SearchRow::Session(Box::new(ranked))),
        );
        let known: HashSet<SessionId> = {
            let store = self.store.read().expect("session store lock poisoned");
            store.sessions().keys().cloned().collect()
        };
        rows.extend(
            self.search
                .screens
                .iter()
                .enumerate()
                .filter(|(_, hit)| known.contains(&hit.session_id))
                .map(|(index, _)| SearchRow::Screen(index)),
        );
        rows.extend(
            self.search
                .conversations
                .iter()
                .enumerate()
                .filter(|(_, hit)| known.contains(&hit.session_id))
                .map(|(index, _)| SearchRow::Conversation(index)),
        );
        rows.extend(
            palette::rank_actions(actions, &fuzzy, &mut self.matcher)
                .into_iter()
                .filter(|ranked| ranked.item.enabled)
                .take(ACTION_ROWS)
                .map(|ranked| SearchRow::Action(Box::new(ranked))),
        );
        self.search.rows = rows;
    }

    /// Every session the page can jump to, most recently active first.
    fn open_sessions(&self) -> Vec<SessionRecord> {
        let store = self.store.read().expect("session store lock poisoned");
        let mut sessions: Vec<SessionRecord> = store
            .sessions()
            .values()
            .filter(|session| !session.is_archived())
            .map(|session| session.as_ref().clone())
            .collect();
        sessions.sort_by(|left, right| right.updated_at.0.total_cmp(&left.updated_at.0));
        sessions
    }

    pub(super) fn open_highlighted_search_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.search.rows.get(self.highlight).cloned() else {
            return;
        };
        match row {
            SearchRow::Session(ranked) => {
                self.run_command_selection(CommandSelection::Session(ranked.item.id), window, cx)
            }
            SearchRow::Screen(index) => {
                let hit = &self.search.screens[index];
                let id = hit.session_id.clone();
                let find = find_query(self.query.text(), hit);
                crate::find_reveal::request(id.clone(), find);
                self.run_command_selection(CommandSelection::Session(id), window, cx);
            }
            SearchRow::Conversation(index) => {
                let id = self.search.conversations[index].session_id.clone();
                self.run_command_selection(CommandSelection::Session(id), window, cx);
            }
            SearchRow::Action(ranked) => self.run_command_selection(
                CommandSelection::Action(ranked.item.command.clone()),
                window,
                cx,
            ),
        }
    }

    pub(super) fn search_empty_label(&self) -> &'static str {
        if self.query.text().trim().is_empty() {
            t("nav.search.no_sessions")
        } else if self.search.searching {
            t("nav.search.searching")
        } else {
            t("nav.no_matches")
        }
    }

    pub(super) fn search_is_busy(&self) -> bool {
        self.search.searching
    }

    pub(super) fn render_search_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let selected = index == self.highlight;
        let row = self.search.rows[index].clone();
        let store = self.store.read().expect("session store lock poisoned");
        let session_of = |id: &SessionId| store.sessions().get(id).map(|record| record.as_ref().clone());
        let title_tone = if selected {
            colors.primary
        } else {
            colors.text(diri_ui::TextTone::Unselected)
        };
        let RowParts {
            leading,
            title,
            title_ranges,
            meta,
            line,
            line_ranges,
            mono,
        } = match &row {
            SearchRow::Session(ranked) => {
                let session = &ranked.item;
                let state = status_state(session, false);
                let empty = self.query.text().trim().is_empty();
                RowParts {
                    leading: StatusGlyph::new(
                        ui_agent_kind(session.effective_kind()),
                        state,
                        22.0,
                        colors,
                    )
                    .rendered_mark(),
                    title: session.title.clone(),
                    title_ranges: ranked.title_matches.clone(),
                    meta: status_label(session).to_owned(),
                    line: empty
                        .then(|| crate::session_presentation::blocker_line(session))
                        .flatten()
                        .unwrap_or_else(|| folder_line(session)),
                    line_ranges: Vec::new(),
                    mono: false,
                }
            }
            SearchRow::Screen(hit) => {
                let hit = &self.search.screens[*hit];
                let session = session_of(&hit.session_id);
                let newest = hit.lines.first();
                RowParts {
                    leading: session_logo(session.as_ref(), colors),
                    title: session.as_ref().map_or_else(String::new, |s| s.title.clone()),
                    title_ranges: Vec::new(),
                    meta: t(if hit.screen_only {
                        "nav.search.remote_screen"
                    } else {
                        "nav.search.terminal"
                    })
                    .to_owned(),
                    line: newest.map_or_else(String::new, |line| line.text.trim().to_owned()),
                    line_ranges: newest.map_or_else(Vec::new, |line| {
                        byte_ranges_trimmed(&line.text, &line.ranges)
                    }),
                    mono: true,
                }
            }
            SearchRow::Conversation(hit) => {
                let hit = &self.search.conversations[*hit];
                let session = session_of(&hit.session_id);
                RowParts {
                    leading: session_logo(session.as_ref(), colors),
                    title: session.as_ref().map_or_else(String::new, |s| s.title.clone()),
                    title_ranges: Vec::new(),
                    meta: t(if hit.role == "user" {
                        "nav.search.you_wrote"
                    } else {
                        "nav.search.agent_replied"
                    })
                    .to_owned(),
                    line: hit.excerpt.text.clone(),
                    line_ranges: byte_ranges(&hit.excerpt.text, &hit.excerpt.ranges),
                    mono: false,
                }
            }
            SearchRow::Action(ranked) => RowParts {
                leading: div()
                    .size(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol(ranked.item.system_image, 15.0, colors.secondary))
                    .into_any_element(),
                title: ranked.item.title.clone(),
                title_ranges: ranked.title_matches.clone(),
                meta: ranked.item.shortcut.clone().unwrap_or_default(),
                line: ranked
                    .item
                    .detail
                    .clone()
                    .unwrap_or_else(|| t("nav.search.command").to_owned()),
                line_ranges: Vec::new(),
                mono: false,
            },
        };
        drop(store);
        div()
            .h(px(SEARCH_ROW_HEIGHT))
            .px(px(6.0))
            .py(px(2.0))
            .child(
                div()
                    .id(("search-row", index))
                    .debug_selector(move || format!("search-row-{index}"))
                    .group("search-row")
                    .h_full()
                    .px(px(9.0))
                    .rounded(px(Radius::inner(
                        super::PALETTE_RADIUS,
                        super::PALETTE_ROW_INSET,
                    )))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .glass_menu_row(colors, selected)
                    .cursor_pointer()
                    .active(move |style| style.opacity(0.74))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.highlight != index {
                            this.highlight = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.in_main(window, cx, move |this, window, cx| {
                            this.highlight = index;
                            this.open_highlighted_search_row(window, cx);
                        })
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(28.0))
                            .flex()
                            .justify_center()
                            .child(leading),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .min_w(px(0.0))
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .flex_shrink(1.0)
                                            .text_size(px(Typo::ROW.size))
                                            .text_color(title_tone)
                                            .truncate()
                                            .child(highlighted_label(title, &title_ranges)),
                                    )
                                    .when(!meta.is_empty(), |line| {
                                        line.child(
                                            div()
                                                .flex_none()
                                                .text_size(px(Typo::META.size))
                                                .text_color(colors.tertiary)
                                                .child(meta),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(if mono { 11.5 } else { Typo::META.size + 1.0 }))
                                    .when(mono, |line| {
                                        line.font_family(crate::fonts::mono_family())
                                    })
                                    .text_color(colors.secondary)
                                    .truncate()
                                    .child(highlighted_label(line, &line_ranges)),
                            ),
                    )
                    .child(
                        keycap(colors)
                            .debug_selector(move || format!("search-return-{index}"))
                            .flex_none()
                            .when(!selected, |cue| cue.invisible())
                            .group_hover("search-row", |style| style.visible())
                            .child(Icon::new(IconName::Return, 14.0, colors.secondary)),
                    ),
            )
            .into_any_element()
    }

    /// Below the list: the highlighted hit in context, else what the search
    /// covers and what it is still doing.
    pub(super) fn render_search_preview(&self, colors: SemanticColors) -> AnyElement {
        let caption = |text: String| {
            div()
                .flex_none()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .truncate()
                .child(text)
        };
        let mut body: Vec<AnyElement> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        match self.search.rows.get(self.highlight) {
            Some(SearchRow::Screen(index)) => {
                let hit = &self.search.screens[*index];
                for line in &hit.context {
                    let matched = !line.ranges.is_empty();
                    body.push(
                        div()
                            .h(px(PREVIEW_LINE_HEIGHT))
                            .px(px(6.0))
                            .rounded(px(4.0))
                            .when(matched, |row| row.bg(Palette::CLAY.alpha(0.14)))
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(11.0))
                            .line_height(px(PREVIEW_LINE_HEIGHT))
                            .text_color(if matched {
                                colors.primary
                            } else {
                                colors.secondary
                            })
                            .truncate()
                            .child(highlighted_label(
                                if line.text.is_empty() {
                                    " ".to_owned()
                                } else {
                                    line.text.clone()
                                },
                                &byte_ranges(&line.text, &line.ranges),
                            ))
                            .into_any_element(),
                    );
                }
                let others = hit.lines.len().saturating_sub(1);
                notes.push(if others == 0 && !hit.more {
                    t("nav.search.only_match").to_owned()
                } else if hit.more {
                    tf("nav.search.more_matches_plus", &[("count", &others)])
                } else {
                    tf("nav.search.more_matches", &[("count", &others)])
                });
                if hit.screen_only {
                    notes.push(t("nav.search.remote_history_not_searched").to_owned());
                }
                notes.push(t("nav.search.opens_find").to_owned());
            }
            Some(SearchRow::Conversation(index)) => {
                let hit = &self.search.conversations[*index];
                body.push(
                    div()
                        .px(px(6.0))
                        .text_size(px(11.5))
                        .line_height(px(PREVIEW_LINE_HEIGHT))
                        .text_color(colors.primary)
                        .max_h(px(PREVIEW_LINE_HEIGHT * 5.0))
                        .overflow_hidden()
                        .child(highlighted_label(
                            hit.excerpt.text.clone(),
                            &byte_ranges(&hit.excerpt.text, &hit.excerpt.ranges),
                        ))
                        .into_any_element(),
                );
                notes.push(if hit.messages_ago == 0 {
                    t("nav.search.latest_message").to_owned()
                } else {
                    tf(
                        if hit.messages_ago == 1 {
                            "nav.search.messages_ago_one"
                        } else {
                            "nav.search.messages_ago_other"
                        },
                        &[("count", &hit.messages_ago)],
                    )
                });
                if hit.more > 0 {
                    notes.push(tf(
                        if hit.more == 1 {
                            "nav.search.more_messages_one"
                        } else {
                            "nav.search.more_messages_other"
                        },
                        &[("count", &hit.more)],
                    ));
                }
            }
            _ => {
                body.push(
                    div()
                        .px(px(6.0))
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .child(t(if self.query.text().trim().is_empty() {
                            "nav.search.hint_empty"
                        } else {
                            "nav.search.hint"
                        }))
                        .into_any_element(),
                );
            }
        }
        if let Some(error) = &self.search.error {
            notes.insert(0, error.clone());
        } else if self.search.searching {
            notes.insert(0, t("nav.search.searching").to_owned());
        } else if self.search.transcripts_pending {
            notes.insert(0, t("nav.search.reading_chats").to_owned());
        }
        if self.has_remote_sessions() && !self.query.text().trim().is_empty() {
            let note = t("nav.search.remote_history_not_searched").to_owned();
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        div()
            .id("search-preview")
            .debug_selector(|| "search-preview".into())
            .h(px(PREVIEW_HEIGHT))
            .mx(px(PALETTE_ROW_INSET))
            .mb(px(PALETTE_ROW_INSET))
            .px(px(4.0))
            .py(px(6.0))
            .rounded(px(Radius::inner(PALETTE_RADIUS, PALETTE_ROW_INSET)))
            .bg(Fill::subtle(colors))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .children(body),
            )
            .child(caption(notes.join(" · ")).px(px(6.0)))
            .into_any_element()
    }

    fn has_remote_sessions(&self) -> bool {
        self.store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .values()
            .any(|session| session.host.is_some() && !session.is_archived())
    }
}

/// Seeds the page for the headless screenshots: `scene` is `empty` (who
/// needs you), `screen` (a scrollback hit, previewed) or `chat` (a reply).
#[cfg(all(test, target_os = "macos"))]
pub(super) fn seed_for_screenshot(overlay: &mut NavigationOverlay, scene: &str, cx: &mut Context<NavigationOverlay>) {
    use diri_proto::search::SearchLine;
    overlay.prepare_page(Overlay::Search, cx);
    if scene == "empty" {
        return;
    }
    let sessions = overlay.open_sessions();
    let id = |index: usize| sessions[index % sessions.len()].id.clone();
    let chat_id = sessions
        .iter()
        .find(|session| session.effective_kind().id() == diri_proto::AgentKind::CLAUDE_CODE_ID)
        .map_or_else(|| id(0), |session| session.id.clone());
    let line = |text: &str, word: &str, row: Option<i64>| {
        let start = text.find(word).map(|at| text[..at].chars().count());
        SearchLine {
            row,
            text: text.into(),
            ranges: start
                .map(|start| vec![[start as u32, (start + word.chars().count()) as u32]])
                .unwrap_or_default(),
        }
    };
    let plain = |text: &str| SearchLine {
        row: None,
        text: text.into(),
        ranges: Vec::new(),
    };
    overlay.query.insert("panicked");
    let page = &mut overlay.search;
    page.screens = vec![
        ScreenHit {
            session_id: id(1),
            lines: vec![
                line("thread 'parser::tests::empty_input' panicked at src/parser.rs:88:9:", "panicked", Some(40_212)),
                line("thread 'parser::tests::nested' panicked at src/parser.rs:131:5:", "panicked", Some(38_090)),
            ],
            context: vec![
                plain("    Finished `test` profile [unoptimized + debuginfo] target(s) in 9.13s"),
                plain("     Running unittests src/lib.rs (target/debug/deps/parser-9dff1ad0)"),
                line("thread 'parser::tests::empty_input' panicked at src/parser.rs:88:9:", "panicked", None),
                plain("called `Option::unwrap()` on a `None` value"),
                plain("note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"),
            ],
            more: true,
            screen_only: false,
        },
        ScreenHit {
            session_id: id(3),
            lines: vec![line(
                "thread 'main' panicked at crates/diri-app/src/main.rs:41:5:",
                "panicked",
                Some(912),
            )],
            context: Vec::new(),
            more: false,
            screen_only: false,
        },
    ];
    page.conversations = vec![ConversationHit {
        session_id: chat_id,
        role: "agent".into(),
        excerpt: line(
            "The test panicked because `parse()` unwraps the first token, and empty input has none. I made it return an empty document instead and added a regression test for it.",
            "panicked",
            None,
        ),
        messages_ago: 2,
        more: 1,
    }];
    page.searching = false;
    overlay.rebuild_search_rows();
    let wanted = if scene == "chat" { "chat:" } else { "screen:" };
    overlay.highlight = overlay
        .search
        .rows
        .iter()
        .position(|row| row.key(&overlay.search).starts_with(wanted))
        .unwrap_or(0);
}

/// One row's content: a leading mark, the title with its matches, the meta
/// beside it, and the second line with its matches (`mono` for terminal
/// text).
struct RowParts {
    leading: AnyElement,
    title: String,
    title_ranges: Vec<Range<usize>>,
    meta: String,
    line: String,
    line_ranges: Vec<Range<usize>>,
    mono: bool,
}

/// Folds a continuation page into what is shown: a session found again
/// gains its older lines; a new one joins the end (its lines are older than
/// anything the first page showed).
fn merge_screens(shown: &mut Vec<ScreenHit>, page: Vec<ScreenHit>) {
    for hit in page {
        match shown
            .iter()
            .position(|known| known.session_id == hit.session_id)
        {
            Some(index) => {
                let known = &mut shown[index];
                let room = (LINES_PER_SESSION as usize).saturating_sub(known.lines.len());
                known.more |= hit.more || hit.lines.len() > room;
                known.lines.extend(hit.lines.into_iter().take(room));
            }
            None if shown.len() < MAX_SCREEN_SESSIONS as usize => shown.push(hit),
            None => {}
        }
    }
}

/// What Find opens with: the whole query when the line holds it as typed,
/// else its longest word, so Find's newest match is this hit's line.
fn find_query(query: &str, hit: &ScreenHit) -> String {
    let words: Vec<&str> = query.split_whitespace().collect();
    let phrase = words.join(" ");
    let newest = hit.lines.first().map_or("", |line| line.text.as_str());
    let holds = if phrase.chars().any(char::is_uppercase) {
        newest.contains(&phrase)
    } else {
        newest.to_lowercase().contains(&phrase)
    };
    if holds {
        return phrase;
    }
    words
        .into_iter()
        .max_by_key(|word| word.chars().count())
        .unwrap_or_default()
        .to_owned()
}

/// Needs you, then finished and unseen, then working, then the rest; most
/// recently active first within each.
fn attention_order(session: &SessionRecord) -> (u8, std::cmp::Reverse<i64>) {
    use diri_proto::AttentionLevel;
    let rank = match session.attention() {
        AttentionLevel::NeedsInput => 0,
        AttentionLevel::DoneUnseen => 1,
        AttentionLevel::Working => 2,
        _ => 3,
    };
    (rank, std::cmp::Reverse(session.updated_at.0 as i64))
}

fn status_label(session: &SessionRecord) -> &'static str {
    use diri_proto::AttentionLevel;
    match session.attention() {
        AttentionLevel::NeedsInput => t("nav.search.status.needs_you"),
        AttentionLevel::DoneUnseen => t("nav.search.status.done"),
        AttentionLevel::Working => t("nav.search.status.working"),
        _ => "",
    }
}

fn folder_line(session: &SessionRecord) -> String {
    let folder = Path::new(&session.cwd)
        .file_name()
        .map_or_else(|| session.cwd.clone(), |name| name.to_string_lossy().into_owned());
    match (&session.git_branch, &session.host) {
        (Some(branch), Some(host)) => format!("{folder} · {branch} · {host}"),
        (Some(branch), None) => format!("{folder} · {branch}"),
        (None, Some(host)) => format!("{folder} · {host}"),
        (None, None) => folder,
    }
}

fn session_logo(session: Option<&SessionRecord>, colors: SemanticColors) -> AnyElement {
    let shell = diri_proto::AgentKind::SHELL;
    let kind = session.map_or(&shell, SessionRecord::effective_kind);
    AgentLogo::new(ui_agent_kind(kind), 28.0, colors)
        .badged(false)
        .into_any_element()
}

/// `char` ranges as the byte ranges a label highlights.
fn byte_ranges(text: &str, ranges: &[[u32; 2]]) -> Vec<Range<usize>> {
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect();
    ranges
        .iter()
        .filter_map(|[start, end]| {
            Some(*offsets.get(*start as usize)?..*offsets.get(*end as usize)?)
        })
        .collect()
}

/// [`byte_ranges`] for `text.trim()`, which a row shows.
fn byte_ranges_trimmed(text: &str, ranges: &[[u32; 2]]) -> Vec<Range<usize>> {
    let lead = text.len() - text.trim_start().len();
    byte_ranges(text, ranges)
        .into_iter()
        .filter(|range| range.start >= lead)
        .map(|range| range.start - lead..range.end - lead)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::search::SearchLine;

    fn hit(id: &str, lines: &[&str], more: bool) -> ScreenHit {
        ScreenHit {
            session_id: SessionId(id.into()),
            lines: lines
                .iter()
                .map(|text| SearchLine {
                    row: Some(1),
                    text: (*text).into(),
                    ranges: Vec::new(),
                })
                .collect(),
            context: Vec::new(),
            more,
            screen_only: false,
        }
    }

    #[test]
    fn continuation_pages_add_older_lines_up_to_the_cap() {
        let mut shown = vec![hit("a", &["a1", "a2"], false)];
        merge_screens(
            &mut shown,
            vec![hit("a", &["a3", "a4", "a5", "a6", "a7"], false), hit("b", &["b1"], false)],
        );
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].lines.len(), LINES_PER_SESSION as usize);
        assert!(shown[0].more);
        assert_eq!(shown[1].session_id.0, "b");
    }

    #[test]
    fn find_opens_with_the_phrase_when_the_line_holds_it() {
        let line = hit("a", &["E: cargo check failed"], false);
        assert_eq!(find_query(" cargo   check ", &line), "cargo check");
        assert_eq!(find_query("failed cargo", &line), "failed");
        assert_eq!(find_query("CARGO", &line), "CARGO");
    }

    #[test]
    fn char_ranges_become_byte_ranges() {
        assert_eq!(byte_ranges("é needle", &[[2, 8]]), vec![3..9]);
        assert_eq!(byte_ranges_trimmed("  needle", &[[2, 8]]), vec![0..6]);
        assert!(byte_ranges("short", &[[2, 80]]).is_empty());
    }
}
