//! Versioned, locally persisted launch recipes.
//!
//! The launcher is a view over this module. Recipes resolve to the same
//! `SpawnOptions` consumed by ordinary launches, so availability, host, and
//! worktree policy cannot drift into a second spawning implementation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use diri_proto::{AgentKind, HostEntry, Project, ProjectId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::agent_catalog;
use crate::store::{SpawnOptions, WorktreeSpawn};

const CURRENT_VERSION: u32 = 1;
pub const MAX_RECIPES: usize = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRecipe {
    pub id: String,
    pub name: String,
    pub agent: AgentKind,
    pub project: RecipeProject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    pub worktree: WorktreePolicy,
    pub initial_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RecipeProject {
    /// A daemon-owned project identity. The latest root wins when the project
    /// moves, while `lastKnownRoot` keeps repair UI useful if it disappears.
    Tracked {
        id: ProjectId,
        last_known_root: String,
    },
    /// An explicit path chosen before Diri has seen a session in that project.
    Path { path: String },
}

impl RecipeProject {
    pub fn display_path(&self) -> &str {
        match self {
            Self::Tracked {
                last_known_root, ..
            } => last_known_root,
            Self::Path { path } => path,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorktreePolicy {
    #[default]
    CurrentCheckout,
    Fresh {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaunchRecipeBook {
    version: u32,
    next_id: u64,
    items: Vec<LaunchRecipe>,
    /// A book written by a newer Diri is kept byte-for-byte semantically
    /// intact. This build exposes no entries and refuses mutations instead of
    /// silently rewriting an unknown schema as version 1.
    unsupported: Option<serde_json::Value>,
}

impl Default for LaunchRecipeBook {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            next_id: 1,
            items: Vec::new(),
            unsupported: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeBookError {
    Full,
    Missing,
    UnsupportedVersion,
}

impl std::fmt::Display for RecipeBookError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Full => "Recipe library is full (64 maximum)",
            Self::Missing => "This recipe no longer exists",
            Self::UnsupportedVersion => {
                "Recipes were created by a newer Diri and cannot be edited here"
            }
        })
    }
}

impl std::error::Error for RecipeBookError {}

#[derive(Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct RecipeBookV1 {
    version: u32,
    next_id: u64,
    items: Vec<LaunchRecipe>,
}

impl Default for RecipeBookV1 {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            next_id: 1,
            items: Vec::new(),
        }
    }
}

impl Serialize for LaunchRecipeBook {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(value) = &self.unsupported {
            return value.serialize(serializer);
        }
        RecipeBookV1 {
            version: CURRENT_VERSION,
            next_id: self.next_id,
            items: self.items.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LaunchRecipeBook {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_json(value))
    }
}

impl LaunchRecipeBook {
    pub fn items(&self) -> &[LaunchRecipe] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&LaunchRecipe> {
        self.items.iter().find(|recipe| recipe.id == id)
    }

    pub fn add(&mut self, mut recipe: LaunchRecipe) -> Result<&LaunchRecipe, RecipeBookError> {
        self.ensure_mutable()?;
        if self.items.len() >= MAX_RECIPES {
            return Err(RecipeBookError::Full);
        }
        recipe.id = self.allocate_id();
        recipe.normalize();
        self.items.push(recipe);
        Ok(self.items.last().expect("recipe was just inserted"))
    }

