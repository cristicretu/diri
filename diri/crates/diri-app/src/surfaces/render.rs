//! The pieces of a surface's look that need no GPUI context: tone colours,
//! link badges, and live bindings resolved against the session store. Kept
//! apart from the view so they are unit-tested without a window.

use diri_proto::surface::link::{self, LinkKind};
use diri_proto::surface::{Binding, Tone};
use diri_proto::{SessionId, SessionRecord, SessionStatus};
use diri_ui::{Ink, Palette, SemanticColors};
use gpui::Rgba;

use crate::store::SessionStore;

/// The ink for a tone on this palette.
pub(crate) fn tone_color(tone: Tone, colors: SemanticColors) -> Rgba {
    match tone {
        Tone::Neutral => colors.primary,
        Tone::Muted => colors.secondary,
        Tone::Info => Palette::GEMINI_BLUE,
        Tone::Success => Ink::FRESH,
        Tone::Warning => Ink::ATTENTION,
        Tone::Danger => Ink::DANGER,
        Tone::Accent => crate::notes::editor_view::accent(),
    }
}

/// A badge's fill: the tone, faint, so its text stays the strong element.
pub(crate) fn tone_fill(tone: Tone, colors: SemanticColors) -> Rgba {
    match tone {
        Tone::Neutral | Tone::Muted => Rgba {
            a: 0.07,
            ..colors.primary
        },
        _ => Rgba {
            a: 0.14,
            ..tone_color(tone, colors)
        },
    }
}

/// How a link presents itself: icon, badge word, label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkLook {
    pub icon: &'static str,
    pub badge: &'static str,
    pub label: String,
    pub internal: bool,
}

pub(crate) fn link_look(url: &str, label: Option<&str>, store: Option<&SessionStore>) -> LinkLook {
    let kind = if link::is_allowed(url) {
        link::classify(url)
    } else {
        LinkKind::Web {
            host: String::new(),
        }
    };
    let icon = match &kind {
        LinkKind::Slack => "slack",
        LinkKind::GithubPull { .. } => "arrow.triangle.pull",
        LinkKind::GithubIssue { .. } | LinkKind::Github { .. } => "github",
        LinkKind::Linear { .. } => "linear",
        LinkKind::Notion => "notion",
        LinkKind::Figma => "figma",
        LinkKind::DiriSession { .. } => "terminal",
        LinkKind::DiriNote { .. } => "doc.text",
        LinkKind::DiriSurface { .. } => "square.grid.2x2",
        LinkKind::Mail => "bubble.left",
        LinkKind::Web { .. } => "link",
    };
    // A session link reads as the session's live title.
    let live = match (&kind, store) {
        (LinkKind::DiriSession { session }, Some(store)) => store
            .sessions()
            .get(&SessionId::new(session))
            .map(|record| record.title.clone()),
        _ => None,
    };
    LinkLook {
        icon,
        badge: kind.badge(),
        label: label
            .map(str::to_owned)
            .filter(|label| !label.trim().is_empty())
            .or(live)
            .unwrap_or_else(|| kind.default_label()),
        internal: kind.is_internal(),
    }
}

/// A binding's live value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub text: String,
    pub tone: Tone,
}

fn is_live(record: &SessionRecord) -> bool {
    !record.is_note() && !record.is_archived()
}

fn needs_input(record: &SessionRecord) -> bool {
    matches!(record.status, SessionStatus::NeedsInput(_)) || record.needs_input.is_some()
}

/// The status words and tone of one session, as notes and the To-dos page
/// show them.
pub(crate) fn session_status(record: &SessionRecord) -> Resolved {
    let facts = diri_notes::work::SessionFacts::from_record(record);
    let state = diri_notes::work::state(false, Some(Some(&facts)));
    use diri_notes::work::WorkState;
    let tone = match &state {
        WorkState::NeedsYou(_) => Tone::Warning,
        WorkState::Review(_) => Tone::Accent,
        WorkState::Starting | WorkState::Working => Tone::Info,
        _ => Tone::Muted,
    };
    Resolved {
        text: state.label().to_owned(),
        tone,
    }
}

