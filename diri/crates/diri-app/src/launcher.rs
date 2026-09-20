//! The prompt composer for work that already has a destination: text quoted
//! or dropped onto a session, and the review step of a handoff. It never
//! starts a session. New sessions launch directly (the New Agent shortcut and
//! menu) and take their first task in the agent's own prompt.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use diri_proto::{AgentKind, SessionId};
use diri_ui::{AgentKind as UiAgentKind, AgentLogo, Fill, Ink, Palette, Radius, SemanticColors};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyDownEvent, MouseButton, Render, Role, Task, Window, div, prelude::*, px, rgba,
};

use crate::AppServices;
use crate::composer::PromptComposer;
use crate::delegation::HandoffProposal;
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::CARET;
use crate::notifications::SendTextCommand;
use crate::query_editor::{self, ClipboardEdit, Edit};

const PANEL_WIDTH: f32 = 540.0;
const TITLE_HEIGHT: f32 = 36.0;
const TITLE_GAP: f32 = 22.0;
const CONTROL_SIZE: f32 = 32.0;
const CONTROL_RADIUS: f32 = 9.0;
const SHELF_HEIGHT: f32 = 40.0;

/// Composer metrics. The text area is sized from the wrapped line count
/// rather than pinned at one height: a one-line prompt should not sit in a
/// half-empty box, and a twenty-line one should not vanish out of the bottom
/// of a fixed one — it grows to [`COMPOSER_MAX_LINES`] and then scrolls,
/// following the caret.
const COMPOSER_FONT_SIZE: f32 = 13.0;
const COMPOSER_LINE_HEIGHT: f32 = 19.0;
const COMPOSER_MIN_LINES: usize = 2;
const COMPOSER_MAX_LINES: usize = 9;
const COMPOSER_INSET: f32 = 8.0;
const COMPOSER_PADDING: f32 = 16.0;
const COMPOSER_PAD_TOP: f32 = 12.0;
const COMPOSER_PAD_BOTTOM: f32 = 6.0;
const COMPOSER_CONTROLS_HEIGHT: f32 = 44.0;

/// The width text actually wraps at, derived from the panel so the two cannot
/// drift apart: the panel, less the composer's margin, padding and border.
const COMPOSER_TEXT_WIDTH: f32 = PANEL_WIDTH - 2.0 * COMPOSER_INSET - 2.0 * COMPOSER_PADDING - 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct LauncherSurfaceFills {
    composer: gpui::Rgba,
}

#[cfg(test)]
fn launcher_colors_for_theme(theme_id: &str) -> SemanticColors {
    crate::app_theme::colors(theme_id)
}

fn launcher_surface_fills(colors: SemanticColors) -> LauncherSurfaceFills {
    LauncherSurfaceFills {
        composer: colors.floating_surface(),
    }
}

const fn composer_text_height(lines: usize) -> f32 {
    let visible = if lines < COMPOSER_MIN_LINES {
        COMPOSER_MIN_LINES
    } else if lines > COMPOSER_MAX_LINES {
        COMPOSER_MAX_LINES
    } else {
        lines
    };
    visible as f32 * COMPOSER_LINE_HEIGHT + COMPOSER_PAD_TOP + COMPOSER_PAD_BOTTOM
}

pub(crate) enum LauncherEvent {
    Closed,
}

pub(crate) struct LauncherOverlay {
    services: Arc<AppServices>,
    focus: FocusHandle,
    prompt: PromptComposer,
    target: LauncherTarget,
    session_drafts: HashMap<SessionId, String>,
    mode: LauncherMode,
    /// The active destination draft survives a temporary handoff proposal.
    saved_prompt: Option<String>,
    delivery: DeliveryState,
    /// Drafts containing paths validated on this Mac cannot be submitted to a
    /// remote Agent. Pure text/quotes do not carry this restriction.
    session_drafts_with_local_paths: HashSet<SessionId>,
    fallback_notice: Option<String>,
    /// Finder drops may partially succeed. Keep their ignored-path detail
    /// inline with the staged draft until the user sends or replaces it; a
    /// toast would separate the reason from its action.
    drop_notice: Option<String>,
    open: bool,
    preview: bool,
    _store_changes: Task<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LauncherTarget {
    /// Closed, or reviewing a handoff, which names its own two sessions.
    Idle,
    Session(SessionId),
}

#[derive(Clone, Debug)]
enum LauncherMode {
    Compose,
    Handoff(HandoffProposal),
}

/// One acknowledged submission at a time. Tickets prevent a late completion
/// from an old proposal from closing or annotating a newer composer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeliveryState {
    next_ticket: u64,
    pending: Option<u64>,
}

