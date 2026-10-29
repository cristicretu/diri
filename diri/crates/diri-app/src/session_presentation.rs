//! The working mark loops through pre-rasterized frames that the window
//! advances on its own (`diri_ui::FrameLoop`). No task, entity invalidation,
//! or frame request belongs to this mark, and no caller re-renders for it.

use diri_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel as ProtoAttentionLevel, SessionRecord,
};
use diri_ui::{AgentKind, FrameLoop, Icon, IconName, Ink, SemanticColors, StatusState};
use gpui::{AnyElement, IntoElement, Role, div, prelude::*, px};
use std::time::Duration;

static FRAMES: [&str; 8] = [
    "icons/working-0.svg",
    "icons/working-1.svg",
    "icons/working-2.svg",
    "icons/working-3.svg",
    "icons/working-4.svg",
    "icons/working-5.svg",
    "icons/working-6.svg",
    "icons/working-7.svg",
];

/// One working-mark frame lasts this long: eight frames, one lap a second.
const FRAME_INTERVAL: Duration = Duration::from_millis(125);

pub(crate) fn activity_mark(state: StatusState, colors: SemanticColors) -> AnyElement {
    // Match the project badge column; the mark itself stays optically smaller.
    let slot = div()
        .size(px(18.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match state {
        StatusState::Working => slot
            .child(FrameLoop::new(
                &FRAMES,
                FRAME_INTERVAL,
                14.0,
                colors.primary,
            ))
            .into_any_element(),
        StatusState::NeedsInput { destructive } => slot
            .child(Icon::new(
                IconName::Warning,
                14.0,
                Ink::on_surface(
                    if destructive {
                        Ink::DANGER
                    } else {
                        Ink::ATTENTION
                    },
                    colors,
                ),
            ))
            .into_any_element(),
        StatusState::DoneUnseen => slot
            .child(Icon::new(
                IconName::Check,
                14.0,
                Ink::on_surface(Ink::FRESH, colors),
            ))
            .into_any_element(),
        StatusState::Hibernated => slot
            .id("sleeping-status")
            .role(Role::Image)
            .aria_label(crate::i18n::t("session.sleeping"))
            .child(Icon::new(IconName::Moon, 13.0, colors.tertiary))
            .into_any_element(),
        StatusState::IdleSeen | StatusState::None => slot.into_any_element(),
    }
}

pub(crate) fn status_state(session: &SessionRecord, migrating: bool) -> StatusState {
    if migrating {
        return StatusState::Working;
    }
    if session.hibernation.is_some() {
        return StatusState::Hibernated;
    }
    match session.attention() {
        ProtoAttentionLevel::NeedsInput => StatusState::NeedsInput {
            destructive: session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive),
        },
        ProtoAttentionLevel::DoneUnseen => StatusState::DoneUnseen,
        ProtoAttentionLevel::Working => StatusState::Working,
        ProtoAttentionLevel::IdleSeen => StatusState::IdleSeen,
        ProtoAttentionLevel::None | ProtoAttentionLevel::Unknown => StatusState::None,
    }
}

/// What a blocked session's row says in place of its title: the question or
/// permission it is waiting on, when the program named it. A generic
/// "Waiting for …" placeholder (every Engine fallback reads that way, and a
/// password prompt never carries its text) keeps the title, which says more.
pub(crate) fn blocker_line(session: &SessionRecord) -> Option<String> {
    if session.hibernation.is_some()
        || !matches!(session.attention(), ProtoAttentionLevel::NeedsInput)
    {
        return None;
    }
    let detail = session.needs_input.as_ref().filter(|detail| !detail.secret)?;
    let text = one_line(&detail.summary);
    (!text.is_empty() && !text.starts_with("Waiting for")).then(|| capitalized(&text))
}

/// The second line of a row in the Activity filter: what the session is
/// doing or left for you, and since when (epoch milliseconds) for the
/// states that wait on you.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActivityLine {
    pub(crate) text: String,
    pub(crate) since_ms: Option<u64>,
}

