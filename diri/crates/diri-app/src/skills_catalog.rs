//! Bounded, read-only discovery of local SKILL.md files. This catalogue never
//! installs, activates, or executes a skill; provider configuration owns that.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 128 * 1024;
const MAX_ENTRIES: usize = 2_000;
const MAX_DIRECTORIES: usize = 10_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Scope {
    #[default]
    All,
    Personal,
    Project,
    Plugins,
}

impl Scope {
    pub const ALL: [Self; 4] = [Self::All, Self::Personal, Self::Project, Self::Plugins];

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Personal => "Personal",
            Self::Project => "Project",
            Self::Plugins => "Plugins",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub source: String,
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub sources: BTreeSet<String>,
    pub scopes: Vec<Scope>,
}

impl Skill {
    pub fn matches(&self, query: &str, scope: Scope) -> bool {
        if scope != Scope::All && !self.scopes.contains(&scope) {
            return false;
        }
        let haystack = format!(
            "{} {} {}",
            self.name,
            self.description,
            self.sources.iter().cloned().collect::<Vec<_>>().join(" ")
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| haystack.contains(&word.to_lowercase()))
    }

    pub fn source_label(&self) -> String {
        self.sources.iter().cloned().collect::<Vec<_>>().join(" · ")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub skills: Vec<Skill>,
    pub unreadable: usize,
    pub limited: bool,
}

pub fn roots(home: &Path, project: Option<&Path>) -> Vec<SkillRoot> {
    let providers = [
        (".agents/skills", "Shared"),
        (".claude/skills", "Claude"),
        (".codex/skills", "Codex"),
        (".cursor/skills", "Cursor"),
        (".gemini/skills", "Gemini"),
        (".opencode/skills", "OpenCode"),
        (".pi/agent/skills", "Pi"),
    ];
    let mut roots = Vec::new();
    for (relative, source) in providers {
        roots.push(SkillRoot {
            path: home.join(relative),
            source: source.into(),
            scope: Scope::Personal,
        });
        if let Some(project) = project {
            roots.push(SkillRoot {
                path: project.join(relative),
                source: format!("Project · {source}"),
                scope: Scope::Project,
            });
        }
    }
    roots.push(SkillRoot {
        path: home.join(".config/opencode/skills"),
        source: "OpenCode".into(),
        scope: Scope::Personal,
    });
    for (variable, source) in [("CODEX_HOME", "Codex"), ("CLAUDE_CONFIG_DIR", "Claude")] {
        if let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) {
            roots.push(SkillRoot {
                path: PathBuf::from(path).join("skills"),
                source: source.into(),
                scope: Scope::Personal,
            });
        }
    }
    // Inspect only the known cache layout: publisher/plugin/version/skills.
    // Never walk node_modules, plugin scripts, repositories, or skill resources.
    // Cached versions are identified as such, not claimed to be enabled.
    for provider in [".codex", ".claude"] {
        for publisher in directories(&home.join(provider).join("plugins/cache"), 128) {
            for plugin in directories(&publisher, 128) {
                for version in directories(&plugin, 32) {
                    roots.push(SkillRoot {
                        path: version.join("skills"),
                        source: format!(
                            "{} plugin cache · {} · {}",
                            provider.trim_start_matches('.'),
                            basename(&plugin),
                            basename(&version)
                        ),
                        scope: Scope::Plugins,
                    });
                }
            }
        }
    }
    roots
}

fn basename(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn directories(path: &Path, limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .take(limit)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

pub fn scan(roots: &[SkillRoot]) -> Catalog {
    let mut catalog = Catalog::default();
    let mut skills = BTreeMap::<PathBuf, Skill>::new();
    let mut remaining = MAX_DIRECTORIES;
    for root in roots {
        let Ok(canonical_root) = fs::canonicalize(&root.path) else {
            continue;
        };
        let mut visited = HashSet::new();
        let mut pending = vec![(root.path.clone(), 0)];
        while let Some((directory, depth)) = pending.pop() {
            if remaining == 0 || skills.len() >= MAX_ENTRIES {
                catalog.limited = true;
                break;
            }
            remaining -= 1;
            let canonical = match fs::canonicalize(&directory) {
                Ok(path) => path,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        catalog.unreadable += 1;
                    }
                    continue;
                }
            };
            if !visited.insert(canonical.clone()) {
                continue;
            }
            let path = canonical.join("SKILL.md");
            match read(&path) {
                Ok(text) => {
                    let (name, description) = metadata(&text, &basename(&canonical));
                    let skill = skills.entry(path.clone()).or_insert_with(|| Skill {
                        name,
                        description,
                        path,
                        sources: BTreeSet::new(),
                        scopes: Vec::new(),
                    });
                    skill.sources.insert(root.source.clone());
                    if !skill.scopes.contains(&root.scope) {
                        skill.scopes.push(root.scope);
                    }
                    // A skill's supporting directories are not other skills.
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    catalog.unreadable += 1;
                    continue;
                }
            }
            // Linked individual skills are common installations. Read their
            // SKILL.md above, but never traverse an external linked directory.
            if !canonical.starts_with(&canonical_root) {
                continue;
            }
            if depth >= 5 {
                catalog.limited = true;
                continue;
            }
            match fs::read_dir(&canonical) {
                Ok(entries) => {
                    let mut children = Vec::new();
                    for entry in entries.take(remaining.min(MAX_ENTRIES)) {
                        let Ok(entry) = entry else {
                            catalog.unreadable += 1;
                            continue;
                        };
                        let name = entry.file_name();
                        if matches!(
                            name.to_str(),
                            Some("node_modules" | ".git" | "references" | "scripts" | "assets")
                        ) {
                            continue;
                        }
                        if entry
                            .file_type()
                            .is_ok_and(|kind| kind.is_dir() || kind.is_symlink())
                        {
                            children.push((entry.path(), depth + 1));
                        }
                    }
                    children.sort();
                    pending.extend(children.into_iter().rev());
                }
                Err(_) => catalog.unreadable += 1,
            }
        }
    }
    catalog.skills = skills.into_values().collect();
    catalog.skills.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    catalog
}