impl DeliveryState {
    fn begin(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.pending = Some(self.next_ticket);
        self.pending
    }

    fn settle(&mut self, ticket: u64) -> bool {
        if self.pending != Some(ticket) {
            return false;
        }
        self.pending = None;
        true
    }

    fn invalidate(&mut self) {
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.pending = None;
    }

    const fn is_sending(self) -> bool {
        self.pending.is_some()
    }
}

impl EventEmitter<LauncherEvent> for LauncherOverlay {}

impl LauncherOverlay {
    pub(crate) fn new(services: Arc<AppServices>, preview: bool, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let mut changes = services.store.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.prune_session_drafts();
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Self {
            services,
            focus,
            prompt: PromptComposer::default(),
            target: LauncherTarget::Idle,
            session_drafts: HashMap::new(),
            mode: LauncherMode::Compose,
            saved_prompt: None,
            delivery: DeliveryState::default(),
            session_drafts_with_local_paths: HashSet::new(),
            fallback_notice: None,
            drop_notice: None,
            open: false,
            preview,
            _store_changes: store_changes,
        }
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    /// Open the native composer for one existing session and append staged
    /// context to that session's identity-keyed local draft. Merely opening
    /// this surface never writes to the PTY, selects the session, or wakes a
    /// hibernated process.
    pub(crate) fn open_for_session(
        &mut self,
        session_id: SessionId,
        insertion: &str,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delivery.is_sending() {
            return;
        }
        self.restore_prompt();
        self.switch_target(LauncherTarget::Session(session_id.clone()));
        self.prompt.append_context(insertion);
        self.drop_notice = notice;
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Finder paths are meaningful only on this Mac. Keep that provenance
    /// attached to the identity-keyed draft so a later target transition
    /// cannot accidentally make it submittable to a remote host.
    pub(crate) fn open_local_paths_for_session(
        &mut self,
        session_id: SessionId,
        insertion: &str,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delivery.is_sending() {
            return;
        }
        self.session_drafts_with_local_paths
            .insert(session_id.clone());
        self.open_for_session(session_id, insertion, notice, window, cx);
    }

    /// Drafts are keyed by session identity, and a removed session can never
    /// be targeted again; unsent text for it would otherwise stay for good.
    fn prune_session_drafts(&mut self) {
        if self.session_drafts.is_empty() && self.session_drafts_with_local_paths.is_empty() {
            return;
        }
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        let target = match &self.target {
            LauncherTarget::Session(id) => Some(id),
            LauncherTarget::Idle => None,
        };
        let keep = |id: &SessionId| store.sessions().contains_key(id) || target == Some(id);
        self.session_drafts.retain(|id, _| keep(id));
        self.session_drafts_with_local_paths.retain(|id| keep(id));
    }

    fn switch_target(&mut self, target: LauncherTarget) {
        if self.target == target {
            return;
        }
        let saved = transition_draft(
            &self.target,
            &target,
            self.prompt.text(),
            &mut self.session_drafts,
        );
        self.prompt.clear();
        if !saved.is_empty() {
            self.prompt.insert_multiline(&saved);
        }
        self.target = target;
    }

    /// Opens an identity-targeted review surface. Merely opening it cannot
    /// write to either session; the only send path is the labelled confirmation
    /// control rendered by `render_handoff_panel`.
    pub(crate) fn open_handoff(
        &mut self,
        proposal: HandoffProposal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delivery.is_sending() {
            return;
        }
        self.restore_prompt();
        self.saved_prompt = Some(self.prompt.text().to_owned());
        self.prompt.clear();
        self.prompt.insert_multiline(&proposal.summary);
        self.mode = LauncherMode::Handoff(proposal);
        self.delivery.invalidate();
        self.fallback_notice = None;
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open
            || (self.delivery.is_sending() && matches!(self.mode, LauncherMode::Handoff(_)))
        {
            return;
        }
        self.open = false;
        self.restore_prompt();
        cx.emit(LauncherEvent::Closed);
        cx.notify();
    }

    /// Close from outside the launcher (sidebar session click, menu bar, etc.).
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
    }

    fn restore_prompt(&mut self) {
        if !matches!(self.mode, LauncherMode::Handoff(_)) {
            return;
        }
        self.delivery.invalidate();
        self.prompt.clear();
        if let Some(prompt) = self.saved_prompt.take()
            && !prompt.is_empty()
        {
            self.prompt.insert_multiline(&prompt);
        }
        self.mode = LauncherMode::Compose;
    }

    /// Why the prompt cannot be sent yet, as something to show the user.
    /// `None` means it can. The submit button used to just sit there dimmed
    /// with no explanation, which reads as "broken" rather than "not yet".
    fn blocker(&self) -> Option<String> {
        if self.delivery.is_sending() {
            return Some(
                if matches!(self.mode, LauncherMode::Handoff(_)) {
                    "Sending handoff…"
                } else {
                    "Sending prompt…"
                }
                .to_owned(),
            );
        }
        if let LauncherMode::Handoff(proposal) = &self.mode {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let Some(target) = store.sessions().get(&proposal.target_id) else {
                return Some("The target session is no longer available".to_owned());
            };
            if target.is_archived() || matches!(target.status, diri_proto::SessionStatus::Exited(_))
            {
                return Some("The target session has ended".to_owned());
            }
            return None;
        }
        if let LauncherTarget::Session(id) = &self.target {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let Some(session) = store.sessions().get(id) else {
                return Some("This session is no longer available".to_owned());
            };
            return (session.host.is_some() && self.session_drafts_with_local_paths.contains(id))
                .then(|| "Local paths cannot be used on a remote session".to_owned());
        }
        // Nothing to send to: the composer only ever opens for a session or
        // a handoff, so this is a closed composer asked to submit.
        Some("Choose a session to send this to".to_owned())
    }

    fn can_submit(&self) -> bool {
        !self.preview
            && !self.delivery.is_sending()
            && !self.prompt.text().trim().is_empty()
            && self.blocker().is_none()
    }

    fn submit(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_submit() {
            return false;
        }
        if let Some(command) = handoff_command(&self.mode, self.prompt.text()) {
            let Some(ticket) = self.delivery.begin() else {
                return false;
            };
            self.fallback_notice = None;
            let client = Arc::clone(self.services.store.client());
            let runtime = Arc::clone(&self.services.tokio);
            cx.spawn(async move |this, cx| {
                let task = runtime.spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client
                        .send_text(&command.session_id, command.text, command.submit)
                        .await
                });
                let result = match task.await {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("handoff task stopped: {error}")),
                };
                let _ = this.update(cx, |this, cx| {
                    if !this.delivery.settle(ticket) {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            this.prompt.clear();
                            this.close(cx);
                        }
                        Err(error) => {
                            this.fallback_notice = Some(format!(
                                "The handoff was not sent: {error}. Review it and try again."
                            ));
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
            cx.notify();
            return true;
        }
        let LauncherTarget::Session(session_id) = self.target.clone() else {
            return false;
        };
        let Some(ticket) = self.delivery.begin() else {
            return false;
        };
        self.services
            .store
            .store
            .write()
            .expect("store lock")
            .dismiss_action_failure();
        let prompt = self.prompt.text().trim().to_owned();
        let client = Arc::clone(self.services.store.client());
        let runtime = Arc::clone(&self.services.tokio);
        self.fallback_notice = None;
        cx.spawn(async move |this, cx| {
            let destination = session_id.clone();
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.send_text(&destination, prompt, true).await
            });
            let result = task
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.finish_submission(ticket, session_id, result, cx);
            });
        })
        .detach();
        cx.notify();
        true
    }

    fn finish_submission(
        &mut self,
        ticket: u64,
        session_id: SessionId,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if !self.delivery.settle(ticket) {
            return;
        }
        match result {
            Ok(()) => {
                let mut store = self.services.store.store.write().expect("store lock");
                if self.open {
                    store.select(session_id.clone());
                }
                self.session_drafts.remove(&session_id);
                self.session_drafts_with_local_paths.remove(&session_id);
                drop(store);
                self.services.store.publish_local_change();
                self.prompt.clear();
                self.drop_notice = None;
                self.close(cx);
            }
            Err(error) => {
                self.fallback_notice = Some(
                    "Couldn’t confirm delivery. Check the session before sending again. Your draft is saved."
                        .into(),
                );
                self.services
                    .store
                    .store
                    .write()
                    .expect("store lock")
                    .report_prompt_delivery_failure(error);
                self.services.store.publish_local_change();
                cx.notify();
            }
        }
    }

    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Submission is already explicit at this point. Freeze the editor
        // until the daemon acknowledges it so post-submit edits cannot be
        // mistaken for content that was delivered. Escape may reveal the
        // workspace for Agent startup prompts; it does not cancel delivery.
        if self.delivery.is_sending() {
            if event.keystroke.key == "escape" {
                self.close(cx);
            }
            return true;
        }
        let shift = event.keystroke.modifiers.shift;
        match event.keystroke.key.as_str() {
            "escape" => {
                self.close(cx);
                true
            }
            "enter" if shift => {
                self.prompt.insert_multiline("\n");
                cx.notify();
                true
            }
            "enter" => self.submit(cx),
            "up" => {
                self.prompt.move_up(shift);
                cx.notify();
                true
            }
            "down" => {
                self.prompt.move_down(shift);
                cx.notify();
                true
            }
            _ => self.edit_prompt(event, cx),
        }
    }

    fn edit_prompt(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(edit) = query_editor::edit_for(&event.keystroke) else {
            return false;
        };
        match edit {
            Edit::Local(local) => {
                self.prompt.apply(local);
            }
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(self.prompt.editor(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Cut) => {
                query_editor::cut_selection(self.prompt.editor_mut(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Paste) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.prompt.insert_multiline(&text);
                }
            }
        }
        if self.prompt.text().is_empty()
            && let LauncherTarget::Session(id) = &self.target
        {
            // A remote user can recover from a rejected Finder drop by
            // clearing the draft, without weakening provenance while any of
            // the local insertion remains.
            self.session_drafts_with_local_paths.remove(id);
        }
        cx.notify();
        true
    }

    fn render_handoff_panel(
        &self,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let LauncherMode::Handoff(proposal) = &self.mode else {
            unreachable!("handoff panel requires a handoff proposal");
        };
        let sending = self.delivery.is_sending();
        let can_submit = self.can_submit();
        let blocker = self.blocker();
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        let composer_fill = if colors.appearance == diri_ui::Appearance::Dark {
            rgba(0x26282dff)
        } else {
            rgba(0xf2f1efff)
        };
        let remote_label = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            store
                .sessions()
                .get(&proposal.target_id)
                .and_then(|target| target.host.as_deref())
                .map(|host| format!("Remote · {}", store.host_display_name(host)))
        };
        let prompt = if self.prompt.is_empty() {
            div()
                .h(px(COMPOSER_LINE_HEIGHT))
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(div().text_color(colors.primary.alpha(0.92)).child(CARET))
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Describe the handoff…"),
                )
                .into_any_element()
        } else {
            div()
                .id("handoff-prompt-lines")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(self.prompt.scroll_handle())
                .children(self.prompt.render_lines(
                    px(COMPOSER_LINE_HEIGHT),
                    focused.then_some(CARET),
                    HighlightStyle {
                        background_color: Some(Palette::CLAY.alpha(0.35).into()),
                        ..HighlightStyle::default()
                    },
                ))
                .into_any_element()
        };

        div()
            .relative()
            .w(px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.primary.alpha(0.94))
                            .child("Review handoff"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(proposal.source_title.clone())
                            .child(sf_symbol("arrow.right", 9.0, colors.tertiary))
                            .child(proposal.target_title.clone())
                            .when_some(remote_label, |row, label| {
                                row.child(
                                    div()
                                        .ml(px(3.0))
                                        .px(px(7.0))
                                        .py(px(3.0))
                                        .rounded(px(Radius::CHIP))
                                        .bg(Ink::ATTENTION.alpha(0.11))
                                        .border_1()
                                        .border_color(Ink::ATTENTION.alpha(0.28))
                                        .text_size(px(9.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Ink::ATTENTION)
                                        .child(label),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mx(px(COMPOSER_INSET))
                    .h(px(composer_height))
                    .rounded(px(Radius::PANEL))
                    .bg(composer_fill)
                    .border_1()
                    .border_color(if focused {
                        Palette::CLAY.alpha(0.42)
                    } else {
                        colors.primary.alpha(0.09)
                    })
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| window.focus(&focus, cx)
                    })
                    .child(
                        div()
                            .h(px(text_height))
                            .px(px(COMPOSER_PADDING))
                            .pt(px(COMPOSER_PAD_TOP))
                            .pb(px(COMPOSER_PAD_BOTTOM))
                            .text_size(px(COMPOSER_FONT_SIZE))
                            .line_height(px(COMPOSER_LINE_HEIGHT))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_CONTROLS_HEIGHT))
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .text_size(px(10.0))
                                    .text_color(if self.fallback_notice.is_some() {
                                        Ink::ATTENTION
                                    } else {
                                        colors.tertiary
                                    })
                                    .child(
                                        blocker
                                            .or_else(|| self.fallback_notice.clone())
                                            .unwrap_or_else(|| {
                                                "Review and edit before sending · ⇧↵ new line"
                                                    .to_owned()
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .child(
                                        div()
                                            .id("handoff-cancel")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(10.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(CONTROL_RADIUS))
                                            .text_size(px(11.0))
                                            .text_color(if sending {
                                                colors.tertiary
                                            } else {
                                                colors.secondary
                                            })
                                            .when(!sending, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(move |button| {
                                                        button.bg(Fill::subtle(colors))
                                                    })
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.close(cx);
                                                    }))
                                            })
                                            .child("Cancel"),
                                    )
                                    .child(
                                        div()
                                            .id("handoff-submit")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(12.0))
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .bg(if can_submit {
                                                colors.primary
                                            } else {
                                                Fill::subtle(colors)
                                            })
                                            .when(can_submit, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(move |button| button.opacity(0.86))
                                                    .active(move |button| button.opacity(0.72))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.submit(cx);
                                                    }))
                                            })
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if can_submit {
                                                colors.background
                                            } else {
                                                colors.tertiary
                                            })
                                            .child(sf_symbol_weighted(
                                                "paperplane.fill",
                                                10.0,
                                                SymbolWeight::Semibold,
                                                if can_submit {
                                                    colors.background
                                                } else {
                                                    colors.tertiary
                                                },
                                            ))
                                            .child(if sending {
                                                "Sending…"
                                            } else {
                                                "Send handoff"
                                            }),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_session_panel(
        &self,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = match &self.target {
            LauncherTarget::Session(id) => self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(id)
                .cloned(),
            LauncherTarget::Idle => None,
        };
        let title = session.as_ref().map_or_else(
            || "Unavailable session".to_owned(),
            |session| session.title.clone(),
        );
        let cwd = session.as_ref().map_or_else(
            || "Session no longer available".to_owned(),
            |session| session.cwd.clone(),
        );
        let logo = session.as_ref().map_or(UiAgentKind::Generic, |session| {
            ui_agent_kind(session.effective_kind())
        });
        let can_submit = self.can_submit();
        let draft_state = session.as_ref().map_or("Local draft", |session| {
            if session.hibernation.is_some() {
                "Local draft · sleeping agent untouched"
            } else {
                "Local draft · nothing sent"
            }
        });
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        let fills = launcher_surface_fills(colors);
        let prompt = if self.prompt.is_empty() {
            div()
                .h(px(COMPOSER_LINE_HEIGHT))
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(div().text_color(colors.primary.alpha(0.92)).child(CARET))
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Add context or instructions…"),
                )
                .into_any_element()
        } else {
            div()
                .id("session-composer-prompt-lines")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(self.prompt.scroll_handle())
                .children(self.prompt.render_lines(
                    px(COMPOSER_LINE_HEIGHT),
                    focused.then_some(CARET),
                    HighlightStyle {
                        background_color: Some(Palette::GEMINI_BLUE.alpha(0.30).into()),
                        ..HighlightStyle::default()
                    },
                ))
                .into_any_element()
        };

        div()
            .relative()
            .w(px(PANEL_WIDTH))
            .child(
                div()
                    .h(px(TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .child(sf_symbol("paperclip", 16.0, Palette::GEMINI_BLUE))
                    .child(
                        div()
                            .max_w(px(PANEL_WIDTH - 52.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.primary.alpha(0.94))
                            .child(format!("Add context to {title}")),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mt(px(TITLE_GAP))
                    .mx(px(COMPOSER_INSET))
                    .h(px(composer_height))
                    .rounded(px(Radius::PANEL))
                    .bg(fills.composer)
                    .border_1()
                    .border_color(if focused {
                        Palette::GEMINI_BLUE.alpha(0.46)
                    } else {
                        colors.primary.alpha(0.09)
                    })
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| window.focus(&focus, cx)
                    })
                    .child(
                        div()
                            .h(px(text_height))
                            .px(px(COMPOSER_PADDING))
                            .pt(px(COMPOSER_PAD_TOP))
                            .pb(px(COMPOSER_PAD_BOTTOM))
                            .text_size(px(COMPOSER_FONT_SIZE))
                            .line_height(px(COMPOSER_LINE_HEIGHT))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_CONTROLS_HEIGHT))
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(div().text_size(px(10.0)).text_color(colors.tertiary).child(
                                self.blocker().unwrap_or_else(|| {
                                    "Review first — dropping sent nothing".to_owned()
                                }),
                            ))
                            .child(
                                div()
                                    .id("session-composer-submit")
                                    .size(px(CONTROL_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(CONTROL_RADIUS))
                                    .bg(if can_submit {
                                        colors.primary
                                    } else {
                                        Fill::subtle(colors)
                                    })
                                    .when(can_submit, |button| {
                                        button
                                            .cursor_pointer()
                                            .hover(move |button| button.opacity(0.86))
                                            .active(move |button| button.opacity(0.72))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit(cx);
                                            }))
                                    })
                                    .child(sf_symbol_weighted(
                                        "arrow.up",
                                        10.0,
                                        SymbolWeight::Bold,
                                        if can_submit {
                                            colors.background
                                        } else {
                                            colors.tertiary
                                        },
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mx(px(16.0))
                    .h(px(SHELF_HEIGHT))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(AgentLogo::new(logo, 17.0, colors).badged(false))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(cwd),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(colors.tertiary)
                            .child(draft_state),
                    ),
            )
            .when_some(self.drop_notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .id("session-composer-drop-notice")
                        .mt(px(9.0))
                        .mx(px(COMPOSER_INSET))
                        .px(px(9.0))
                        .py(px(7.0))
                        .flex()
                        .items_start()
                        .gap(px(7.0))
                        .rounded(px(Radius::ROW))
                        .bg(Ink::ATTENTION.alpha(0.08))
                        .border_1()
                        .border_color(Ink::ATTENTION.alpha(0.20))
                        .child(sf_symbol(
                            "exclamationmark.circle.fill",
                            11.0,
                            Ink::ATTENTION,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .text_color(colors.secondary)
                                .child(notice),
                        ),
                )
            })
            .when_some(self.fallback_notice.clone(), |panel, message| {
                panel.child(
                    div()
                        .mx(px(COMPOSER_INSET))
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(colors.secondary)
                        .child(message),
                )
            })
            .into_any_element()
    }
}