/// `unseen` is the session's newest unread inbox entry, which is what an
/// unseen mark stands for.
pub(crate) fn activity_line(
    session: &SessionRecord,
    state: StatusState,
    unseen: Option<&crate::notification_feed::NotificationEntry>,
) -> ActivityLine {
    use crate::i18n::{t, tf};
    use crate::notification_feed::NotificationKind;
    match state {
        StatusState::NeedsInput { .. } => {
            let detail = session.needs_input.as_ref();
            let text = detail
                .filter(|detail| !detail.secret)
                .map(|detail| one_line(&detail.summary))
                .filter(|text| !text.is_empty())
                .map_or_else(|| t("activity.needs_you").to_owned(), |text| capitalized(&text));
            ActivityLine {
                text,
                since_ms: detail.map(|detail| detail.occurred_at.0.max(0.0) as u64),
            }
        }
        StatusState::Working => ActivityLine {
            text: match session.terminal_progress {
                Some(diri_proto::TerminalProgress {
                    state: diri_proto::TerminalProgressState::Normal,
                    percent,
                }) => tf("activity.working_percent", &[("percent", &percent)]),
                _ => t("activity.working").to_owned(),
            },
            since_ms: None,
        },
        _ => match unseen {
            Some(entry) => ActivityLine {
                text: match entry.kind {
                    NotificationKind::Done => t("activity.done").to_owned(),
                    NotificationKind::Failed => t("activity.failed").to_owned(),
                    NotificationKind::NeedsInput => t("activity.needed_you").to_owned(),
                    NotificationKind::Custom => one_line(&entry.title),
                },
                since_ms: Some(entry.created_at_ms),
            },
            // Finished per the Engine, with no inbox entry (posted before
            // this launch, or never): its last turn is the news.
            None if state == StatusState::DoneUnseen => ActivityLine {
                text: t("activity.done").to_owned(),
                since_ms: session
                    .last_turn_completed_at
                    .map(|at| at.0.max(0.0) as u64),
            },
            None if state == StatusState::Hibernated => ActivityLine {
                text: t("session.sleeping").to_owned(),
                since_ms: None,
            },
            None => ActivityLine {
                text: t("activity.idle").to_owned(),
                since_ms: None,
            },
        },
    }
}

/// A compact age ("now", "7m", "3h", "2d") and the epoch millisecond at
/// which it next reads differently, so a view can wake exactly then.
pub(crate) fn age_label(since_ms: u64, now_ms: u64) -> (String, u64) {
    use crate::i18n::{t, tf};
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let elapsed = now_ms.saturating_sub(since_ms);
    let (label, unit) = match elapsed {
        0..MINUTE => (t("notify.age_now").to_owned(), MINUTE),
        MINUTE..HOUR => (tf("notify.age_minutes", &[("n", &(elapsed / MINUTE))]), MINUTE),
        HOUR..DAY => (tf("notify.age_hours", &[("n", &(elapsed / HOUR))]), HOUR),
        _ => (tf("notify.age_days", &[("n", &(elapsed / DAY))]), DAY),
    };
    let next = since_ms.saturating_add((elapsed / unit + 1) * unit);
    (label, next)
}

