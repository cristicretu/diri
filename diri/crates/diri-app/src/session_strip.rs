//! One quiet row under the terminal header: the session's pull requests with
//! their check status, a "needs your input" mark, and the preview a running
//! app exposes. It is hidden when there is nothing to say; the Links popover
//! keeps the full list.
use super::session_links::pr_summary;
use super::*;
use crate::palette_chrome::PaletteTooltip;
use diri_proto::{ArtifactKind, PullRequestStatus};
use diri_ui::{Icon, IconName};
use gpui::FontWeight;

const STRIP_HEIGHT: f32 = 26.0;
const MAX_PULL_REQUESTS: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum StripAction {
    /// Open in the default browser.
    Open(String),
    /// Open in the panel's Preview surface.
    Preview(String),
    /// Informational only.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChipIcon {
    Native(IconName),
    Symbol(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct StripChip {
    id: String,
    icon: ChipIcon,
    pub(super) label: String,
    /// A short status beside the label, with its tone.
    pub(super) status: Option<(String, gpui::Rgba)>,
    /// A tinted chip carries its own tone for text and fill.
    tint: Option<gpui::Rgba>,
    help: String,
    pub(super) action: StripAction,
}

fn short_pr_status(pr: &PullRequestStatus) -> String {
    match pr.state.as_str() {
        "MERGED" => "Merged".into(),
        "CLOSED" => "Closed".into(),
        _ if pr.checks_failed > 0 => format!("{} failed", pr.checks_failed),
        _ if pr.checks_pending > 0 => format!("{} running", pr.checks_pending),
        _ if pr.checks_passed > 0 => "Passed".into(),
        _ if pr.is_draft => "Draft".into(),
        _ => "Open".into(),
    }
}

/// The chips in display order: pull requests, the input mark, the preview.
pub(super) fn strip_chips(session: &SessionRecord) -> Vec<StripChip> {
    let mut chips = Vec::new();
    for (index, pr) in session
        .pull_requests
        .as_deref()
        .unwrap_or_default()
        .iter()
        .take(MAX_PULL_REQUESTS)
        .enumerate()
    {
        let (_, tone) = pr_summary(pr);
        chips.push(StripChip {
            id: format!("strip-pr-{index}"),
            icon: ChipIcon::Native(if pr.state == "MERGED" {
                IconName::Merge
            } else {
                IconName::PullRequest
            }),
            label: format!("#{}", pr.number),
            status: Some((short_pr_status(pr), tone)),
            tint: None,
            help: pr
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| format!("Pull request #{}", pr.number)),
            action: StripAction::Open(pr.url.clone()),
        });
    }
    if let Some(detail) = &session.needs_input {
        let tone = if detail.risk_hint == diri_proto::RiskHint::Destructive {
            Ink::DANGER
        } else {
            Ink::ATTENTION
        };
        chips.push(StripChip {
            id: "strip-input".into(),
            icon: ChipIcon::Symbol("bubble.left"),
            label: "Needs your input".into(),
            status: None,
            tint: Some(tone),
            help: detail.summary.clone(),
            action: StripAction::None,
        });
    }
    if let Some(port) = session
        .listening_ports
        .as_deref()
        .and_then(|ports| ports.first())
    {
        chips.push(StripChip {
            id: "strip-preview".into(),
            icon: ChipIcon::Native(IconName::Monitor),
            label: format!("localhost:{}", port.port),
            status: None,
            tint: None,
            help: format!("{} · open in Preview", port.process_name),
            action: StripAction::Preview(format!("http://localhost:{}", port.port)),
        });
    } else if let Some(preview) = session.artifacts.as_deref().and_then(|artifacts| {
        artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Preview)
    }) {
        chips.push(StripChip {
            id: "strip-preview".into(),
            icon: ChipIcon::Native(IconName::Monitor),
            label: url::Url::parse(&preview.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Preview".into()),
            status: None,
            tint: None,
            help: format!("{} · open in Preview", preview.url),
            action: StripAction::Preview(preview.url.clone()),
        });
    }
    chips
}

impl TerminalPane {
    pub(super) fn render_session_strip(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chips = strip_chips(session);
        if chips.is_empty() {
            return None;
        }
        let mut row = div()
            .id("session-strip")
            .debug_selector(|| "session-strip".into())
            .h(px(STRIP_HEIGHT))
            .flex_none()
            .px(px(Metrics::TOOLBAR_EDGE_INSET))
            .pb(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .bg(colors.work_surface_nested());
        for chip in chips {
            row = row.child(self.render_strip_chip(chip, colors, cx));
        }
        Some(row.into_any_element())
    }

    fn render_strip_chip(
        &self,
        chip: StripChip,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let StripChip {
            id,
            icon,
            label,
            status,
            tint,
            help,
            action,
        } = chip;
        let selector = id.clone();
        let text = tint.unwrap_or(colors.secondary);
        let fill = tint.map_or(colors.primary.alpha(0.05), |tone| tone.alpha(0.12));
        let hover_fill = tint.map_or(colors.primary.alpha(0.09), |tone| tone.alpha(0.18));
        let actionable = action != StripAction::None;
        let icon = match icon {
            ChipIcon::Native(name) => Icon::new(name, 11.0, text).into_any_element(),
            ChipIcon::Symbol(symbol) => sf_symbol(symbol, 10.5, text),
        };
        div()
            .id(SharedString::from(id))
            .debug_selector(move || selector.clone())
            .h(px(20.0))
            .px(px(7.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded_full()
            .bg(fill)
            .text_size(px(Typo::META.size))
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .when(actionable, |chip| {
                chip.cursor_pointer().hover(move |chip| chip.bg(hover_fill))
            })
            .child(icon)
            .child(label)
            .when_some(status, |chip, (status, tone)| {
                chip.child(div().size(px(5.0)).flex_none().rounded_full().bg(tone))
                    .child(div().text_color(tone).child(status))
            })
            .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(help.clone(), colors)).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |_, _, _, cx| {
                match &action {
                    StripAction::Open(url) => cx.open_url(url),
                    StripAction::Preview(url) => {
                        cx.emit(TerminalPaneEvent::OpenPreview { url: url.clone() })
                    }
                    StripAction::None => {}
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }
}

/// A session with one of everything the strip shows, for fixtures.
#[cfg(test)]
pub(super) fn seed_strip_fixture(session: &mut SessionRecord) {
    use super::tests::pull_request;
    let mut failing = pull_request("https://github.com/diri/app/pull/188");
    failing.number = 188;
    failing.title = Some("Reshape the right sidebar".into());
    failing.checks_failed = 1;
    failing.checks_pending = 0;
    let mut passing = pull_request("https://github.com/diri/app/pull/186");
    passing.number = 186;
    passing.checks_failed = 0;
    passing.checks_pending = 0;
    passing.checks_passed = 3;
    session.pull_requests = Some(vec![failing, passing]);
    session.needs_input = Some(diri_proto::NeedsInputDetail {
        kind: diri_proto::NeedsInputKind::Permission,
        source: diri_proto::NeedsInputSource::ClaudePermissionHook,
        tool_name: Some("Bash".into()),
        summary: "Allow `cargo test -p diri-app`?".into(),
        prompt_excerpt: None,
        options: None,
        risk_hint: diri_proto::RiskHint::Neutral,
        occurred_at: diri_proto::DateMillis(0.0),
    });
    session.listening_ports = Some(vec![diri_proto::PortInfo {
        port: 3000,
        process_name: "next-server".into(),
    }]);
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture_session;
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    #[test]
    fn strip_orders_pull_requests_input_and_preview_and_hides_when_empty() {
        let mut session = fixture_session();
        session.pull_requests = None;
        session.needs_input = None;
        session.listening_ports = None;
        session.artifacts = None;
        assert!(strip_chips(&session).is_empty(), "nothing to say, no strip");

        seed_strip_fixture(&mut session);
        let chips = strip_chips(&session);
        let ids: Vec<_> = chips.iter().map(|chip| chip.id.as_str()).collect();
        assert_eq!(
            ids,
            ["strip-pr-0", "strip-pr-1", "strip-input", "strip-preview"]
        );
        assert_eq!(chips[0].label, "#188");
        assert_eq!(
            chips[0].status.as_ref().map(|s| s.0.as_str()),
            Some("1 failed")
        );
        assert_eq!(chips[0].status.as_ref().map(|s| s.1), Some(Ink::DANGER));
        assert_eq!(
            chips[1].status.as_ref().map(|s| s.0.as_str()),
            Some("Passed")
        );
        assert_eq!(chips[2].label, "Needs your input");
        assert_eq!(chips[2].action, StripAction::None);
        assert_eq!(chips[3].label, "localhost:3000");
        assert_eq!(
            chips[3].action,
            StripAction::Preview("http://localhost:3000".into())
        );

        session.listening_ports = None;
        session.artifacts = Some(vec![diri_proto::SessionArtifact {
            kind: ArtifactKind::Preview,
            url: "https://preview.example.com/pr-188".into(),
            first_seen_at: diri_proto::DateMillis(0.0),
        }]);
        let chips = strip_chips(&session);
        assert_eq!(chips[3].label, "preview.example.com");
        assert_eq!(
            chips[3].action,
            StripAction::Preview("https://preview.example.com/pr-188".into())
        );
    }

    #[test]
    fn merged_and_draft_pull_requests_read_as_such() {
        let mut merged = super::super::tests::pull_request("https://example.com/pull/1");
        merged.state = "MERGED".into();
        assert_eq!(short_pr_status(&merged), "Merged");
        let mut draft = super::super::tests::pull_request("https://example.com/pull/2");
        draft.is_draft = true;
        draft.checks_passed = 0;
        draft.checks_failed = 0;
        draft.checks_pending = 0;
        assert_eq!(short_pr_status(&draft), "Draft");
    }

    /// The strip sits under the header and its preview chip asks the window
    /// for the Preview surface instead of leaving the app.
    #[gpui::test]
    fn strip_renders_under_the_header_and_asks_for_the_preview_surface(cx: &mut TestAppContext) {
        struct StripHarness {
            pane: Entity<TerminalPane>,
            previews: Vec<String>,
        }
        impl Render for StripHarness {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.pane.clone())
            }
        }
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = fixture_session();
        seed_strip_fixture(&mut session);
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            let pane = cx.new(|cx| TerminalPane::new(runtime, tokio, window, cx));
            cx.subscribe(&pane, |this: &mut StripHarness, _, event, _| {
                if let TerminalPaneEvent::OpenPreview { url } = event {
                    this.previews.push(url.clone());
                }
            })
            .detach();
            StripHarness {
                pane,
                previews: Vec::new(),
            }
        });
        cx.simulate_resize(gpui::size(px(800.0), px(500.0)));
        cx.run_until_parked();
        let header = cx
            .debug_bounds("session-links-trigger")
            .expect("terminal header");
        let strip = cx.debug_bounds("session-strip").expect("session strip");
        assert!(
            strip.top() >= header.bottom(),
            "the strip sits under the header"
        );
        assert!(cx.debug_bounds("strip-pr-0").is_some());
        assert!(cx.debug_bounds("strip-input").is_some());
        let preview = cx.debug_bounds("strip-preview").expect("preview chip");
        cx.simulate_click(preview.center(), Modifiers::default());
        cx.run_until_parked();
        harness.read_with(cx, |harness, _| {
            assert_eq!(harness.previews, ["http://localhost:3000"]);
        });

        let pane = harness.read_with(cx, |harness, _| harness.pane.clone());
        let mut quiet = fixture_session();
        quiet.pull_requests = None;
        quiet.needs_input = None;
        quiet.listening_ports = None;
        quiet.artifacts = None;
        pane.update(cx, |pane, cx| {
            pane.runtime.store.write().unwrap().upsert_session(quiet);
            cx.notify();
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("session-strip").is_none(),
            "an empty strip is not painted"
        );
    }
}