pub fn read(path: &Path) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file: File = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Skill must be a regular UTF-8 file smaller than 128 KiB",
        ));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other("Skill is too large"));
    }
    Ok(text)
}

/// Extract display metadata without interpreting YAML tags or executing any
/// skill instructions. Plain, quoted and folded/literal descriptions are
/// supported; missing metadata falls back to the directory name.
fn metadata(text: &str, fallback: &str) -> (String, String) {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim) != Some("---") {
        return (fallback.into(), String::new());
    }
    let mut name = fallback.to_owned();
    let mut description = String::new();
    let mut field = "";
    for line in lines {
        if matches!(line.trim(), "---" | "...") {
            break;
        }
        if line.starts_with(char::is_whitespace) && field == "description" {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str(line.trim());
            continue;
        }
        field = "";
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "name" if !value.is_empty() => name = scalar(value),
            "description" => {
                field = "description";
                description = if matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
                    String::new()
                } else {
                    scalar(value)
                };
            }
            _ => {}
        }
    }
    let description = scalar(&description);
    (
        name.chars().filter(|c| !c.is_control()).take(160).collect(),
        description
            .chars()
            .filter(|c| !c.is_control())
            .take(2_000)
            .collect(),
    )
}

fn scalar(value: &str) -> String {
    if value.starts_with('"')
        && let Ok(value) = serde_json::from_str::<String>(value)
    {
        return value;
    }
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return value.replace("''", "'");
    }
    value.to_owned()
}

pub fn body(text: &str) -> &str {
    let text = text.trim_start_matches('\u{feff}');
    if text.lines().next().map(str::trim) != Some("---") {
        return text;
    }
    let mut offset = text.find('\n').map_or(text.len(), |index| index + 1);
    for line in text[offset..].split_inclusive('\n') {
        offset += line.len();
        if matches!(line.trim(), "---" | "...") {
            return &text[offset..];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_descriptions_quotes_and_body_remain_readable() {
        let text = "---\r\nname: 'User''s skill'\r\ndescription: >-\r\n  Find useful files\r\n  across a project.\r\n---\r\n# Instructions\r\n";
        assert_eq!(
            metadata(text, "fallback"),
            (
                "User's skill".into(),
                "Find useful files across a project.".into()
            )
        );
        assert_eq!(body(text), "# Instructions\r\n");
    }

    #[test]
    fn scan_merges_shared_sources_and_keeps_same_name_different_files() {
        let temp = tempfile::tempdir().unwrap();
        for folder in ["shared/one", "project/one"] {
            fs::create_dir_all(temp.path().join(folder)).unwrap();
            fs::write(
                temp.path().join(folder).join("SKILL.md"),
                "---\nname: design\ndescription: interface polish\n---\nHello",
            )
            .unwrap();
        }
        let roots = [
            SkillRoot {
                path: temp.path().join("shared"),
                source: "Claude".into(),
                scope: Scope::Personal,
            },
            SkillRoot {
                path: temp.path().join("shared"),
                source: "Codex".into(),
                scope: Scope::Personal,
            },
            SkillRoot {
                path: temp.path().join("project"),
                source: "Project".into(),
                scope: Scope::Project,
            },
        ];
        let catalog = scan(&roots);
        assert_eq!(catalog.skills.len(), 2);
        assert!(catalog.skills.iter().any(|skill| skill.sources.len() == 2));
        assert_eq!(
            catalog
                .skills
                .iter()
                .filter(|skill| skill.matches("design polish", Scope::Project))
                .count(),
            1
        );
    }

    #[test]
    fn scan_bounds_files_and_does_not_walk_supporting_resources() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("one/references/nested")).unwrap();
        fs::write(temp.path().join("one/SKILL.md"), "# My skill").unwrap();
        fs::write(
            temp.path().join("one/references/nested/SKILL.md"),
            "Not a skill",
        )
        .unwrap();
        fs::create_dir_all(temp.path().join("large")).unwrap();
        File::create(temp.path().join("large/SKILL.md"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let catalog = scan(&[SkillRoot {
            path: temp.path().into(),
            source: "Test".into(),
            scope: Scope::Personal,
        }]);
        assert_eq!(catalog.skills.len(), 1);
        assert_eq!(catalog.unreadable, 1);
    }

    #[cfg(unix)]
    #[test]
    fn linked_skills_are_read_without_traversing_external_trees() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        let external = temp.path().join("external");
        fs::create_dir_all(external.join("nested")).unwrap();
        fs::write(external.join("nested/SKILL.md"), "# Shared skill").unwrap();
        symlink(&external, root.join("escape")).unwrap();
        let roots = [SkillRoot {
            path: root.clone(),
            source: "Test".into(),
            scope: Scope::Personal,
        }];
        assert!(scan(&roots).skills.is_empty());
        symlink(external.join("nested"), root.join("installed")).unwrap();
        assert_eq!(scan(&roots).skills.len(), 1);
    }
}
