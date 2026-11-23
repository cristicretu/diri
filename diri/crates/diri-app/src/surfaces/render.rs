//! The pieces of a surface's look that need no GPUI context: tone colours,
//! link badges, and live bindings resolved against the session store. Kept
//! apart from the view so they are unit-tested without a window.

use diri_proto::surface::link::{self, LinkKind};
use diri_proto::surface::node::Color;
use diri_proto::surface::{Binding, Icon, Tone};
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

/// How a link presents itself: the agent's glyph (or a generic one for
/// its kind), the agent's badge word, and a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkLook {
    pub icon: Option<Icon>,
    pub default_symbol: &'static str,
    pub badge: Option<String>,
    pub label: String,
    pub internal: bool,
}

pub(crate) fn link_look(
    url: &str,
    label: Option<&str>,
    icon: Option<&Icon>,
    badge: Option<&str>,
    store: Option<&SessionStore>,
) -> LinkLook {
    let kind = if link::is_allowed(url) {
        link::classify(url)
    } else {
        LinkKind::Web {
            host: String::new(),
        }
    };
    let default_symbol = match &kind {
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
        icon: icon.cloned(),
        default_symbol,
        badge: badge.map(str::to_owned).filter(|b| !b.trim().is_empty()),
        label: label
            .map(str::to_owned)
            .filter(|label| !label.trim().is_empty())
            .or(live)
            .unwrap_or_else(|| kind.default_label()),
        internal: kind.is_internal(),
    }
}

/// `"🎧"` is an emoji, `"chart.bar"` a built-in icon, `"icon.svg"` an
/// image in the app's folder.
pub(crate) fn icon_shorthand(text: &str) -> Icon {
    let lower = text.to_ascii_lowercase();
    if diri_proto::surface::identity::IMAGE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
    {
        Icon::Asset(text.to_owned())
    } else if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.')
    {
        Icon::Symbol(text.to_owned())
    } else {
        Icon::Emoji(text.to_owned())
    }
}

/// An agent's accent, parsed; Diri's clay when it set none.
pub(crate) fn accent_of(accent: Option<&str>) -> Rgba {
    accent
        .and_then(diri_proto::surface::identity::parse_accent)
        .map_or(crate::notes::editor_view::accent(), |(r, g, b)| Rgba {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: 1.0,
        })
}

/// A component color on this palette: a theme token (following light and
/// dark), or the agent's own hex.
pub(crate) fn color(color: &Color, colors: SemanticColors, accent: Rgba) -> Rgba {
    if let Some((r, g, b, a)) = color.hex() {
        return Rgba {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        };
    }
    let faint = |a: f32| Rgba {
        a: colors.primary.a * a,
        ..colors.primary
    };
    match color.0.as_str() {
        "primary" => colors.primary,
        "secondary" => colors.secondary,
        "tertiary" => colors.tertiary,
        "accent" => accent,
        "success" => Ink::FRESH,
        "warning" => Ink::ATTENTION,
        "danger" => Ink::DANGER,
        "info" => Palette::GEMINI_BLUE,
        "background" => colors.background,
        "surface" => faint(0.035),
        "fill" => faint(0.07),
        "border" => faint(0.09),
        _ => colors.primary,
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
    fn links_look_as_the_agent_says() {
        let look = link_look("https://github.com/acme/web/pull/812", None, None, None, None);
        assert_eq!((look.default_symbol, look.badge.as_deref(), look.label.as_str()), ("link", None, "github.com"));
        let icon = Icon::Symbol("arrow.triangle.pull".into());
        let look = link_look("https://x.dev/1", Some("Retry PR"), Some(&icon), Some("PR"), None);
        assert_eq!((look.icon.as_ref(), look.badge.as_deref(), look.label.as_str()), (Some(&icon), Some("PR"), "Retry PR"));
        assert!(link_look("diri://session/s1", None, None, None, None).internal);
        assert_eq!(link_look("javascript:alert(1)", None, None, None, None).default_symbol, "link");
    }

    #[test]
    fn icons_and_colors_parse() {
        assert_eq!(icon_shorthand("🎧"), Icon::Emoji("🎧".into()));
        assert_eq!(icon_shorthand("chart.bar"), Icon::Symbol("chart.bar".into()));
        assert_eq!(icon_shorthand("art/icon.svg"), Icon::Asset("art/icon.svg".into()));
        let colors = SemanticColors::dark();
        let accent = accent_of(Some("#ff0000"));
        assert_eq!(color(&Color("accent".into()), colors, accent), accent);
        assert_eq!(color(&Color("#00ff0080".into()), colors, accent).a, 128.0 / 255.0);
    }

    #[test]
    fn tones_differ_between_strong_and_faint() {
        let colors = SemanticColors::dark();
        assert!(tone_fill(Tone::Success, colors).a < tone_color(Tone::Success, colors).a);
    }
}
