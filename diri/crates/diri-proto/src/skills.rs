//! Diri's agent skills: the detailed rules for a Diri capability, kept out of
//! the MCP server instructions, which Claude Code cuts at 2048 characters.
//!
//! Each body is plain Markdown in `skills/`. The Engine writes them into a
//! Claude Code plugin (`--plugin-dir`), where they load as native skills named
//! `diri:<name>`; every other agent reads them with the `get_skill` MCP tool.
//! The short instructions only say which skill covers what.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Stable id: the `get_skill` argument and the Claude skill folder name.
    pub name: &'static str,
    /// When to use it. Claude Code matches requests against this line.
    pub description: &'static str,
    pub markdown: &'static str,
}

impl Skill {
    /// `SKILL.md` for a Claude Code plugin: YAML frontmatter, then the body.
    pub fn skill_md(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}",
            self.name,
            yaml_string(self.description),
            self.markdown
        )
    }
}

fn yaml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

pub const SCHEDULING: Skill = Skill {
    name: "scheduling",
    description: "Inside Diri, use this for ANY request to run something later, at a set time, or repeatedly (every weekday at 9, tomorrow morning, in 2 hours), including waking a sleeping Mac for it. Use it instead of the built-in schedule skill, CronCreate, /loop, or reminders: those die with this session or run in the cloud without this Mac's checkout.",
    markdown: include_str!("../skills/scheduling.md"),
};

pub const NOTES: Skill = Skill {
    name: "notes",
    description: "Work started from a Diri note, or writing to Diri notes: read the brief, record findings and results, tick to-dos, never delete the person's writing.",
    markdown: include_str!("../skills/notes.md"),
};

pub const ORCHESTRATION: Skill = Skill {
    name: "orchestration",
    description: "Spawn and coordinate other Diri agents: parallel subtasks in worktrees, tracked tasks you assign or receive, waiting, retries, and reviewing their work.",
    markdown: include_str!("../skills/orchestration.md"),
};

pub const API: Skill = Skill {
    name: "api",
    description: "Building, running or debugging an HTTP API or dev server inside Diri: open the endpoint, prefilled, in the person's API tab with open_api_request (GET can auto-send; other methods wait for Send).",
    markdown: include_str!("../skills/api.md"),
};

pub const APP_SKILLS: Skill = Skill {
    name: "app-skills",
    description: "Doing a task in a Mac app with Diri's computer tool, especially one you'll repeat: check app_skills for a saved skill first; teach one (learn, trace, save), replay it with args or args_list, and repair it when a run breaks.",
    markdown: include_str!("../skills/app-skills.md"),
};

pub const ALL: [Skill; 5] = [SCHEDULING, NOTES, ORCHESTRATION, API, APP_SKILLS];

pub fn find(name: &str) -> Option<Skill> {
    let name = name.trim().trim_start_matches("diri:");
    ALL.into_iter().find(|skill| skill.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_folder_safe_and_findable() {
        for skill in ALL {
            assert!(
                skill
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{}",
                skill.name
            );
            assert_eq!(find(skill.name), Some(skill));
            assert_eq!(find(&format!("diri:{}", skill.name)), Some(skill));
            assert!(
                skill.markdown.starts_with("# "),
                "{} has a title",
                skill.name
            );
        }
        assert_eq!(find("nope"), None);
    }

    #[test]
    fn skill_md_has_parseable_frontmatter() {
        let md = SCHEDULING.skill_md();
        let mut parts = md.splitn(3, "---\n");
        assert_eq!(parts.next(), Some(""));
        let front = parts.next().unwrap();
        assert!(front.contains("name: scheduling\n"));
        assert!(front.contains("description: \"Inside Diri, use this"));
        assert!(
            parts
                .next()
                .unwrap()
                .trim_start()
                .starts_with("# Scheduling")
        );
    }
}