impl Focusable for LauncherOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LauncherOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .id("prompt-composer")
            .key_context("DiriLauncher")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.handle_key_down(event, window, cx);
            }));
        if !self.open {
            return root.size(px(0.0));
        }

        // Soft-wrapping needs the text system, which only exists here. Doing
        // it before the panel is built is what lets the composer size itself
        // to the prompt and scroll the caret into view.
        let text_width = COMPOSER_TEXT_WIDTH;
        self.prompt.layout(
            px(text_width),
            gpui::font(crate::fonts::ui_family()),
            px(COMPOSER_FONT_SIZE),
            window,
        );

        let colors = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::colors_in(&store)
        };
        let focused = self.focus.is_focused(window);
        root.size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.background)
            // The whole canvas behaves like the editor's: a click anywhere
            // returns focus to the prompt.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("launcher-back")
                    .absolute()
                    .top(px(12.0))
                    .right(px(16.0))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(Radius::ROW))
                    .role(Role::Button)
                    .aria_label("Back to workspace")
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx)))
                    .child("Back")
                    .child(div().text_color(colors.tertiary).child("esc")),
            )
            .child(
                div()
                    .relative()
                    .child(if matches!(self.mode, LauncherMode::Handoff(_)) {
                        self.render_handoff_panel(colors, focused, cx)
                    } else {
                        self.render_session_panel(colors, focused, cx)
                    })
                    .when(self.delivery.is_sending(), |panel| {
                        panel.child(
                            div()
                                .absolute()
                                .inset_0()
                                .occlude()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_mouse_down(MouseButton::Right, |_, _, cx| {
                                    cx.stop_propagation()
                                }),
                        )
                    }),
            )
    }
}