/// Reads a binding from live Diri state. `None` when its subject is gone,
/// so the component falls back to what the agent wrote.
pub(crate) fn resolve(
    binding: &Binding,
    store: &SessionStore,
    note_todos: &dyn Fn(&str) -> Option<usize>,
) -> Option<Resolved> {
    let session = |id: &str| store.sessions().get(&SessionId::new(id)).cloned();
    let count = |filter: &dyn Fn(&SessionRecord) -> bool, tone: Tone| {
        let n = store
            .sessions()
            .values()
            .filter(|record| is_live(record) && filter(record))
            .count();
        Some(Resolved {
            text: n.to_string(),
            tone: if n == 0 { Tone::Muted } else { tone },
        })
    };
    match binding {
        Binding::SessionStatus { session: id } => session(id).map(|r| session_status(&r)),
        Binding::SessionTitle { session: id } => session(id).map(|r| Resolved {
            text: r.title.clone(),
            tone: Tone::Neutral,
        }),
        Binding::WorkingCount => count(&|r| matches!(r.status, SessionStatus::Working), Tone::Info),
        Binding::NeedsInputCount => count(&needs_input, Tone::Warning),
        Binding::UnreadCount => count(
            &|r| match (r.last_turn_completed_at, r.last_seen_at) {
                (Some(done), Some(seen)) => done.0 > seen.0,
                (Some(_), None) => true,
                _ => false,
            },
            Tone::Accent,
        ),
        Binding::NoteTodos { note } => note_todos(note).map(|open| Resolved {
            text: open.to_string(),
            tone: if open == 0 { Tone::Success } else { Tone::Neutral },
        }),
        Binding::PrChecks { session: id } => {
            let record = session(id)?;
            let pr = record.pull_requests.as_ref()?.first()?.clone();
            let (text, tone) = if pr.checks_failed > 0 {
                (format!("{} failing", pr.checks_failed), Tone::Danger)
            } else if pr.checks_pending > 0 {
                (format!("{} running", pr.checks_pending), Tone::Warning)
            } else if pr.checks_passed > 0 {
                ("Checks passing".to_owned(), Tone::Success)
            } else {
                (format!("#{}", pr.number), Tone::Muted)
            };
            Some(Resolved { text, tone })
        }
    }
}

/// "2m ago" for the header.
pub(crate) fn ago(updated_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(updated_ms) / 1000;
    match secs {
        0..=44 => crate::i18n::t("surfaces.just_now").to_owned(),
        45..=3599 => crate::i18n::tf("surfaces.minutes_ago", &[("count", &(secs / 60).max(1))]),
        3600..=86_399 => crate::i18n::tf("surfaces.hours_ago", &[("count", &(secs / 3600))]),
        _ => crate::i18n::tf("surfaces.days_ago", &[("count", &(secs / 86_400))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_badge_by_kind() {
        let look = link_look("https://github.com/acme/web/pull/812", None, None);
        assert_eq!((look.icon, look.badge, look.label.as_str()), ("arrow.triangle.pull", "PR", "acme/web#812"));
        let look = link_look("https://linear.app/a/issue/ENG-1/x", Some("Rollout"), None);
        assert_eq!((look.icon, look.label.as_str()), ("linear", "Rollout"));
        let look = link_look("diri://session/s1", None, None);
        assert!(look.internal);
        // A refused URL never classifies as anything it claims to be.
        let look = link_look("javascript:alert(1)", None, None);
        assert_eq!(look.badge, "Link");
    }

    #[test]
    fn tones_differ_between_strong_and_faint() {
        let colors = SemanticColors::dark();
        assert!(tone_fill(Tone::Success, colors).a < tone_color(Tone::Success, colors).a);
    }
}