    pub fn replace(&mut self, id: &str, mut recipe: LaunchRecipe) -> Result<(), RecipeBookError> {
        self.ensure_mutable()?;
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return Err(RecipeBookError::Missing);
        };
        recipe.id = id.to_owned();
        recipe.normalize();
        self.items[index] = recipe;
        Ok(())
    }

    pub fn rename(&mut self, id: &str, name: impl Into<String>) -> Result<(), RecipeBookError> {
        self.ensure_mutable()?;
        let Some(recipe) = self.items.iter_mut().find(|item| item.id == id) else {
            return Err(RecipeBookError::Missing);
        };
        let name = name.into();
        let name = name.trim();
        if name.is_empty() {
            return Err(RecipeBookError::Missing);
        }
        recipe.name = name.chars().take(80).collect();
        Ok(())
    }

    pub fn duplicate(&mut self, id: &str) -> Result<&LaunchRecipe, RecipeBookError> {
        self.ensure_mutable()?;
        let mut duplicate = self.get(id).ok_or(RecipeBookError::Missing)?.clone();
        duplicate.name = unique_copy_name(&duplicate.name, &self.items);
        self.add(duplicate)
    }

    pub fn move_by(&mut self, id: &str, delta: isize) -> Result<bool, RecipeBookError> {
        self.ensure_mutable()?;
        let Some(from) = self.items.iter().position(|recipe| recipe.id == id) else {
            return Err(RecipeBookError::Missing);
        };
        let to =
            (from as isize + delta).clamp(0, self.items.len().saturating_sub(1) as isize) as usize;
        if from == to {
            return Ok(false);
        }
        let recipe = self.items.remove(from);
        self.items.insert(to, recipe);
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> Result<LaunchRecipe, RecipeBookError> {
        self.ensure_mutable()?;
        let index = self
            .items
            .iter()
            .position(|recipe| recipe.id == id)
            .ok_or(RecipeBookError::Missing)?;
        Ok(self.items.remove(index))
    }

    pub fn normalize(&mut self) {
        if self.unsupported.is_some() {
            return;
        }
        self.version = CURRENT_VERSION;
        self.next_id = self.next_id.max(1);
        self.items.truncate(MAX_RECIPES);

        let mut ids = HashSet::new();
        let mut next_id = self.next_id;
        for recipe in &mut self.items {
            recipe.normalize();
            if recipe.id.is_empty() || !ids.insert(recipe.id.clone()) {
                let (replacement, following) = first_free_id(&ids, next_id);
                ids.insert(replacement.clone());
                recipe.id = replacement;
                next_id = following;
            }
        }
        self.next_id = next_id.max(1);
    }

    fn allocate_id(&mut self) -> String {
        let ids = self
            .items
            .iter()
            .map(|recipe| recipe.id.clone())
            .collect::<HashSet<_>>();
        let (id, following) = first_free_id(&ids, self.next_id);
        self.next_id = following;
        id
    }

    fn ensure_mutable(&self) -> Result<(), RecipeBookError> {
        if self.unsupported.is_some() {
            Err(RecipeBookError::UnsupportedVersion)
        } else {
            Ok(())
        }
    }

    fn from_json(value: serde_json::Value) -> Self {
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(CURRENT_VERSION);
        if version != CURRENT_VERSION {
            return Self {
                version,
                next_id: 1,
                items: Vec::new(),
                unsupported: Some(value),
            };
        }
        let next_id = value
            .get("nextId")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        let items = value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect();
        let mut book = Self {
            version: CURRENT_VERSION,
            next_id,
            items,
            unsupported: None,
        };
        book.normalize();
        book
    }
}

fn first_free_id(ids: &HashSet<String>, preferred: u64) -> (String, u64) {
    // A valid book contains at most 64 entries, so this finite candidate set
    // always contains a free id even for adversarial `u64::MAX` input.
    std::iter::once(preferred.max(1))
        .chain(1..=u64::try_from(MAX_RECIPES + 1).unwrap_or(65))
        .find_map(|candidate| {
            let id = format!("recipe-{candidate}");
            (!ids.contains(&id)).then(|| (id, candidate.wrapping_add(1).max(1)))
        })
        .expect("65 candidates cannot all be occupied by a 64-entry book")
}

impl LaunchRecipe {
    pub fn draft(
        name: impl Into<String>,
        agent: AgentKind,
        project: RecipeProject,
        host: Option<String>,
        initial_prompt: impl Into<String>,
    ) -> Self {
        let mut recipe = Self {
            id: String::new(),
            name: name.into(),
            agent,
            project,
            host,
            worktree: WorktreePolicy::CurrentCheckout,
            initial_prompt: initial_prompt.into(),
            title: None,
        };
        recipe.normalize();
        recipe
    }

