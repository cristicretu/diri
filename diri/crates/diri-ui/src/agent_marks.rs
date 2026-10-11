//! Monochrome marks for Agents beyond the four hand-drawn brand marks.
//!
//! Each mark is one even-odd path in a 24×24 view box under
//! `assets/agents/`, keyed by the Agent's id (a manifest's, or the Engine's
//! recognition catalog's). `assets/agents/README.md` says where every one
//! came from and under which licence; `generate.py` there rebuilds them.
//! Adding an Agent is an SVG and a line here.
//!
//! Marks are parsed once, on first draw, and drawn like the brand marks: the
//! platform rasterizer caches each (mark, size, color), so a row costs an
//! image lookup per frame.

use std::sync::OnceLock;

use crate::SvgPath;

/// An Agent's mark: its id and its 24×24 path.
#[derive(Debug)]
pub struct AgentMark {
    id: &'static str,
    svg: &'static str,
    parsed: OnceLock<SvgPath>,
}

impl AgentMark {
    const fn new(id: &'static str, svg: &'static str) -> Self {
        Self {
            id,
            svg,
            parsed: OnceLock::new(),
        }
    }

    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Whether the mark is an official logo rather than a letter monogram.
    pub fn is_monogram(&self) -> bool {
        self.svg.contains("<!-- monogram")
    }

    pub(crate) fn parsed(&self) -> &SvgPath {
        self.parsed.get_or_init(|| {
            SvgPath::parse(path_data(self.svg)).expect("bundled agent mark SVG must be valid")
        })
    }
}

// Marks are identified by their id: one mark per Agent.
impl PartialEq for AgentMark {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AgentMark {}

impl std::hash::Hash for AgentMark {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

macro_rules! marks {
    ($($id:literal),* $(,)?) => {
        /// Every bundled Agent mark, by id.
        pub static AGENT_MARKS: [AgentMark; [$($id),*].len()] = [
            $(AgentMark::new($id, include_str!(concat!("../assets/agents/", $id, ".svg")))),*
        ];
    };
}

marks![
    "abacusai",
    "aider",
    "amp",
    "antigravity",
    "auggie",
    "cline",
    "codebuddy",
    "codebuff",
    "coderabbit",
    "command-code",
    "copilot",
    "crush",
    "devin",
    "droid",
    "goose",
    "grok",
    "hermes",
    "junie",
    "kilo",
    "kimi",
    "kiro",
    "maki",
    "mcode",
    "muse",
    "openclaw",
    "opencode",
    "openhands",
    "pi",
    "qoder",
    "qwen",
    "traecli",
    "vibe",
    "warp",
    "whipcode",
];

/// The mark for the Agent `id`, if one is bundled.
pub fn agent_mark(id: &str) -> Option<&'static AgentMark> {
    AGENT_MARKS.iter().find(|mark| mark.id == id)
}

/// The `d` of the one `<path>` in a generated mark.
fn path_data(svg: &str) -> &str {
    let start = svg.find(" d=\"").expect("agent mark has a path") + 4;
    let end = svg[start..]
        .find('"')
        .expect("agent mark path is terminated");
    &svg[start..start + end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PathCommand;

    #[test]
    fn every_mark_parses_inside_its_view_box() {
        for mark in &AGENT_MARKS {
            let commands = mark.parsed().commands();
            assert!(commands.len() > 3, "{}", mark.id);
            // The ink's bounds, sampling curves rather than trusting their
            // control points.
            let (mut min, mut max) = ((f32::MAX, f32::MAX), (f32::MIN, f32::MIN));
            let mut include = |point: (f32, f32)| {
                min = (min.0.min(point.0), min.1.min(point.1));
                max = (max.0.max(point.0), max.1.max(point.1));
            };
            let mut current = (0.0, 0.0);
            for command in commands {
                match *command {
                    PathCommand::MoveTo(x, y) | PathCommand::LineTo(x, y) => current = (x, y),
                    PathCommand::QuadTo { control, to } => {
                        for step in 1..16 {
                            let t = step as f32 / 16.0;
                            let u = 1.0 - t;
                            include((
                                u * u * current.0 + 2.0 * u * t * control.0 + t * t * to.0,
                                u * u * current.1 + 2.0 * u * t * control.1 + t * t * to.1,
                            ));
                        }
                        current = to;
                    }
                    PathCommand::CubicTo {
                        control_a: a,
                        control_b: b,
                        to,
                    } => {
                        for step in 1..16 {
                            let t = step as f32 / 16.0;
                            let u = 1.0 - t;
                            let (w0, w1, w2, w3) =
                                (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                            include((
                                w0 * current.0 + w1 * a.0 + w2 * b.0 + w3 * to.0,
                                w0 * current.1 + w1 * a.1 + w2 * b.1 + w3 * to.1,
                            ));
                        }
                        current = to;
                    }
                    PathCommand::Close => continue,
                }
                include(current);
            }
            assert!(
                min.0 >= -0.01 && min.1 >= -0.01 && max.0 <= 24.01 && max.1 <= 24.01,
                "{} leaves the 24 box: {min:?}..{max:?}",
                mark.id
            );
            // Centred, so every mark shares the brand marks' optical axis.
            let centre = ((min.0 + max.0) / 2.0, (min.1 + max.1) / 2.0);
            assert!(
                (centre.0 - 12.0).abs() < 0.3 && (centre.1 - 12.0).abs() < 0.3,
                "{} is off centre: {centre:?}",
                mark.id
            );
        }
    }

    #[test]
    fn ids_are_unique_and_found() {
        let mut ids = std::collections::HashSet::new();
        for mark in &AGENT_MARKS {
            assert!(ids.insert(mark.id), "duplicate {}", mark.id);
            assert_eq!(agent_mark(mark.id), Some(mark));
        }
        assert_eq!(agent_mark("claude-code"), None, "a brand mark, not here");
        assert_eq!(agent_mark("shell"), None);
    }

    #[test]
    fn every_mark_has_a_source_note() {
        let readme = include_str!("../assets/agents/README.md");
        for mark in &AGENT_MARKS {
            assert!(
                readme.contains(&format!("| `{}` |", mark.id)),
                "{} has no row in assets/agents/README.md",
                mark.id
            );
        }
    }
}