fn transition_draft(
    current: &LauncherTarget,
    next: &LauncherTarget,
    current_text: &str,
    session_drafts: &mut HashMap<SessionId, String>,
) -> String {
    match current {
        LauncherTarget::Idle => {}
        LauncherTarget::Session(id) if current_text.is_empty() => {
            session_drafts.remove(id);
        }
        LauncherTarget::Session(id) => {
            session_drafts.insert(id.clone(), current_text.to_owned());
        }
    }
    match next {
        LauncherTarget::Idle => String::new(),
        LauncherTarget::Session(id) => session_drafts.get(id).cloned().unwrap_or_default(),
    }
}

fn handoff_command(mode: &LauncherMode, text: &str) -> Option<SendTextCommand> {
    let LauncherMode::Handoff(proposal) = mode else {
        return None;
    };
    let text = text.trim();
    (!text.is_empty()).then(|| SendTextCommand {
        session_id: proposal.target_id.clone(),
        text: text.to_owned(),
        submit: true,
    })
}

fn ui_agent_kind(kind: &AgentKind) -> UiAgentKind {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => UiAgentKind::ClaudeCode,
        AgentKind::CODEX_ID => UiAgentKind::Codex,
        AgentKind::CURSOR_ID => UiAgentKind::Cursor,
        AgentKind::GEMINI_ID => UiAgentKind::Gemini,
        AgentKind::SHELL_ID => UiAgentKind::Shell,
        _ => UiAgentKind::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use crate::store::StoreRuntime;
    use crate::usage::UsageSnapshot;
    use gpui::{Keystroke, TestAppContext};

    fn test_services(store: Arc<StoreRuntime>) -> Arc<AppServices> {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        Arc::new(AppServices {
            store,
            usage_tx,
            usage_limits_refresh: tokio::sync::mpsc::channel(1).0,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        })
    }

    #[gpui::test]
    fn drafts_for_removed_sessions_are_pruned(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let live = fixture.list.sessions[0].id.clone();
        let removed = fixture.list.sessions[1].id.clone();
        runtime.store.write().expect("store").hydrate(fixture.list);
        let services = test_services(runtime.clone());
        let (launcher, cx) =
            cx.add_window_view(move |_, cx| LauncherOverlay::new(services, false, cx));
        launcher.update(cx, |launcher, _| {
            for id in [&live, &removed] {
                launcher.session_drafts.insert(id.clone(), "draft".into());
                launcher.session_drafts_with_local_paths.insert(id.clone());
            }
            runtime
                .store
                .write()
                .expect("store")
                .remove_session_record(&removed);
            launcher.prune_session_drafts();
            assert_eq!(
                launcher.session_drafts.keys().collect::<Vec<_>>(),
                vec![&live]
            );
            assert!(!launcher.session_drafts_with_local_paths.contains(&removed));
            assert!(launcher.session_drafts_with_local_paths.contains(&live));
        });
    }

    #[gpui::test]
    fn composer_keeps_failed_drafts_and_closes_only_after_acknowledgement(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let target = fixture
            .list
            .sessions
            .iter()
            .find(|session| session.host.is_none())
            .expect("a local session")
            .id
            .clone();
        let other = fixture
            .list
            .sessions
            .iter()
            .find(|session| session.id != target)
            .expect("a second session")
            .id
            .clone();
        runtime.store.write().expect("store").hydrate(fixture.list);
        let services = test_services(runtime);
        let (launcher, cx) =
            cx.add_window_view(move |_, cx| LauncherOverlay::new(services, false, cx));
        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open_for_session(target.clone(), "Review the changes", None, window, cx);
            assert!(launcher.submit(cx));
            assert!(launcher.open);
            assert_eq!(launcher.prompt.text(), "Review the changes");
            assert!(!launcher.submit(cx), "double click cannot send twice");
            launcher.handle_key_down(&key("escape"), window, cx);
            launcher.open_for_session(other, "different draft", None, window, cx);
            assert!(
                !launcher.open,
                "Escape reveals the workspace without cancelling delivery"
            );
            assert_eq!(launcher.target, LauncherTarget::Session(target.clone()));
            assert_eq!(launcher.prompt.text(), "Review the changes");
            launcher.open = true;
            let ticket = launcher.delivery.pending.expect("awaiting daemon");
            launcher.finish_submission(
                ticket,
                target.clone(),
                Err("test delivery failed".into()),
                cx,
            );
            assert!(launcher.open);
            assert!(launcher.can_submit());
            assert_eq!(launcher.prompt.text(), "Review the changes");
            assert!(
                launcher
                    .fallback_notice
                    .as_deref()
                    .unwrap()
                    .contains("Your draft is saved")
            );
            assert!(launcher.submit(cx));
            let retry = launcher.delivery.pending.unwrap();
            launcher.finish_submission(ticket, target.clone(), Ok(()), cx);
            assert!(
                launcher.open,
                "stale completion cannot erase the retry draft"
            );
            launcher.finish_submission(retry, target, Ok(()), cx);
            assert!(!launcher.open);
            assert!(launcher.prompt.is_empty());
        });
    }

    #[gpui::test]
    fn a_composer_with_no_destination_never_sends(cx: &mut TestAppContext) {
        let services = test_services(Arc::new(StoreRuntime::inert()));
        let (launcher, cx) =
            cx.add_window_view(move |_, cx| LauncherOverlay::new(services, false, cx));
        launcher.update(cx, |launcher, cx| {
            // Starting sessions is not this surface's job any more: with no
            // session and no handoff there is nothing a submit could mean.
            launcher.prompt.insert_multiline("Build me a website");
            assert_eq!(launcher.target, LauncherTarget::Idle);
            assert!(!launcher.can_submit());
            assert!(!launcher.submit(cx));
            assert_eq!(launcher.delivery.pending, None);
        });
    }

    fn key(value: &str) -> KeyDownEvent {
        let mut keystroke = Keystroke::parse(value).expect("valid test key");
        if !keystroke.modifiers.platform
            && !keystroke.modifiers.control
            && !keystroke.modifiers.function
            && keystroke.key.chars().count() == 1
        {
            keystroke.key_char = Some(keystroke.key.clone());
        }
        KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn launcher_uses_the_selected_diri_theme_and_semantic_surfaces() {
        let colors = launcher_colors_for_theme("dirijor-light");
        let expected = crate::app_theme::colors("dirijor-light");
        let fills = launcher_surface_fills(colors);

        assert_eq!(colors, expected);
        assert_eq!(fills.composer, expected.floating_surface());
    }

    #[test]
    fn each_session_keeps_an_independent_draft_and_idle_keeps_none() {
        let idle = LauncherTarget::Idle;
        let first = LauncherTarget::Session(SessionId("first".into()));
        let second = LauncherTarget::Session(SessionId("second".into()));
        let mut sessions = HashMap::new();

        assert_eq!(transition_draft(&idle, &first, "", &mut sessions), "");
        assert_eq!(
            transition_draft(&first, &second, "review this\n'/tmp/one.rs'", &mut sessions),
            ""
        );
        assert_eq!(
            transition_draft(&second, &first, "compare '/tmp/two.rs'", &mut sessions),
            "review this\n'/tmp/one.rs'"
        );
        assert_eq!(
            transition_draft(&first, &idle, "review this\n'/tmp/one.rs'", &mut sessions),
            "",
            "there is no new-session draft to fall back to"
        );
        assert_eq!(
            transition_draft(&idle, &second, "", &mut sessions),
            "compare '/tmp/two.rs'"
        );
    }

    #[test]
    fn handoff_is_inert_until_explicit_submit_builds_one_targeted_command() {
        let proposal = HandoffProposal {
            source_id: SessionId("source".into()),
            target_id: SessionId("target".into()),
            source_title: "Source".into(),
            target_title: "Target".into(),
            summary: "cached summary".into(),
        };
        assert_eq!(handoff_command(&LauncherMode::Compose, "edited"), None);
        assert_eq!(
            handoff_command(&LauncherMode::Handoff(proposal), "  edited summary  "),
            Some(SendTextCommand {
                session_id: SessionId("target".into()),
                text: "edited summary".into(),
                submit: true,
            })
        );
    }

    #[test]
    fn delivery_accepts_one_send_and_ignores_stale_completions() {
        let mut delivery = DeliveryState::default();
        let first = delivery.begin().expect("first send");
        assert!(delivery.is_sending());
        assert_eq!(delivery.begin(), None, "double submit must be refused");

        delivery.invalidate();
        let replacement = delivery.begin().expect("replacement proposal send");
        assert_ne!(first, replacement);
        assert!(
            !delivery.settle(first),
            "an old RPC must not close a replacement composer"
        );
        assert!(delivery.is_sending());
        assert!(delivery.settle(replacement));
        assert!(!delivery.is_sending());
    }

    #[gpui::test]
    fn staging_context_uses_the_requested_identity_without_selecting_or_sending(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(crate::store::StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        assert!(fixture.list.sessions.len() >= 2);
        let active = fixture.list.sessions[0].id.clone();
        let target = fixture.list.sessions[1].id.clone();
        fixture.list.sessions[0].kind = AgentKind::CODEX;
        fixture.list.sessions[1].kind = AgentKind::CLAUDE_CODE;
        fixture.list.sessions[0].foreground_agent = None;
        fixture.list.sessions[1].foreground_agent = None;
        fixture.list.sessions[1].host = Some("build-box".to_owned());
        fixture.list.sessions[1].hibernation = Some(diri_proto::HibernationInfo {
            since: diri_proto::DateMillis(1.0),
            reason: diri_proto::HibernationReason::Manual,
            tree_pids: vec![42],
            tree_start_times: None,
        });
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(active.clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        let services = Arc::new(AppServices {
            store: Arc::clone(&runtime),
            usage_tx,
            usage_limits_refresh: tokio::sync::mpsc::channel(1).0,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        });
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open_for_session(target.clone(), "first quoted turn", None, window, cx);
            launcher.open_for_session(target.clone(), "second quoted turn", None, window, cx);
        });

        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id(),
            Some(&active),
            "staging a different target must not switch the active session"
        );
        assert!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(&target)
                .is_some_and(|record| record.hibernation.is_some()),
            "an app-owned draft cannot wake or rewrite hibernation state"
        );
        launcher.read_with(cx, |launcher, _| {
            assert_eq!(launcher.target, LauncherTarget::Session(target.clone()));
            assert_eq!(
                launcher.blocker(),
                None,
                "plain quoted text must remain submittable to a remote agent"
            );
            assert_eq!(
                launcher.prompt.text(),
                "first quoted turn\nsecond quoted turn"
            );
            assert!(launcher.open);
        });

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open_local_paths_for_session(
                target.clone(),
                "'/Users/me/local.png'",
                None,
                window,
                cx,
            );
        });
        launcher.read_with(cx, |launcher, _| {
            assert_eq!(
                launcher.blocker().as_deref(),
                Some("Local paths cannot be used on a remote session")
            );
        });
    }
}