    pub fn resolve(
        &self,
        projects: &HashMap<ProjectId, Project>,
        hosts: &[HostEntry],
        catalog: Option<&diri_proto::AgentReadinessResult>,
        effective_host: impl Fn(&Project) -> Option<String>,
    ) -> Result<ResolvedRecipe, RecipeIssue> {
        if self.initial_prompt.trim().is_empty() {
            return Err(RecipeIssue::EmptyPrompt);
        }
        if let Some(host) = self.host.as_deref()
            && !hosts.iter().any(|candidate| candidate.id == host)
        {
            return Err(RecipeIssue::MissingHost(host.to_owned()));
        }
        if catalog.is_none() {
            return Err(RecipeIssue::AgentsLoading);
        }
        if !agent_catalog::kind_spawnable(&self.agent, catalog) {
            return Err(RecipeIssue::AgentUnavailable(self.agent.clone()));
        }
        if self.host.is_some() && matches!(self.worktree, WorktreePolicy::Fresh { .. }) {
            return Err(RecipeIssue::RemoteWorktreeUnsupported);
        }

        let cwd = match &self.project {
            RecipeProject::Tracked { id, .. } => {
                let project = projects
                    .get(id)
                    .ok_or_else(|| RecipeIssue::MissingProject(id.clone()))?;
                let actual_host = effective_host(project);
                if actual_host != self.host {
                    return Err(RecipeIssue::ProjectMoved {
                        expected_host: self.host.clone(),
                        actual_host,
                    });
                }
                project.root.clone()
            }
            RecipeProject::Path { path } if self.host.is_none() && !Path::new(path).is_dir() => {
                return Err(RecipeIssue::MissingPath(path.clone()));
            }
            RecipeProject::Path { path } => path.clone(),
        };

        let worktree = match &self.worktree {
            WorktreePolicy::CurrentCheckout => None,
            WorktreePolicy::Fresh { branch } => Some(WorktreeSpawn {
                create: true,
                branch: branch.clone(),
            }),
        };
        Ok(ResolvedRecipe {
            kind: self.agent.clone(),
            options: SpawnOptions {
                cwd: Some(cwd),
                host: self.host.clone(),
                worktree,
                title: self.title.clone(),
                initial_prompt: Some(self.initial_prompt.trim().to_owned()),
                ..SpawnOptions::default()
            },
        })
    }

