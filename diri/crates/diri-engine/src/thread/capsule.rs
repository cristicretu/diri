//! The Handoff Capsule: what a second Agent needs to continue a thread.
//!
//! Tier 3 handoff (Claude → Codex → Claude) starts the next Agent from a
//! capsule instead of the first Agent's transcript. The capsule is a pure,
//! deterministic function of its inputs: the same thread, goal and git state
//! always render the same bytes, so the receiving Agent's first request is
//! cacheable and reviewable, and the parked Agent's own conversation is
//! never touched (`PromptCache.parked_prefix_preserved`).

use std::collections::BTreeSet;

use diri_proto::thread::{ThreadBody, ThreadItem, ToolDetail};

use super::normalize::bounded;

/// Recent conversation turns carried over, newest last.
const RECENT_TURNS: usize = 6;
const TURN_BYTES: usize = 2 * 1024;
const OUTCOMES: usize = 12;
const DIFF_BYTES: usize = 24 * 1024;
const MAX_FILES: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapsuleInputs<'a> {
    /// A pinned goal or note the user wants the next Agent to pursue.
    pub goal: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// `git diff --stat` and the diff itself, already bounded by the caller.
    pub diff_stat: &'a str,
    pub diff: &'a str,
    /// The Agent the thread is moving from, for the header.
    pub from_agent: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffCapsule {
    pub goal: Option<String>,
    pub branch: Option<String>,
    pub from_agent: String,
    pub touched_files: Vec<String>,
    pub recent: Vec<(&'static str, String)>,
    pub outcomes: Vec<String>,
    pub diff_stat: String,
    pub diff: String,
}

pub fn build(items: &[ThreadItem], inputs: &CapsuleInputs<'_>) -> HandoffCapsule {
    let mut touched = BTreeSet::new();
    let mut recent = Vec::new();
    let mut calls: Vec<(&str, String)> = Vec::new();
    let mut outcomes = Vec::new();
    for item in items {
        match &item.body {
            ThreadBody::User { text, .. } if !text.trim().is_empty() => {
                recent.push(("User", bounded(text.trim(), TURN_BYTES)));
            }
            ThreadBody::Assistant { text } if !text.trim().is_empty() => {
                recent.push(("Assistant", bounded(text.trim(), TURN_BYTES)));
            }
            ThreadBody::ToolCall {
                call_id, detail, ..
            } => {
                match detail {
                    ToolDetail::Edit { path, .. } | ToolDetail::Write { path, .. } => {
                        touched.insert(path.clone());
                    }
                    ToolDetail::Patch { patch } => {
                        touched.extend(patch_paths(patch));
                    }
                    _ => {}
                }
                if let ToolDetail::Command { command } = detail {
                    calls.push((call_id.as_str(), first_line(command)));
                }
            }
            ThreadBody::ToolResult {
                call_id, is_error, ..
            } => {
                if let Some((_, command)) = calls.iter().find(|(id, _)| id == call_id) {
                    outcomes.push(format!(
                        "{} `{}`",
                        if *is_error { "✗" } else { "✓" },
                        command
                    ));
                }
            }
            _ => {}
        }
    }
    let skip = recent.len().saturating_sub(RECENT_TURNS);
    let skip_outcomes = outcomes.len().saturating_sub(OUTCOMES);
    HandoffCapsule {
        goal: inputs.goal.map(|goal| bounded(goal.trim(), TURN_BYTES)),
        branch: inputs.branch.map(str::to_owned),
        from_agent: inputs.from_agent.to_owned(),
        touched_files: touched.into_iter().take(MAX_FILES).collect(),
        recent: recent.split_off(skip),
        outcomes: outcomes.split_off(skip_outcomes),
        diff_stat: bounded(inputs.diff_stat.trim_end(), DIFF_BYTES),
        diff: bounded(inputs.diff.trim_end(), DIFF_BYTES),
    }
}

impl HandoffCapsule {
    /// Markdown for the receiving Agent's first prompt. Fixed section order,
    /// no timestamps, no session identities: stable bytes in, stable bytes out.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "You are continuing work another coding agent ({}) started in this repository. \
             Read this handoff, check the working tree, then continue.\n",
            self.from_agent
        ));
        if let Some(goal) = &self.goal {
            out.push_str("\n## Goal\n\n");
            out.push_str(goal);
            out.push('\n');
        }
        if let Some(branch) = &self.branch {
            out.push_str(&format!("\n## Branch\n\n`{branch}`\n"));
        }
        if !self.touched_files.is_empty() {
            out.push_str("\n## Files touched\n\n");
            for file in &self.touched_files {
                out.push_str(&format!("- `{file}`\n"));
            }
        }
        if !self.outcomes.is_empty() {
            out.push_str("\n## Recent checks\n\n");
            for outcome in &self.outcomes {
                out.push_str(&format!("- {outcome}\n"));
            }
        }
        if !self.recent.is_empty() {
            out.push_str("\n## Recent conversation\n");
            for (role, text) in &self.recent {
                out.push_str(&format!("\n**{role}:** {text}\n"));
            }
        }
        if !self.diff_stat.is_empty() {
            out.push_str("\n## Uncommitted changes\n\n```\n");
            out.push_str(&self.diff_stat);
            out.push_str("\n```\n");
        }
        if !self.diff.is_empty() {
            out.push_str("\n```diff\n");
            out.push_str(&self.diff);
            out.push_str("\n```\n");
        }
        out
    }
}