fn one_line(text: &str) -> String {
    text.replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

pub(crate) fn is_loading(session: &SessionRecord, migrating: bool) -> bool {
    !migrating
        && session.hibernation.is_none()
        && session.effective_kind() != &ProtoAgentKind::SHELL
        && matches!(session.status, diri_proto::SessionStatus::Starting)
}

/// The glyph a terminal's mark takes from what it is running: a dev server
/// (it listens on a port) is a globe, a known program its kind of glyph, and
/// anything else keeps the shell's own mark. Generic glyphs, never a
/// project's logo, so the set stays small, monochrome and unlicensed.
pub(crate) fn program_glyph(session: &SessionRecord) -> Option<IconName> {
    if session.effective_kind() != &ProtoAgentKind::SHELL {
        return None;
    }
    if session.foreground_ports.as_ref().is_some_and(|ports| !ports.is_empty()) {
        return Some(IconName::Globe);
    }
    program_glyph_for(session.foreground_program.as_deref()?)
}

fn program_glyph_for(program: &str) -> Option<IconName> {
    // Linux reports `python3.12`, macOS `Python`; versions and case don't
    // change what a program is.
    let name = program.trim().to_ascii_lowercase();
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
    Some(match name {
        "vim" | "nvim" | "vi" | "hx" | "helix" | "nano" | "emacs" | "micro" | "kak" | "ed"
        | "zed" | "code" => IconName::Code,
        "git" | "lazygit" | "tig" | "gitui" | "gh" | "jj" | "glab" => IconName::Branch,
        "psql" | "pgcli" | "mysql" | "mariadb" | "mycli" | "sqlite" | "litecli" | "redis-cli"
        | "valkey-cli" | "mongosh" | "mongo" | "duckdb" | "clickhouse" | "usql" => {
            IconName::Database
        }
        "docker" | "podman" | "kubectl" | "k9s" | "helm" | "nerdctl" | "lazydocker"
        | "docker-compose" | "kind" | "minikube" => IconName::Cube,
        "cargo" | "rustc" | "make" | "gmake" | "cmake" | "ninja" | "go" | "swift"
        | "xcodebuild" | "gradle" | "mvn" | "bazel" | "just" | "zig" | "tsc" | "gcc"
        | "clang" | "dotnet" => IconName::Hammer,
        "node" | "bun" | "deno" | "npm" | "npx" | "pnpm" | "yarn" | "python" | "ipython"
        | "uv" | "pip" | "ruby" | "irb" | "rails" | "php" | "elixir" | "iex" | "julia" | "r"
        | "lua" | "ghci" => IconName::Stack,
        "ssh" | "mosh" | "autossh" | "et" | "telnet" => IconName::Server,
        "htop" | "top" | "btop" | "btm" | "atop" | "glances" | "nvtop" | "iotop" => {
            IconName::Activity
        }
        "less" | "more" | "man" | "bat" | "tail" | "most" => IconName::Text,
        _ => return None,
    })
}

pub(crate) fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    // Brand vocabulary, not a protocol type: any other Agent takes its mark
    // from the catalog, and one without a mark the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        ProtoAgentKind::NOTE_ID => AgentKind::Note,
        id => AgentKind::from_agent_id(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asking(summary: &str) -> SessionRecord {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .find(|session| session.kind == ProtoAgentKind::CLAUDE_CODE)
            .expect("claude fixture");
        session.hibernation = None;
        session.status =
            diri_proto::SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
        session.needs_input = Some(diri_proto::NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Permission,
            source: diri_proto::NeedsInputSource::ProgramStatus,
            tool_name: None,
            summary: summary.into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: diri_proto::DateMillis(1_000.0),
            secret: false,
        });
        session
    }

    #[test]
    fn a_named_question_replaces_the_title_until_it_is_answered() {
        assert_eq!(
            blocker_line(&asking("Allow deploy to staging?")).as_deref(),
            Some("Allow deploy to staging?")
        );
        // Engine phrasing is lower-case and quotes commands; the row reads
        // as a sentence on one line.
        assert_eq!(
            blocker_line(&asking("wants to run `npm\n test`")).as_deref(),
            Some("Wants to run npm test")
        );
        // Placeholders and password prompts keep the title.
        assert_eq!(blocker_line(&asking("Waiting for permission")), None);
        let mut secret = asking("Password:");
        secret.needs_input.as_mut().unwrap().secret = true;
        assert_eq!(blocker_line(&secret), None);
        // Answered: the title is back.
        let mut answered = asking("Allow deploy to staging?");
        answered.status = diri_proto::SessionStatus::Working;
        assert_eq!(blocker_line(&answered), None);
    }

    #[test]
    fn activity_lines_name_the_state_and_date_what_waits_on_you() {
        use crate::notification_feed::{NotificationEntry, NotificationKind};
        let session = asking("Allow deploy to staging?");
        let line = activity_line(&session, StatusState::NeedsInput { destructive: false }, None);
        assert_eq!(line.text, "Allow deploy to staging?");
        assert_eq!(line.since_ms, Some(1_000));
        let mut working = session.clone();
        working.terminal_progress = Some(diri_proto::TerminalProgress {
            state: diri_proto::TerminalProgressState::Normal,
            percent: 40,
        });
        let line = activity_line(&working, StatusState::Working, None);
        assert_eq!((line.text.as_str(), line.since_ms), ("Working · 40%", None));
        let entry = |kind| NotificationEntry {
            id: "e".into(),
            session_id: session.id.clone(),
            incarnation: 0,
            kind,
            title: "Build finished".into(),
            body: String::new(),
            created_at_ms: 5_000,
            read: false,
            resolved: false,
            blocker: String::new(),
        };
        let done = activity_line(&session, StatusState::DoneUnseen, Some(&entry(NotificationKind::Done)));
        assert_eq!((done.text.as_str(), done.since_ms), ("Finished", Some(5_000)));
        let failed = activity_line(&session, StatusState::DoneUnseen, Some(&entry(NotificationKind::Failed)));
        assert_eq!(failed.text, "Failed");
        assert_eq!(activity_line(&session, StatusState::IdleSeen, None).text, "Idle");
    }

    #[test]
    fn ages_change_exactly_at_their_next_boundary() {
        assert_eq!(age_label(0, 59_999), ("now".to_owned(), 60_000));
        assert_eq!(age_label(0, 60_000), ("1m".to_owned(), 120_000));
        assert_eq!(age_label(0, 7 * 60_000 + 5), ("7m".to_owned(), 8 * 60_000));
        assert_eq!(age_label(0, 3_600_000), ("1h".to_owned(), 7_200_000));
        assert_eq!(age_label(10, 0), ("now".to_owned(), 60_010));
    }

    #[test]
    fn terminals_take_a_glyph_from_their_program() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        let mut shell = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .find(|session| session.kind == ProtoAgentKind::SHELL)
            .expect("shell fixture");
        shell.foreground_agent = None;
        shell.foreground_ports = None;
        assert_eq!(program_glyph(&shell), None, "a prompt keeps the shell mark");
        for (program, glyph) in [
            ("nvim", Some(IconName::Code)),
            ("psql", Some(IconName::Database)),
            ("python3.12", Some(IconName::Stack)),
            ("Python", Some(IconName::Stack)),
            ("k9s", Some(IconName::Cube)),
            ("cargo", Some(IconName::Hammer)),
            ("lazygit", Some(IconName::Branch)),
            ("ssh", Some(IconName::Server)),
            ("htop", Some(IconName::Activity)),
            ("less", Some(IconName::Text)),
            ("my-script", None),
        ] {
            shell.foreground_program = Some(program.into());
            assert_eq!(program_glyph(&shell), glyph, "{program}");
        }
        // A program that serves a port reads as the server it is.
        shell.foreground_program = Some("node".into());
        shell.foreground_ports = Some(vec![diri_proto::PortInfo {
            port: 3000,
            process_name: "node".into(),
        }]);
        assert_eq!(program_glyph(&shell), Some(IconName::Globe));
        // An Agent typed at the prompt shows its own mark.
        shell.foreground_ports = None;
        shell.foreground_agent = Some(ProtoAgentKind::CLAUDE_CODE);
        assert_eq!(program_glyph(&shell), None);
    }

    #[test]
    fn plain_terminal_has_no_loading_or_working_indicator() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        use diri_proto::SessionStatus;

        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .find(|session| session.kind == ProtoAgentKind::SHELL)
            .expect("shell fixture");
        for status in [
            SessionStatus::Starting,
            SessionStatus::Working,
            SessionStatus::Idle,
        ] {
            session.status = status;
            assert!(!is_loading(&session, false));
            assert_eq!(status_state(&session, false), StatusState::None);
        }

        session.foreground_agent = Some(ProtoAgentKind::CLAUDE_CODE);
        session.status = SessionStatus::Starting;
        assert!(is_loading(&session, false));
        assert_eq!(status_state(&session, false), StatusState::Working);
        assert!(!is_loading(&session, true));
    }

    /// Every Agent Diri can launch or recognise has its own mark, and every
    /// bundled mark belongs to one: no Agent is left on the generic terminal
    /// treatment, and no mark is dead weight.
    #[test]
    fn every_agent_has_its_own_mark() {
        let (manifests, _) =
            diri_engine::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
                .unwrap();
        let mut ids: std::collections::BTreeSet<&str> = manifests.ids().into_iter().collect();
        ids.extend(
            diri_engine::recognition::CATALOG
                .iter()
                .map(|agent| agent.id),
        );
        for id in &ids {
            let kind = ui_agent_kind(&ProtoAgentKind::new(*id));
            match *id {
                ProtoAgentKind::GENERIC_ID => assert_eq!(kind, AgentKind::Generic),
                ProtoAgentKind::SHELL_ID => assert_eq!(kind, AgentKind::Shell),
                _ => assert!(kind.brand_mark().is_some(), "{id} has no mark"),
            }
        }
        for mark in &diri_ui::AGENT_MARKS {
            assert!(ids.contains(mark.id()), "{} is no Agent's mark", mark.id());
            assert_eq!(
                ui_agent_kind(&ProtoAgentKind::new(mark.id())),
                AgentKind::Agent(mark)
            );
        }
        // An id from a newer Engine falls back to the terminal treatment.
        assert_eq!(
            ui_agent_kind(&ProtoAgentKind::new("some-future-agent")),
            AgentKind::Generic
        );
    }

    /// Every Agent mark next to the four brand marks, as the sidebar draws
    /// them (a 16 px status glyph, rasterized by CoreGraphics) and at 32 px
    /// to judge their shapes, on a dark, a light and two themed palettes.
    ///
    /// `DIRI_VISUAL_OUTPUT=/tmp/marks.png cargo test -p diri-app
    /// render_agent_marks_screenshot -- --ignored`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the Agent mark grid screenshot"]
    fn render_agent_marks_screenshot() {
        use diri_term::theme::TermTheme;
        use diri_ui::{SemanticColors, StatusGlyph, Typo};
        use gpui::{HeadlessAppContext, Render, Window, size};
        use std::sync::Arc;

        struct Grid {
            panels: Vec<SemanticColors>,
            marks: Vec<(AgentKind, String)>,
        }

        impl Render for Grid {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let panel = |colors: SemanticColors, marks: &[(AgentKind, String)]| {
                    let cell = |(kind, name): &(AgentKind, String)| {
                        div()
                            .w(px(150.0))
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                StatusGlyph::new(*kind, StatusState::IdleSeen, 16.0, colors)
                                    .rendered_mark(),
                            )
                            .child(
                                StatusGlyph::new(*kind, StatusState::IdleSeen, 32.0, colors)
                                    .rendered_mark(),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.primary)
                                    .child(name.clone()),
                            )
                    };
                    div()
                        .w(px(620.0))
                        .p(px(14.0))
                        .flex()
                        .flex_wrap()
                        .bg(colors.background)
                        .children(marks.iter().map(cell))
                };
                div()
                    .flex()
                    .flex_wrap()
                    .size_full()
                    .children(self.panels.iter().map(|colors| panel(*colors, &self.marks)))
            }
        }

        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT");
        diri_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark_for_fixture);
        let (manifests, _) =
            diri_engine::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
                .unwrap();
        let mut marks: Vec<(AgentKind, String)> = [
            ProtoAgentKind::CLAUDE_CODE,
            ProtoAgentKind::CODEX,
            ProtoAgentKind::CURSOR,
            ProtoAgentKind::GEMINI,
        ]
        .iter()
        .map(|kind| (ui_agent_kind(kind), ui_agent_kind(kind).label().to_owned()))
        .collect();
        marks.extend(diri_ui::AGENT_MARKS.iter().map(|mark| {
            let name = diri_proto::agent_names::recognized_agent_name(mark.id())
                .map(str::to_owned)
                .or_else(|| {
                    let raw = manifests.raw_agent(mark.id())?.clone();
                    serde_json::from_value::<diri_proto::AgentDescriptor>(raw)
                        .ok()
                        .map(|descriptor| descriptor.display_name)
                })
                .unwrap_or_else(|| mark.id().to_owned());
            (AgentKind::Agent(mark), name)
        }));
        let panels = [
            TermTheme::DIRIJOR_DARK.id,
            TermTheme::DIRIJOR_LIGHT.id,
            TermTheme::TOKYO_NIGHT.id,
            TermTheme::SOLARIZED_LIGHT.id,
        ]
        .into_iter()
        .map(crate::app_theme::sidebar_colors)
        .collect();

        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window = cx
            .open_window(size(px(1240.0), px(1120.0)), |_, cx| {
                cx.new(|_| Grid { panels, marks })
            })
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }

    #[test]
    fn every_activity_frame_is_embedded_in_the_app() {
        use gpui::AssetSource;
        for path in FRAMES {
            let bytes = diri_ui::IconAssets
                .load(path)
                .expect("load activity frame")
                .expect(path);
            let svg = std::str::from_utf8(bytes.as_ref()).expect(path);
            assert!(svg.starts_with("<svg"), "{path}");
            assert!(
                svg.contains("currentColor"),
                "{path} must follow the theme color"
            );
        }
    }
}