    fn normalize(&mut self) {
        self.name = self.name.trim().chars().take(80).collect();
        self.initial_prompt = self.initial_prompt.trim().chars().take(32_768).collect();
        self.title = self
            .title
            .take()
            .map(|title| title.trim().chars().take(160).collect::<String>())
            .filter(|title| !title.is_empty());
        if let WorktreePolicy::Fresh { branch } = &mut self.worktree {
            *branch = branch
                .take()
                .map(|branch| branch.trim().chars().take(180).collect::<String>())
                .filter(|branch| !branch.is_empty());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRecipe {
    pub kind: AgentKind,
    pub options: SpawnOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeIssue {
    EmptyPrompt,
    MissingHost(String),
    MissingProject(ProjectId),
    MissingPath(String),
    ProjectMoved {
        expected_host: Option<String>,
        actual_host: Option<String>,
    },
    AgentsLoading,
    AgentUnavailable(AgentKind),
    RemoteWorktreeUnsupported,
}

impl RecipeIssue {
    pub fn message(&self) -> String {
        match self {
            Self::EmptyPrompt => "Add an initial prompt to repair this recipe".to_owned(),
            Self::MissingHost(host) => format!("Host ‘{host}’ is missing — choose a new host"),
            Self::MissingProject(_) => "Project is missing — choose a new project".to_owned(),
            Self::MissingPath(path) => {
                format!("Folder ‘{path}’ is missing — choose a new folder")
            }
            Self::ProjectMoved { .. } => {
                "Project moved to a different host — review the destination".to_owned()
            }
            Self::AgentsLoading => "Checking which Agents are available…".to_owned(),
            Self::AgentUnavailable(kind) => {
                format!("{} is unavailable — choose another Agent", kind.id())
            }
            Self::RemoteWorktreeUnsupported => {
                "Remote recipes cannot create local worktrees".to_owned()
            }
        }
    }
}

/// Kept as a named preference boundary so older preference annotations and
/// focused tests remain readable. `LaunchRecipeBook` owns item-level recovery
/// and unknown-version preservation.
pub fn deserialize_recipe_book<'de, D>(deserializer: D) -> Result<LaunchRecipeBook, D::Error>
where
    D: Deserializer<'de>,
{
    LaunchRecipeBook::deserialize(deserializer)
}

pub fn suggested_recipe_name(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let compact = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "New recipe".to_owned();
    }
    let mut name = compact.chars().take(42).collect::<String>();
    if compact.chars().count() > 42 {
        name.push('…');
    }
    name
}

fn unique_copy_name(name: &str, existing: &[LaunchRecipe]) -> String {
    let base = format!("{} copy", name.trim());
    if !existing.iter().any(|recipe| recipe.name == base) {
        return base;
    }
    for suffix in 2.. {
        let candidate = format!("{base} {suffix}");
        if !existing.iter().any(|recipe| recipe.name == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Prefs, SessionStore, StoreEffect};
    use diri_proto::{AgentDescriptor, AgentReadinessItem, SessionSpawnParams};

    fn catalog(kind: &AgentKind, ready: bool) -> diri_proto::AgentReadinessResult {
        diri_proto::AgentReadinessResult {
            agents: vec![AgentReadinessItem {
                kind: kind.clone(),
                binary: kind.id().to_owned(),
                path: ready.then(|| format!("/usr/bin/{}", kind.id())),
                descriptor: Some(AgentDescriptor {
                    id: kind.id().to_owned(),
                    display_name: "Agent".into(),
                    ..AgentDescriptor::default()
                }),
                ..AgentReadinessItem::default()
            }],
            ..diri_proto::AgentReadinessResult::default()
        }
    }

    fn project(id: &str, root: &str, host: Option<&str>) -> Project {
        Project {
            id: ProjectId(id.into()),
            root: root.into(),
            name: "Diri".into(),
            pinned_order: None,
            host: host.map(str::to_owned),
        }
    }

    #[test]
    fn book_supports_stable_crud_and_ordering() {
        let project = RecipeProject::Path {
            path: "/tmp".into(),
        };
        let mut book = LaunchRecipeBook::default();
        let first = book
            .add(LaunchRecipe::draft(
                "Review",
                AgentKind::CODEX,
                project.clone(),
                None,
                "Review this PR",
            ))
            .expect("add first")
            .id
            .clone();
        let second = book
            .add(LaunchRecipe::draft(
                "Tests",
                AgentKind::CLAUDE_CODE,
                project,
                None,
                "Fix the tests",
            ))
            .expect("add second")
            .id
            .clone();

        book.rename(&first, "Review carefully").expect("rename");
        let copy = book.duplicate(&first).expect("duplicate").id.clone();
        assert!(book.move_by(&copy, -2).expect("move"));
        assert_eq!(book.items()[0].id, copy);
        assert_eq!(book.items()[1].name, "Review carefully");
        assert_eq!(book.remove(&second).expect("remove").name, "Tests");
        assert_eq!(book.items().len(), 2);
    }

    #[test]
    fn tracked_recipe_resolves_through_the_canonical_spawn_options() {
        let kind = AgentKind::CODEX;
        let mut recipe = LaunchRecipe::draft(
            "Fresh review",
            kind.clone(),
            RecipeProject::Tracked {
                id: ProjectId("diri".into()),
                last_known_root: "/old/diri".into(),
            },
            None,
            "Review this branch",
        );
        recipe.title = Some("Review lane".into());
        recipe.worktree = WorktreePolicy::Fresh {
            branch: Some("review/diri".into()),
        };
        let projects =
            HashMap::from([(ProjectId("diri".into()), project("diri", "/new/diri", None))]);

        let resolved = recipe
            .resolve(&projects, &[], Some(&catalog(&kind, true)), |project| {
                project.host.clone()
            })
            .expect("recipe resolves");
        assert_eq!(resolved.kind, kind);
        assert_eq!(resolved.options.cwd.as_deref(), Some("/new/diri"));
        assert_eq!(resolved.options.title.as_deref(), Some("Review lane"));
        assert_eq!(
            resolved.options.worktree,
            Some(WorktreeSpawn {
                create: true,
                branch: Some("review/diri".into())
            })
        );

        let (mut store, mut effects) = SessionStore::headless(Prefs::default());
        store.spawn_kind(resolved.kind, resolved.options);
        assert_eq!(
            effects.try_recv().expect("canonical spawn effect"),
            StoreEffect::Spawn(SessionSpawnParams {
                kind: AgentKind::CODEX,
                cwd: "/new/diri".into(),
                new_worktree: Some(true),
                worktree_branch: Some("review/diri".into()),
                title: Some("Review lane".into()),
                initial_prompt: Some("Review this branch".into()),
                parent: None,
                initial_cols: None,
                initial_rows: None,
                host: None,
                same_repo_as: None,
            })
        );
    }

    #[test]
    fn legacy_remote_project_resolves_through_the_effective_host_projection() {
        let kind = AgentKind::CODEX;
        let recipe = LaunchRecipe::draft(
            "Remote tests",
            kind.clone(),
            RecipeProject::Tracked {
                id: ProjectId("diri".into()),
                last_known_root: "~/old/diri".into(),
            },
            Some("forge".into()),
            "Run tests",
        );
        let projects = HashMap::from([(ProjectId("diri".into()), project("diri", "~/diri", None))]);
        let hosts = vec![HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge".into(),
            default_cwd: None,
            node: None,
        }];
        let resolved = recipe
            .resolve(&projects, &hosts, Some(&catalog(&kind, true)), |_| {
                Some("forge".into())
            })
            .expect("legacy project uses its session-derived host");
        assert_eq!(resolved.options.host.as_deref(), Some("forge"));
        assert_eq!(resolved.options.cwd.as_deref(), Some("~/diri"));
    }

    #[test]
    fn stale_dependencies_fail_with_specific_repair_states() {
        let kind = AgentKind::CODEX;
        let mut recipe = LaunchRecipe::draft(
            "Remote",
            kind.clone(),
            RecipeProject::Path {
                path: "~/diri".into(),
            },
            Some("forge".into()),
            "Run the tests",
        );
        assert_eq!(
            recipe.resolve(
                &HashMap::new(),
                &[],
                Some(&catalog(&kind, true)),
                |project| project.host.clone()
            ),
            Err(RecipeIssue::MissingHost("forge".into()))
        );

        let hosts = vec![HostEntry {
            id: "forge".into(),
            name: Some("Forge".into()),
            ssh: "forge".into(),
            default_cwd: None,
            node: None,
        }];
        recipe.worktree = WorktreePolicy::Fresh { branch: None };
        assert_eq!(
            recipe.resolve(
                &HashMap::new(),
                &hosts,
                Some(&catalog(&kind, true)),
                |project| project.host.clone()
            ),
            Err(RecipeIssue::RemoteWorktreeUnsupported)
        );
        recipe.worktree = WorktreePolicy::CurrentCheckout;
        assert!(matches!(
            recipe.resolve(
                &HashMap::new(),
                &hosts,
                Some(&catalog(&kind, false)),
                |project| project.host.clone()
            ),
            Err(RecipeIssue::AgentUnavailable(_))
        ));
    }

    #[test]
    fn malformed_entries_are_isolated_and_unknown_versions_are_preserved() {
        let good = serde_json::to_value(LaunchRecipe::draft(
            "Review",
            AgentKind::CODEX,
            RecipeProject::Path {
                path: "/tmp".into(),
            },
            None,
            "Review this",
        ))
        .expect("serialize recipe");
        let json = serde_json::json!({
            "version": 1,
            "nextId": 2,
            "items": [good.clone(), {"broken": true}, good]
        });
        let book: LaunchRecipeBook = serde_json::from_value(json).expect("decode tolerant book");
        assert_eq!(book.items().len(), 2);

        let future = serde_json::json!({
            "version": 99,
            "nextId": 900,
            "items": [{"futureShape": true}],
            "futurePolicy": "keep-me"
        });
        let mut book: LaunchRecipeBook =
            serde_json::from_value(future.clone()).expect("preserve future book");
        assert!(book.items().is_empty());
        assert_eq!(
            book.add(LaunchRecipe::draft(
                "No downgrade",
                AgentKind::CODEX,
                RecipeProject::Path {
                    path: "/tmp".into()
                },
                None,
                "Stay intact"
            )),
            Err(RecipeBookError::UnsupportedVersion)
        );
        assert_eq!(serde_json::to_value(book).expect("reserialize"), future);
    }

    #[test]
    fn capacity_and_maximum_id_are_bounded() {
        let mut value = serde_json::json!({
            "version": 1,
            "nextId": u64::MAX,
            "items": []
        });
        let items = value["items"].as_array_mut().expect("items");
        for id in 1..=MAX_RECIPES {
            let mut recipe = serde_json::to_value(LaunchRecipe::draft(
                format!("Recipe {id}"),
                AgentKind::CODEX,
                RecipeProject::Path {
                    path: "/tmp".into(),
                },
                None,
                "Run",
            ))
            .expect("recipe");
            recipe["id"] = serde_json::Value::String(if id == 1 {
                format!("recipe-{}", u64::MAX)
            } else {
                format!("recipe-{id}")
            });
            items.push(recipe);
        }
        let mut book: LaunchRecipeBook = serde_json::from_value(value).expect("decode max id");
        assert_eq!(book.items().len(), MAX_RECIPES);
        assert_eq!(
            book.add(LaunchRecipe::draft(
                "One too many",
                AgentKind::CODEX,
                RecipeProject::Path {
                    path: "/tmp".into()
                },
                None,
                "Run"
            )),
            Err(RecipeBookError::Full)
        );
    }

    #[test]
    fn generated_names_are_short_and_human() {
        assert_eq!(
            suggested_recipe_name("\n  Fix   the flaky test\nthen run it"),
            "Fix the flaky test"
        );
        assert!(suggested_recipe_name(&"x".repeat(80)).ends_with('…'));
    }
}