/// `git diff --stat`-style lines for a unified patch, in patch order.
pub fn diff_stat(patch: &str) -> String {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            let path = rest.split(" b/").next().unwrap_or(rest).to_owned();
            files.push((path, 0, 0));
        } else if let Some(last) = files.last_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                last.1 += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                last.2 += 1;
            }
        }
    }
    files
        .iter()
        .map(|(path, added, removed)| format!(" {path} | +{added} -{removed}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    bounded(line, 160)
}

/// Paths named by a Codex `apply_patch` body or a unified diff.
fn patch_paths(patch: &str) -> impl Iterator<Item = String> + '_ {
    patch.lines().filter_map(|line| {
        line.strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("+++ b/"))
            .map(|path| path.trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::thread::{EditHunk, ItemKey};

    fn item(offset: u64, body: ThreadBody) -> ThreadItem {
        ThreadItem {
            key: ItemKey { offset, part: 0 },
            at_ms: Some(offset as i64),
            body,
        }
    }

    fn thread() -> Vec<ThreadItem> {
        vec![
            item(
                0,
                ThreadBody::User {
                    text: "Make the parser streaming".into(),
                    images: vec![],
                },
            ),
            item(
                1,
                ThreadBody::ToolCall {
                    call_id: "e".into(),
                    name: "Edit".into(),
                    detail: ToolDetail::Edit {
                        path: "src/z.rs".into(),
                        hunks: vec![EditHunk {
                            old: "a".into(),
                            new: "b".into(),
                        }],
                    },
                },
            ),
            item(
                2,
                ThreadBody::ToolCall {
                    call_id: "p".into(),
                    name: "apply_patch".into(),
                    detail: ToolDetail::Patch {
                        patch:
                            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch"
                                .into(),
                    },
                },
            ),
            item(
                3,
                ThreadBody::ToolCall {
                    call_id: "t".into(),
                    name: "Bash".into(),
                    detail: ToolDetail::Command {
                        command: "cargo test -p parser\necho done".into(),
                    },
                },
            ),
            item(
                4,
                ThreadBody::ToolResult {
                    call_id: "t".into(),
                    is_error: true,
                    preview: "1 failed".into(),
                    images: vec![],
                },
            ),
            item(
                5,
                ThreadBody::Assistant {
                    text: "One test still fails.".into(),
                },
            ),
        ]
    }

    #[test]
    fn capsule_is_deterministic_and_sorted() {
        let inputs = CapsuleInputs {
            goal: Some("Ship streaming parse"),
            branch: Some("feat/stream"),
            diff_stat: " src/a.rs | 2 +-\n",
            diff: "",
            from_agent: "Claude Code",
        };
        let first = build(&thread(), &inputs);
        assert_eq!(first.touched_files, vec!["src/a.rs", "src/z.rs"]);
        assert_eq!(first.outcomes, vec!["✗ `cargo test -p parser`"]);
        let rendered = first.render();
        assert_eq!(rendered, build(&thread(), &inputs).render());
        let order = [
            "## Goal",
            "## Branch",
            "## Files touched",
            "## Recent checks",
            "## Recent conversation",
            "## Uncommitted changes",
        ];
        let positions: Vec<_> = order
            .iter()
            .map(|heading| rendered.find(heading).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!rendered.contains("toolu_"), "no provider identities");
    }

    #[test]
    fn diff_stat_counts_lines_per_file() {
        let patch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@\n-x\n+y\n+z\ndiff --git a/b.md b/b.md\n+++ b/b.md\n+new\n";
        assert_eq!(diff_stat(patch), " src/a.rs | +2 -1\n b.md | +1 -0");
    }

    #[test]
    fn only_the_latest_turns_are_carried() {
        let items: Vec<_> = (0..20)
            .map(|n| {
                item(
                    n,
                    ThreadBody::Assistant {
                        text: format!("turn {n}"),
                    },
                )
            })
            .collect();
        let capsule = build(
            &items,
            &CapsuleInputs {
                from_agent: "Codex",
                ..CapsuleInputs::default()
            },
        );
        assert_eq!(capsule.recent.len(), RECENT_TURNS);
        assert_eq!(capsule.recent.last().unwrap().1, "turn 19");
    }
}
