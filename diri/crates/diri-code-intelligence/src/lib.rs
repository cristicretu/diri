//! Lightweight workspace intelligence shared by Diri's native workbench and
//! its agent-facing tools.
//!
//! Callers cross one seam: create a [`CodeIntelligence`] value for a session,
//! then resolve/open terminal references or search its cached workspace index.
//! Git invocation, traversal safety, file bounds, language classification,
//! symbol heuristics, and ranking remain implementation details. All methods
//! are blocking by design; UI callers dispatch them off GPUI's main thread and
//! MCP callers run them on their request worker.

use std::collections::{HashMap, HashSet};
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde_json::{Value, json};

/// Source files larger than this are not loaded into the native viewer.
pub const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

const MAX_INDEX_FILES: usize = 50_000;
const MAX_SYMBOL_BYTES: u64 = 256 * 1024;
const MAX_TEXT_INDEX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 256;
const MAX_AGENT_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_CANDIDATES_PER_KIND: usize = 5_000;
const MAX_EXCERPT_CONTEXT_LINES: usize = 100;
/// Agent source windows have their own strict context budget, independent of
/// the native viewer's larger file-size policy.
pub const MAX_AGENT_EXCERPT_BYTES: usize = 16 * 1024;
const MAX_AGENT_EXCERPT_LINE_BYTES: usize = 4 * 1024;
const MAX_AGENT_PATH_BYTES: usize = 512;
const MAX_AGENT_PATH_ARGUMENT_CHARS: usize = 4 * 1024;
const MAX_AGENT_ERROR_BYTES: usize = 1024;
const MAX_DISCOVERY_PATH_BYTES: usize = 4 * 1024;
const MAX_DISCOVERY_ENTRIES: usize = 250_000;
const MAX_INDEX_SYMBOL_NAME_CHARS: usize = 256;
const DEFAULT_INDEX_CACHE_AGE: Duration = Duration::from_secs(5 * 60);

const GIT_REPOSITORY_ENVIRONMENT: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".build",
    ".next",
    "build",
    "coverage",
    "DerivedData",
    "dist",
    "node_modules",
    "Pods",
    "target",
    "vendor",
];

/// The deep module exposed to the workbench and terminal adapters.
///
/// Construction and lazy indexing perform blocking filesystem work. The value
/// is owned, `Clone`, `Send`, and `Sync`, so callers can move it through a
/// background task and retain the resulting index for subsequent searches.
#[derive(Clone, Debug)]
pub struct CodeIntelligence {
    workspace_root: PathBuf,
    session_cwd: PathBuf,
    index: OnceLock<WorkspaceIndex>,
    indexed_at: OnceLock<Instant>,
}

/// Small process-local cache for callers that issue several navigation tools
/// in one turn. Index construction is the expensive part; source reads still
/// hit the file on every call. Entries age out quickly so edits become visible,
/// and callers can explicitly request a refresh after a large rewrite.
#[derive(Debug)]
pub struct CodeIntelligenceCache {
    max_age: Duration,
    entries: Mutex<HashMap<WorkspaceCacheKey, CachedIntelligence>>,
}

impl Default for CodeIntelligenceCache {
    fn default() -> Self {
        Self::new(DEFAULT_INDEX_CACHE_AGE)
    }
}

impl CodeIntelligenceCache {
    pub fn new(max_age: Duration) -> Self {
        Self {
            max_age,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn for_session(
        &self,
        cwd: impl AsRef<Path>,
        refresh: bool,
    ) -> Result<Arc<CodeIntelligence>, CodeIntelligenceError> {
        self.for_session_at(cwd.as_ref(), refresh, Instant::now())
    }

    fn for_session_at(
        &self,
        cwd: &Path,
        refresh: bool,
        now: Instant,
    ) -> Result<Arc<CodeIntelligence>, CodeIntelligenceError> {
        let session_cwd = canonical_session_directory(cwd)?;
        let key = WorkspaceCacheKey {
            session_cwd: session_cwd.clone(),
        };
        if !refresh {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.retain(|_, entry| entry.intelligence.index_is_fresh(now, self.max_age));
            if let Some(entry) = entries.get(&key) {
                return Ok(Arc::clone(&entry.intelligence));
            }
        }

        // Repository discovery stays outside the lock. Concurrent first
        // callers may repeat that cheap lookup, but never serialize unrelated
        // workspaces behind Git or filesystem I/O.
        let intelligence = Arc::new(CodeIntelligence::for_canonical_session(session_cwd)?);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                CachedIntelligence {
                    intelligence: Arc::clone(&intelligence),
                },
            );
        Ok(intelligence)
    }

    /// Execute one model-facing workspace tool through the same parsing,
    /// validation, bounds, and wire mapping in every MCP frontend.
    pub fn call_workspace_tool(
        &self,
        cwd: impl AsRef<Path>,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        match tool {
            SEARCH_WORKSPACE_TOOL => {
                validate_tool_arguments(arguments, &["query", "kind", "limit", "refresh"])?;
                let query = required_tool_string(arguments, "query")?;
                if query.chars().count() > 512 {
                    return Err("query must contain at most 512 characters".to_string());
                }
                let kind_wire =
                    optional_tool_string(arguments, "kind")?.unwrap_or_else(|| "all".to_string());
                let kind = WorkspaceSearchKind::from_wire(&kind_wire).ok_or_else(|| {
                    "kind must be one of definitions, mentions, files, or all".to_string()
                })?;
                let limit = bounded_tool_integer(arguments, "limit", 20, 1, 100)?;
                let refresh = optional_tool_bool(arguments, "refresh")?.unwrap_or(false);
                let intelligence = self
                    .for_session(cwd, refresh)
                    .map_err(bounded_agent_error)?;
                let search = intelligence
                    .search_workspace(&query, kind, limit)
                    .map_err(bounded_agent_error)?;
                let stats = intelligence.index_stats();
                let (workspace, workspace_lossy, workspace_truncated) =
                    wire_path(intelligence.workspace_root());
                Ok(json!({
                    "workspace": workspace,
                    "workspacePathLossy": workspace_lossy,
                    "workspacePathTruncated": workspace_truncated,
                    "query": query,
                    "kind": kind_wire,
                    "truncated": search.truncated,
                    "coverage": {
                        "files": stats.files,
                        "searchableFiles": stats.searchable_files,
                        "definitions": stats.definitions,
                        "searchableBytes": stats.searchable_bytes,
                        "fileListTruncated": stats.file_list_truncated,
                        "textIndexTruncated": stats.text_index_truncated,
                    },
                    "matches": search.matches.into_iter().map(|hit| {
                        let (path, path_lossy, path_truncated) = wire_path(&hit.relative_path);
                        json!({
                            "path": path,
                            "pathLossy": path_lossy,
                            "pathTruncated": path_truncated,
                            "kind": hit.kind.as_str(),
                            "line": hit.line,
                            "preview": hit.preview,
                        })
                    }).collect::<Vec<_>>(),
                }))
            }
            READ_SOURCE_TOOL => {
                validate_tool_arguments(arguments, &["path", "line", "context_lines"])?;
                let path = required_tool_string(arguments, "path")?;
                if path.chars().count() > MAX_AGENT_PATH_ARGUMENT_CHARS {
                    return Err(format!(
                        "path must contain at most {MAX_AGENT_PATH_ARGUMENT_CHARS} characters"
                    ));
                }
                let line = optional_tool_integer(arguments, "line", 1, usize::MAX)?;
                let context_lines = bounded_tool_integer(
                    arguments,
                    "context_lines",
                    12,
                    0,
                    MAX_EXCERPT_CONTEXT_LINES,
                )?;
                let intelligence = self.for_session(cwd, false).map_err(bounded_agent_error)?;
                let excerpt = intelligence
                    .source_excerpt(&path, line, context_lines)
                    .map_err(bounded_agent_error)?;
                let (workspace, workspace_lossy, workspace_truncated) =
                    wire_path(intelligence.workspace_root());
                let (path, path_lossy, path_truncated) = wire_path(&excerpt.relative_path);
                Ok(json!({
                    "workspace": workspace,
                    "workspacePathLossy": workspace_lossy,
                    "workspacePathTruncated": workspace_truncated,
                    "path": path,
                    "pathLossy": path_lossy,
                    "pathTruncated": path_truncated,
                    "language": excerpt.language.as_str(),
                    "focusLine": excerpt.focus_line,
                    "truncated": excerpt.truncated,
                    "returnedBytes": excerpt.returned_bytes,
                    "maxBytes": excerpt.max_bytes,
                    "requestedStartLine": excerpt.requested_start_line,
                    "requestedEndLine": excerpt.requested_end_line,
                    "lines": excerpt.lines.into_iter().map(|source| json!({
                        "line": source.number,
                        "text": source.text,
                        "focus": source.number == excerpt.focus_line,
                        "truncated": source.truncated,
                    })).collect::<Vec<_>>(),
                }))
            }
            _ => Err("unknown workspace tool".to_string()),
        }
    }
}

pub const SEARCH_WORKSPACE_TOOL: &str = "search_workspace";
pub const READ_SOURCE_TOOL: &str = "read_source";

#[derive(Clone, Debug)]
pub struct WorkspaceToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn workspace_tool_definitions() -> Vec<WorkspaceToolDefinition> {
    vec![
        WorkspaceToolDefinition {
            name: SEARCH_WORKSPACE_TOOL,
            description: "Navigate the calling agent's local repository with Diri's bounded, git-aware index. Definitions are declaration heuristics; mentions are honest literal text matches and may include comments, strings, and documentation (they are not structural callers); files are ranked paths. Prefer this over broad grep when locating a declaration or narrowing where to edit. A lossy/truncated path is display-only; use the shell for that rare filename.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2, "maxLength": 512, "description": "Symbol, identifier, text, or path fragment." },
                    "kind": { "type": "string", "enum": ["definitions", "mentions", "files", "all"], "default": "all" },
                    "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 },
                    "refresh": { "type": "boolean", "default": false, "description": "Rebuild the bounded index now; use after edits when immediate freshness matters." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        WorkspaceToolDefinition {
            name: READ_SOURCE_TOOL,
            description: "Read a small, byte-bounded numbered source window inside the calling agent's local repository. Use a path returned by search_workspace, optionally centered on its line. The result reports truncation; paths outside the workspace, binary files, and oversized files are rejected.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": MAX_AGENT_PATH_ARGUMENT_CHARS },
                    "line": { "type": "integer", "minimum": 1 },
                    "context_lines": { "type": "integer", "default": 12, "minimum": 0, "maximum": MAX_EXCERPT_CONTEXT_LINES }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn is_workspace_tool(name: &str) -> bool {
    matches!(name, SEARCH_WORKSPACE_TOOL | READ_SOURCE_TOOL)
}

fn validate_tool_arguments(arguments: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be an object".to_string())?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("tool arguments contain an unknown field".to_string());
    }
    Ok(())
}

fn required_tool_string(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} must be a non-empty string"))
}

fn optional_tool_string(arguments: &Value, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("{key} must be a string")),
    }
}

fn optional_tool_bool(arguments: &Value, key: &str) -> Result<Option<bool>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a boolean")),
    }
}

fn bounded_tool_integer(
    arguments: &Value,
    key: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    Ok(optional_tool_integer(arguments, key, minimum, maximum)?.unwrap_or(default))
}

fn optional_tool_integer(
    arguments: &Value,
    key: &str,
    minimum: usize,
    maximum: usize,
) -> Result<Option<usize>, String> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let integer = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("{key} must be an integer"))?;
    if !(minimum..=maximum).contains(&integer) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(Some(integer))
}

fn wire_path(path: &Path) -> (String, bool, bool) {
    let (display, lossy) = match path.to_str() {
        Some(path) => (path.to_string(), false),
        None => (path.to_string_lossy().into_owned(), true),
    };
    let (display, truncated) = truncate_utf8_bytes(&display, MAX_AGENT_PATH_BYTES);
    (display, lossy, truncated)
}

fn bounded_agent_error(error: impl fmt::Display) -> String {
    truncate_utf8_bytes(&error.to_string(), MAX_AGENT_ERROR_BYTES).0
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WorkspaceCacheKey {
    session_cwd: PathBuf,
}

#[derive(Debug)]
struct CachedIntelligence {
    intelligence: Arc<CodeIntelligence>,
}

impl CodeIntelligence {
    /// Resolve the session directory to a canonical Git root when possible,
    /// otherwise use the canonical session directory as the workspace root.
    /// Indexing is lazy: terminal references can open immediately, while the
    /// file tree and search index are built on their first use.
    pub fn for_session(cwd: impl AsRef<Path>) -> Result<Self, CodeIntelligenceError> {
        Self::for_canonical_session(canonical_session_directory(cwd.as_ref())?)
    }

    fn for_canonical_session(session_cwd: PathBuf) -> Result<Self, CodeIntelligenceError> {
        let workspace_root = discover_workspace_root(&session_cwd)?;
        Ok(Self {
            workspace_root,
            session_cwd,
            index: OnceLock::new(),
            indexed_at: OnceLock::new(),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    fn index(&self) -> &WorkspaceIndex {
        self.index.get_or_init(|| {
            let index = build_index(&self.workspace_root);
            // Freshness starts when the expensive snapshot is complete, not
            // when its lazy container was inserted into the cache.
            let _ = self.indexed_at.set(Instant::now());
            index
        })
    }

    fn index_is_fresh(&self, now: Instant, max_age: Duration) -> bool {
        self.indexed_at
            .get()
            .is_none_or(|indexed_at| now.saturating_duration_since(*indexed_at) <= max_age)
    }

    /// Describe the bounded corpus backing agent searches so callers can tell
    /// whether a result came from broad repository coverage or a small subset.
    pub fn index_stats(&self) -> WorkspaceIndexStats {
        let index = self.index();
        WorkspaceIndexStats {
            files: index.files.len(),
            searchable_files: index.texts.len(),
            definitions: index.symbols.len(),
            searchable_bytes: index.searchable_bytes,
            file_list_truncated: index.file_list_truncated,
            text_index_truncated: index.text_index_truncated,
        }
    }

    /// Resolve a terminal-shaped reference without loading its contents.
    /// Relative paths are attempted from the session cwd first and then from
    /// the workspace root. A successful result is always a canonical file
    /// contained by the workspace root.
    pub fn resolve_reference(
        &self,
        terminal_text: &str,
    ) -> Result<ResolvedReference, CodeIntelligenceError> {
        let candidates = parse_reference_candidates(terminal_text);
        if candidates.is_empty() {
            return Err(CodeIntelligenceError::NoFileReference {
                text: excerpt(terminal_text, 160),
            });
        }

        let mut first_error = None;
        for candidate in candidates {
            let bases = if candidate.path.is_absolute() || self.session_cwd == self.workspace_root {
                [Some(self.workspace_root.as_path()), None]
            } else {
                [
                    Some(self.session_cwd.as_path()),
                    Some(self.workspace_root.as_path()),
                ]
            };

            for base in bases.into_iter().flatten() {
                let requested = if candidate.path.is_absolute() {
                    candidate.path.clone()
                } else {
                    base.join(&candidate.path)
                };
                match self.resolve_workspace_file(&requested) {
                    Ok((absolute_path, relative_path)) => {
                        return Ok(ResolvedReference {
                            absolute_path,
                            relative_path,
                            target: candidate.target,
                        });
                    }
                    Err(error) => {
                        if first_error.is_none()
                            || matches!(error, CodeIntelligenceError::OutsideWorkspace { .. })
                        {
                            first_error = Some(error);
                        }
                    }
                }
            }
        }

        Err(
            first_error.unwrap_or_else(|| CodeIntelligenceError::NoFileReference {
                text: excerpt(terminal_text, 160),
            }),
        )
    }

    /// Resolve a terminal reference and return a bounded, viewer-ready source
    /// snapshot in one call.
    pub fn open_reference(
        &self,
        terminal_text: &str,
    ) -> Result<SourceSnapshot, CodeIntelligenceError> {
        let reference = self.resolve_reference(terminal_text)?;
        self.load_resolved(reference)
    }

    /// Rank cached file paths and symbol-like declarations. Results are stable
    /// for equal scores and bounded by `limit`; this performs no filesystem I/O.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if limit == 0 {
            return Vec::new();
        }

        let query = query.trim();
        let mut results = Vec::new();
        let index = self.index();
        for file in &index.files {
            let score = if query.is_empty() {
                Some(0)
            } else {
                path_score(query, &file.display_path)
            };
            if let Some(score) = score {
                results.push(SearchHit {
                    relative_path: file.relative_path.clone(),
                    kind: SearchHitKind::File,
                    line: None,
                    preview: file.display_path.clone(),
                    score,
                });
            }
        }

        if !query.is_empty() {
            for symbol in &index.symbols {
                let Some(mut score) = fuzzy_score(query, &symbol.name) else {
                    continue;
                };
                if symbol.name.eq_ignore_ascii_case(query) {
                    score = score.saturating_add(6_000);
                } else if symbol
                    .name
                    .to_lowercase()
                    .starts_with(&query.to_lowercase())
                {
                    score = score.saturating_add(2_500);
                }
                results.push(SearchHit {
                    relative_path: symbol.relative_path.clone(),
                    kind: SearchHitKind::Symbol,
                    line: Some(symbol.line),
                    preview: symbol.preview.clone(),
                    score: score.saturating_add(1_000),
                });
            }
        }

        results.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        results.truncate(limit);
        results
    }

    /// Search the bounded workspace corpus with an explicit semantic intent.
    ///
    /// Definition results come from declaration-aware language heuristics,
    /// mentions are honest literal text occurrences that are not an exact
    /// declaration line, including comments, strings, and documentation; they
    /// are not structural callers. Files are ranked paths. This is deliberately
    /// a compact navigation primitive rather than an LSP replacement.
    pub fn search_workspace(
        &self,
        query: &str,
        kind: WorkspaceSearchKind,
        limit: usize,
    ) -> Result<WorkspaceSearchResults, CodeIntelligenceError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(CodeIntelligenceError::EmptySearchQuery);
        }
        if query.chars().count() < 2 {
            return Err(CodeIntelligenceError::SearchQueryTooShort { minimum: 2 });
        }
        let limit = limit.min(MAX_AGENT_SEARCH_RESULTS);
        if limit == 0 {
            return Ok(WorkspaceSearchResults {
                matches: Vec::new(),
                truncated: false,
            });
        }

        let index = self.index();
        let query_folded = query.to_lowercase();
        let mut results = Vec::new();
        let mut truncated = false;

        if matches!(
            kind,
            WorkspaceSearchKind::Definitions | WorkspaceSearchKind::All
        ) {
            for symbol in &index.symbols {
                let Some(mut score) = fuzzy_score(query, &symbol.name) else {
                    continue;
                };
                if symbol.name.eq_ignore_ascii_case(query) {
                    score = score.saturating_add(20_000);
                } else if symbol.name.to_lowercase().starts_with(&query_folded) {
                    score = score.saturating_add(8_000);
                }
                if results.len() >= MAX_SEARCH_CANDIDATES_PER_KIND {
                    truncated = true;
                    break;
                }
                results.push(WorkspaceMatch {
                    relative_path: symbol.relative_path.clone(),
                    kind: WorkspaceMatchKind::Definition,
                    line: Some(symbol.line),
                    preview: symbol.preview.clone(),
                    score: score.saturating_add(10_000),
                });
            }
        }

        if matches!(
            kind,
            WorkspaceSearchKind::Mentions | WorkspaceSearchKind::All
        ) {
            let mention_start = results.len();
            let definition_lines: HashSet<_> = index
                .symbols
                .iter()
                .filter(|symbol| symbol.name.eq_ignore_ascii_case(query))
                .map(|symbol| (symbol.relative_path.as_path(), symbol.line))
                .collect();
            for source in &index.texts {
                for (line_index, line) in source.text.lines().enumerate() {
                    let line_folded = line.to_lowercase();
                    if !line_folded.contains(&query_folded) {
                        continue;
                    }
                    let line_number = line_index + 1;
                    if definition_lines.contains(&(source.relative_path.as_path(), line_number)) {
                        continue;
                    }
                    let identifier = contains_identifier(&line_folded, &query_folded);
                    let path_bonus = path_score(query, &source.display_path)
                        .unwrap_or(0)
                        .min(500);
                    if results.len().saturating_sub(mention_start) >= MAX_SEARCH_CANDIDATES_PER_KIND
                    {
                        truncated = true;
                        break;
                    }
                    results.push(WorkspaceMatch {
                        relative_path: source.relative_path.clone(),
                        kind: WorkspaceMatchKind::Mention,
                        line: Some(line_number),
                        preview: excerpt(line.trim(), 180),
                        score: 2_000 + u32::from(identifier) * 1_000 + path_bonus,
                    });
                }
                if results.len().saturating_sub(mention_start) >= MAX_SEARCH_CANDIDATES_PER_KIND {
                    break;
                }
            }
        }

        if matches!(kind, WorkspaceSearchKind::Files | WorkspaceSearchKind::All) {
            let file_start = results.len();
            for file in &index.files {
                if let Some(score) = path_score(query, &file.display_path) {
                    if results.len().saturating_sub(file_start) >= MAX_SEARCH_CANDIDATES_PER_KIND {
                        truncated = true;
                        break;
                    }
                    results.push(WorkspaceMatch {
                        relative_path: file.relative_path.clone(),
                        kind: WorkspaceMatchKind::File,
                        line: None,
                        preview: file.display_path.clone(),
                        score: score.saturating_add(4_000),
                    });
                }
            }
        }

        results.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        results.dedup_by(|left, right| {
            left.relative_path == right.relative_path
                && left.line == right.line
                && left.kind == right.kind
        });
        truncated |= results.len() > limit;
        results.truncate(limit);
        Ok(WorkspaceSearchResults {
            matches: results,
            truncated,
        })
    }

    /// Read a bounded, numbered source window around a workspace-contained
    /// path or terminal-shaped reference. The path safety and UTF-8/size
    /// policy are identical to the native source viewer.
    pub fn source_excerpt(
        &self,
        reference: &str,
        focus_line: Option<usize>,
        context_lines: usize,
    ) -> Result<SourceExcerpt, CodeIntelligenceError> {
        let snapshot = self.open_reference(reference)?;
        let focus_line = focus_line
            .or_else(|| snapshot.target.map(|target| target.line))
            .unwrap_or(1)
            .clamp(1, snapshot.lines.len().max(1));
        let context_lines = context_lines.min(MAX_EXCERPT_CONTEXT_LINES);
        let start = focus_line.saturating_sub(context_lines).max(1);
        let end = focus_line
            .saturating_add(context_lines)
            .min(snapshot.lines.len());
        let mut lines = Vec::new();
        let mut returned_bytes = 0usize;
        let mut content_truncated = false;
        let focus_index = focus_line - 1;

        let mut add_line = |index: usize| {
            let line = &snapshot.lines[index];
            let source = sanitize_agent_text(&snapshot.text[line.range.clone()]);
            let remaining = MAX_AGENT_EXCERPT_BYTES.saturating_sub(returned_bytes);
            if remaining == 0 && !source.is_empty() {
                return false;
            }
            let (text, truncated) =
                truncate_utf8_bytes(&source, remaining.min(MAX_AGENT_EXCERPT_LINE_BYTES));
            returned_bytes = returned_bytes.saturating_add(text.len());
            content_truncated |= truncated;
            lines.push(SourceExcerptLine {
                number: line.number,
                text,
                truncated,
            });
            true
        };

        // Spend the budget at the focus first, then expand symmetrically. A
        // giant generated line above the target can never crowd the target out.
        add_line(focus_index);
        for distance in 1..=context_lines {
            let mut added = false;
            if let Some(index) = focus_index.checked_sub(distance)
                && index >= start - 1
            {
                added |= add_line(index);
            }
            let index = focus_index.saturating_add(distance);
            if index < end {
                added |= add_line(index);
            }
            if !added {
                break;
            }
        }
        lines.sort_by_key(|line| line.number);
        let truncated = content_truncated || lines.len() < end.saturating_sub(start - 1);

        Ok(SourceExcerpt {
            relative_path: snapshot.relative_path,
            language: snapshot.language,
            focus_line,
            lines,
            requested_start_line: start,
            requested_end_line: end,
            returned_bytes,
            max_bytes: MAX_AGENT_EXCERPT_BYTES,
            truncated,
        })
    }

    fn resolve_workspace_file(
        &self,
        requested: &Path,
    ) -> Result<(PathBuf, PathBuf), CodeIntelligenceError> {
        let canonical = fs::canonicalize(requested).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                let lexical = lexical_normalize(requested);
                if lexical.starts_with(&self.workspace_root) {
                    CodeIntelligenceError::NotFound {
                        path: requested.to_path_buf(),
                    }
                } else {
                    CodeIntelligenceError::OutsideWorkspace { path: lexical }
                }
            } else {
                CodeIntelligenceError::Io {
                    path: requested.to_path_buf(),
                    operation: "resolve",
                    message: error.to_string(),
                }
            }
        })?;
        if !canonical.starts_with(&self.workspace_root) {
            return Err(CodeIntelligenceError::OutsideWorkspace { path: canonical });
        }
        if !canonical.is_file() {
            return Err(CodeIntelligenceError::NotAFile { path: canonical });
        }
        let relative = canonical
            .strip_prefix(&self.workspace_root)
            .expect("workspace containment was checked")
            .to_path_buf();
        Ok((canonical, relative))
    }

    fn load_resolved(
        &self,
        reference: ResolvedReference,
    ) -> Result<SourceSnapshot, CodeIntelligenceError> {
        let (file, absolute_path, metadata) =
            open_contained_file(&self.workspace_root, &reference.absolute_path)?;
        let relative_path = absolute_path
            .strip_prefix(&self.workspace_root)
            .expect("opened file containment was checked")
            .to_path_buf();
        if metadata.len() > MAX_SOURCE_BYTES {
            return Err(CodeIntelligenceError::TooLarge {
                path: absolute_path,
                size: metadata.len(),
                limit: MAX_SOURCE_BYTES,
            });
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| CodeIntelligenceError::Io {
                path: absolute_path.clone(),
                operation: "read",
                message: error.to_string(),
            })?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(CodeIntelligenceError::TooLarge {
                path: absolute_path,
                size: bytes.len() as u64,
                limit: MAX_SOURCE_BYTES,
            });
        }
        if bytes.contains(&0) {
            return Err(CodeIntelligenceError::BinaryFile {
                path: absolute_path,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| CodeIntelligenceError::NotUtf8 {
            path: absolute_path.clone(),
        })?;
        let lines = source_lines(&text);
        let target = reference
            .target
            .map(|target| clamp_target(target, &text, &lines));
        let language = SourceLanguage::from_path(&relative_path);

        Ok(SourceSnapshot {
            absolute_path,
            relative_path,
            language,
            text,
            lines,
            target,
        })
    }
}

/// Open first, then ask the kernel which object the handle actually names.
/// This closes the canonicalize/open race: later path swaps cannot change the
/// already-open handle we validate and read from.
fn open_contained_file(
    workspace_root: &Path,
    requested: &Path,
) -> Result<(File, PathBuf, fs::Metadata), CodeIntelligenceError> {
    open_contained_file_with(workspace_root, requested, || {})
}

fn open_contained_file_with(
    workspace_root: &Path,
    requested: &Path,
    after_open: impl FnOnce(),
) -> Result<(File, PathBuf, fs::Metadata), CodeIntelligenceError> {
    let file = open_source_handle(requested).map_err(|error| CodeIntelligenceError::Io {
        path: requested.to_path_buf(),
        operation: "open",
        message: error.to_string(),
    })?;
    let opened_metadata = file.metadata().map_err(|error| CodeIntelligenceError::Io {
        path: requested.to_path_buf(),
        operation: "inspect opened file",
        message: error.to_string(),
    })?;
    after_open();
    let opened_path = opened_file_path(&file, requested)?;
    if !opened_path.starts_with(workspace_root) {
        return Err(CodeIntelligenceError::OutsideWorkspace { path: opened_path });
    }
    if !opened_metadata.is_file() {
        return Err(CodeIntelligenceError::PathChangedDuringRead {
            path: requested.to_path_buf(),
        });
    }
    Ok((file, opened_path, opened_metadata))
}

#[cfg(unix)]
fn open_source_handle(requested: &Path) -> io::Result<File> {
    // Resolution already canonicalized source-viewer paths, and discovery
    // rejects indexed symlinks. Refusing a last-component symlink here closes
    // the most useful swap window before the descriptor exists. Nonblocking
    // mode also prevents a raced FIFO from hanging an MCP worker before
    // handle-path and regular-file validation can reject it.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(requested)
}

#[cfg(not(unix))]
fn open_source_handle(requested: &Path) -> io::Result<File> {
    File::open(requested)
}

#[cfg(target_os = "linux")]
fn opened_file_path(file: &File, requested: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|_| {
        CodeIntelligenceError::PathChangedDuringRead {
            path: requested.to_path_buf(),
        }
    })
}

#[cfg(target_os = "macos")]
fn opened_file_path(file: &File, requested: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    let mut buffer = vec![0i8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes to the valid writable
    // buffer, and `file` keeps the descriptor alive for the duration.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if result == -1 {
        return Err(CodeIntelligenceError::PathChangedDuringRead {
            path: requested.to_path_buf(),
        });
    }
    // SAFETY: a successful F_GETPATH writes a nul-terminated C string.
    let bytes = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_bytes()
        .to_vec();
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn opened_file_path(_file: &File, requested: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    Err(CodeIntelligenceError::Io {
        path: requested.to_path_buf(),
        operation: "validate opened file",
        message: "race-resistant contained reads are unavailable on this platform".to_string(),
    })
}

#[derive(Clone, Debug)]
struct WorkspaceIndex {
    files: Vec<IndexedFile>,
    symbols: Vec<IndexedSymbol>,
    texts: Vec<IndexedTextFile>,
    searchable_bytes: u64,
    file_list_truncated: bool,
    text_index_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexedFile {
    relative_path: PathBuf,
    display_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexedTextFile {
    relative_path: PathBuf,
    display_path: String,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedReference {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    /// One-based line and column parsed from the terminal, if present.
    pub target: Option<SourceTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceTarget {
    /// One-based line number. Loaded snapshots clamp it to the document.
    pub line: usize,
    /// One-based character column, not a UTF-8 byte offset. A loaded snapshot
    /// clamps it to a valid caret position on the target line.
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub language: SourceLanguage,
    pub text: String,
    /// One entry per visual source line, including a final empty line when the
    /// file ends in a newline. Ranges exclude line terminators.
    pub lines: Vec<SourceLine>,
    pub target: Option<SourceTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLine {
    pub number: usize,
    /// Byte range into [`SourceSnapshot::text`], excluding `\r`/`\n`.
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub relative_path: PathBuf,
    pub kind: SearchHitKind,
    /// A one-based declaration line for symbol results.
    pub line: Option<usize>,
    pub preview: String,
    /// Larger is a better match. The magnitude is intentionally private to
    /// this implementation; callers should only rely on returned ordering.
    pub score: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SearchHitKind {
    Symbol,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSearchKind {
    Definitions,
    Mentions,
    Files,
    All,
}

impl WorkspaceSearchKind {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "definitions" => Some(Self::Definitions),
            "mentions" => Some(Self::Mentions),
            "files" => Some(Self::Files),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceMatch {
    pub relative_path: PathBuf,
    pub kind: WorkspaceMatchKind,
    pub line: Option<usize>,
    pub preview: String,
    score: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSearchResults {
    pub matches: Vec<WorkspaceMatch>,
    /// True when the caller's limit or the defensive candidate ceiling hid
    /// additional matches. Agents must narrow the query instead of assuming
    /// the returned set is exhaustive.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceIndexStats {
    pub files: usize,
    pub searchable_files: usize,
    pub definitions: usize,
    pub searchable_bytes: u64,
    pub file_list_truncated: bool,
    pub text_index_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkspaceMatchKind {
    Definition,
    Mention,
    File,
}

impl WorkspaceMatchKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Mention => "mention",
            Self::File => "file",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceExcerpt {
    pub relative_path: PathBuf,
    pub language: SourceLanguage,
    pub focus_line: usize,
    pub lines: Vec<SourceExcerptLine>,
    pub requested_start_line: usize,
    pub requested_end_line: usize,
    pub returned_bytes: usize,
    pub max_bytes: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceExcerptLine {
    pub number: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceLanguage {
    Rust,
    Swift,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    Java,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Ruby,
    Shell,
    Markdown,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
    Sql,
    PlainText,
}

impl SourceLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Swift => "swift",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Shell => "shell",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Html => "html",
            Self::Css => "css",
            Self::Sql => "sql",
            Self::PlainText => "text",
        }
    }

    pub fn from_path(path: &Path) -> Self {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        match extension.as_str() {
            "rs" => Self::Rust,
            "swift" => Self::Swift,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" => Self::JavaScript,
            "jsx" => Self::Jsx,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Self::Cpp,
            "cs" => Self::CSharp,
            "rb" => Self::Ruby,
            "sh" | "bash" | "zsh" | "fish" => Self::Shell,
            "md" | "mdx" | "markdown" => Self::Markdown,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "html" | "htm" => Self::Html,
            "css" | "scss" | "sass" | "less" => Self::Css,
            "sql" => Self::Sql,
            _ if matches!(
                file_name.as_str(),
                "bashrc" | "zshrc" | "profile" | ".bashrc" | ".zshrc"
            ) =>
            {
                Self::Shell
            }
            _ => Self::PlainText,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeIntelligenceError {
    EmptySearchQuery,
    SearchQueryTooShort {
        minimum: usize,
    },
    WorkspaceUnavailable {
        path: PathBuf,
        message: String,
    },
    NoFileReference {
        text: String,
    },
    OutsideWorkspace {
        path: PathBuf,
    },
    NotFound {
        path: PathBuf,
    },
    NotAFile {
        path: PathBuf,
    },
    PathChangedDuringRead {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    BinaryFile {
        path: PathBuf,
    },
    NotUtf8 {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        operation: &'static str,
        message: String,
    },
}

impl fmt::Display for CodeIntelligenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySearchQuery => write!(formatter, "Search query cannot be empty"),
            Self::SearchQueryTooShort { minimum } => {
                write!(
                    formatter,
                    "Search query must be at least {minimum} characters"
                )
            }
            Self::WorkspaceUnavailable { path, message } => {
                write!(
                    formatter,
                    "Cannot use workspace {}: {message}",
                    path.display()
                )
            }
            Self::NoFileReference { text } => {
                write!(formatter, "No file reference found in {text:?}")
            }
            Self::OutsideWorkspace { path } => write!(
                formatter,
                "Refusing to open a path outside the workspace: {}",
                path.display()
            ),
            Self::NotFound { path } => write!(formatter, "File not found: {}", path.display()),
            Self::NotAFile { path } => {
                write!(formatter, "Path is not a file: {}", path.display())
            }
            Self::PathChangedDuringRead { path } => write!(
                formatter,
                "Refusing to read {} because it changed while being opened",
                path.display()
            ),
            Self::TooLarge { path, size, limit } => write!(
                formatter,
                "{} is too large for the source viewer ({size} bytes; limit {limit})",
                path.display()
            ),
            Self::BinaryFile { path } => {
                write!(formatter, "{} appears to be a binary file", path.display())
            }
            Self::NotUtf8 { path } => {
                write!(formatter, "{} is not valid UTF-8 text", path.display())
            }
            Self::Io {
                path,
                operation,
                message,
            } => write!(
                formatter,
                "Could not {operation} {}: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CodeIntelligenceError {}

#[derive(Clone, Debug)]
struct IndexedSymbol {
    relative_path: PathBuf,
    name: String,
    line: usize,
    preview: String,
}

#[derive(Debug)]
struct ParsedReference {
    path: PathBuf,
    target: Option<SourceTarget>,
}

fn canonical_session_directory(requested: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    let canonical = fs::canonicalize(requested).map_err(|error| {
        CodeIntelligenceError::WorkspaceUnavailable {
            path: requested.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    if canonical.is_file() {
        canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
            CodeIntelligenceError::WorkspaceUnavailable {
                path: canonical.clone(),
                message: "the session path has no parent directory".to_string(),
            }
        })
    } else if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(CodeIntelligenceError::WorkspaceUnavailable {
            path: canonical,
            message: "the session path is not a directory".to_string(),
        })
    }
}

fn git_command(current_dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for variable in GIT_REPOSITORY_ENVIRONMENT {
        command.env_remove(variable);
    }
    command
}

fn discover_workspace_root(session_cwd: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    if let Ok(output) = git_command(session_cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        && output.status.success()
    {
        let root = String::from_utf8_lossy(&output.stdout);
        let root = root.trim();
        if !root.is_empty() {
            let canonical = fs::canonicalize(root).map_err(|error| {
                CodeIntelligenceError::WorkspaceUnavailable {
                    path: PathBuf::from(root),
                    message: error.to_string(),
                }
            })?;
            // Git environment/configuration must never widen a session into
            // an unrelated tree. A repository root is valid only as a real
            // ancestor of the canonical session directory.
            if valid_workspace_root(session_cwd, &canonical) {
                return Ok(canonical);
            }
        }
    }

    for ancestor in session_cwd.ancestors() {
        if has_git_marker(ancestor) {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(session_cwd.to_path_buf())
}

fn valid_workspace_root(session_cwd: &Path, candidate: &Path) -> bool {
    candidate.is_dir() && session_cwd.starts_with(candidate) && has_git_marker(candidate)
}

fn has_git_marker(candidate: &Path) -> bool {
    fs::symlink_metadata(candidate.join(".git")).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink()
            && (metadata.file_type().is_dir() || metadata.file_type().is_file())
    })
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

struct PathDiscovery {
    paths: Vec<PathBuf>,
    truncated: bool,
}

fn build_index(workspace_root: &Path) -> WorkspaceIndex {
    let discovery = git_paths(workspace_root).unwrap_or_else(|| filesystem_paths(workspace_root));
    let mut files = Vec::with_capacity(discovery.paths.len());
    let mut symbols = Vec::new();
    let mut texts = Vec::new();
    let mut indexed_bytes = 0u64;
    let mut text_index_truncated = false;

    for relative_path in discovery.paths {
        if !safe_relative_path(&relative_path) {
            continue;
        }
        let absolute_path = workspace_root.join(&relative_path);
        let Ok(metadata) = fs::symlink_metadata(&absolute_path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }

        let display_path = relative_path.to_string_lossy().replace('\\', "/");
        let language = SourceLanguage::from_path(&relative_path);
        files.push(IndexedFile {
            relative_path: relative_path.clone(),
            display_path: display_path.clone(),
        });
        if indexed_bytes >= MAX_TEXT_INDEX_BYTES {
            text_index_truncated = true;
        } else if metadata.len() <= MAX_SYMBOL_BYTES
            && let Some(text) = read_indexable_text(workspace_root, &absolute_path)
        {
            let text_bytes = text.len() as u64;
            if indexed_bytes.saturating_add(text_bytes) > MAX_TEXT_INDEX_BYTES {
                text_index_truncated = true;
                continue;
            }
            indexed_bytes = indexed_bytes.saturating_add(text_bytes);
            if language != SourceLanguage::PlainText || is_symbol_text_file(&relative_path) {
                symbols.extend(index_symbols(&text, &relative_path, language));
            }
            texts.push(IndexedTextFile {
                relative_path,
                display_path,
                text,
            });
        }
    }

    files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    symbols.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.line.cmp(&right.line))
    });
    texts.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    WorkspaceIndex {
        files,
        symbols,
        texts,
        searchable_bytes: indexed_bytes,
        file_list_truncated: discovery.truncated,
        text_index_truncated,
    }
}

fn git_paths(workspace_root: &Path) -> Option<PathDiscovery> {
    git_paths_with_limits(workspace_root, MAX_INDEX_FILES, MAX_DISCOVERY_ENTRIES)
}

#[cfg(test)]
fn git_paths_with_limit(workspace_root: &Path, max_files: usize) -> Option<PathDiscovery> {
    git_paths_with_limits(workspace_root, max_files, MAX_DISCOVERY_ENTRIES)
}

fn git_paths_with_limits(
    workspace_root: &Path,
    max_files: usize,
    max_entries: usize,
) -> Option<PathDiscovery> {
    if max_files == 0 || max_entries == 0 {
        return Some(PathDiscovery {
            paths: Vec::new(),
            truncated: true,
        });
    }
    let mut child = git_command(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut paths = Vec::new();
    let mut current = Vec::new();
    let mut oversized = false;
    let mut truncated = false;
    let mut entries_seen = 0usize;
    let mut buffer = [0u8; 8 * 1024];

    'output: loop {
        let read = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        for byte in &buffer[..read] {
            if *byte == 0 {
                entries_seen = entries_seen.saturating_add(1);
                if !current.is_empty() && !oversized {
                    let path = bytes_to_path(&current);
                    if safe_relative_path(&path) && !ignored_path(&path) {
                        paths.push(path);
                        if paths.len() >= max_files {
                            truncated = true;
                            break 'output;
                        }
                    }
                } else if oversized {
                    truncated = true;
                }
                current.clear();
                oversized = false;
                if entries_seen >= max_entries {
                    truncated = true;
                    break 'output;
                }
            } else if current.len() < MAX_DISCOVERY_PATH_BYTES {
                current.push(*byte);
            } else {
                oversized = true;
            }
        }
    }

    if !current.is_empty() || oversized {
        // `git ls-files -z` always terminates paths. Treat malformed or
        // abruptly-ended output as incomplete instead of accepting it as a
        // complete repository snapshot.
        truncated = true;
    }

    drop(stdout);
    if truncated {
        let _ = child.kill();
        let _ = child.wait();
    } else if !child.wait().ok()?.success() {
        return None;
    }
    Some(PathDiscovery { paths, truncated })
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn filesystem_paths(workspace_root: &Path) -> PathDiscovery {
    filesystem_paths_with_limits(workspace_root, MAX_INDEX_FILES, MAX_DISCOVERY_ENTRIES)
}

fn filesystem_paths_with_limits(
    workspace_root: &Path,
    max_files: usize,
    max_entries: usize,
) -> PathDiscovery {
    if max_files == 0 || max_entries == 0 {
        return PathDiscovery {
            paths: Vec::new(),
            truncated: true,
        };
    }
    let mut result = Vec::new();
    let mut pending = vec![workspace_root.to_path_buf()];
    let mut truncated = false;
    let mut entries_seen = 0usize;

    'walk: while let Some(directory) = pending.pop() {
        if result.len() >= max_files {
            truncated = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > max_entries {
                truncated = true;
                break 'walk;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                continue;
            };
            if ignored_path(relative) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if pending.len() < max_entries {
                    pending.push(path);
                } else {
                    truncated = true;
                }
            } else if file_type.is_file() {
                result.push(relative.to_path_buf());
                if result.len() >= max_files {
                    truncated = true;
                    break;
                }
            }
        }
    }
    PathDiscovery {
        paths: result,
        truncated,
    }
}

fn ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy();
        IGNORED_DIRECTORIES
            .iter()
            .any(|ignored| name.eq_ignore_ascii_case(ignored))
    })
}

fn read_indexable_text(workspace_root: &Path, absolute_path: &Path) -> Option<String> {
    let (file, _, metadata) = open_contained_file(workspace_root, absolute_path).ok()?;
    if metadata.len() > MAX_SYMBOL_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_SYMBOL_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_SYMBOL_BYTES
        || bytes.contains(&0)
    {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn index_symbols(text: &str, relative_path: &Path, language: SourceLanguage) -> Vec<IndexedSymbol> {
    text.lines()
        .enumerate()
        .filter_map(|(line, source)| {
            let name = symbol_name(source, language)?;
            Some(IndexedSymbol {
                relative_path: relative_path.to_path_buf(),
                name,
                line: line + 1,
                preview: excerpt(source.trim(), 180),
            })
        })
        .take(MAX_SYMBOLS_PER_FILE)
        .collect()
}

fn contains_identifier(line: &str, query: &str) -> bool {
    line.match_indices(query).any(|(start, matched)| {
        let end = start + matched.len();
        let left = line[..start].chars().next_back();
        let right = line[end..].chars().next();
        !left.is_some_and(is_identifier_character) && !right.is_some_and(is_identifier_character)
    })
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn symbol_name(line: &str, language: SourceLanguage) -> Option<String> {
    let mut line = line.trim_start();
    if line.is_empty()
        || line
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '/' | '#' | '*'))
    {
        return None;
    }

    // Remove common access and declaration modifiers without exposing a full
    // parser at the module seam. The language-specific declaration token below
    // keeps ordinary prose and expressions out of the symbol index.
    loop {
        let next = [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "public ",
            "private ",
            "protected ",
            "internal ",
            "open ",
            "final ",
            "static ",
            "async ",
            "export default ",
            "export ",
            "default ",
        ]
        .into_iter()
        .find_map(|prefix| line.strip_prefix(prefix));
        let Some(next) = next else { break };
        line = next.trim_start();
    }

    let prefixes: &[&str] = match language {
        SourceLanguage::Rust => &[
            "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ", "mod ",
        ],
        SourceLanguage::Swift => &[
            "func ",
            "struct ",
            "class ",
            "enum ",
            "protocol ",
            "actor ",
            "typealias ",
            "let ",
            "var ",
        ],
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => &[
            "function ",
            "class ",
            "interface ",
            "type ",
            "enum ",
            "const ",
            "let ",
            "var ",
        ],
        SourceLanguage::Python => &["def ", "class ", "async def "],
        SourceLanguage::Go => &["func ", "type ", "const ", "var "],
        SourceLanguage::Java | SourceLanguage::Kotlin | SourceLanguage::CSharp => &[
            "class ",
            "interface ",
            "enum ",
            "record ",
            "fun ",
            "object ",
        ],
        SourceLanguage::C | SourceLanguage::Cpp => {
            &["class ", "struct ", "enum ", "namespace ", "typedef "]
        }
        SourceLanguage::Ruby => &["def ", "class ", "module "],
        SourceLanguage::Shell => &["function "],
        SourceLanguage::Css => &["@keyframes ", "@mixin ", "@function "],
        SourceLanguage::Sql => &["CREATE TABLE ", "CREATE VIEW ", "CREATE FUNCTION "],
        SourceLanguage::Markdown => &["# ", "## ", "### ", "#### ", "##### ", "###### "],
        _ => &[],
    };

    let declaration = prefixes.iter().find_map(|prefix| {
        if language == SourceLanguage::Sql {
            line.to_ascii_uppercase()
                .starts_with(prefix)
                .then(|| &line[prefix.len()..])
        } else {
            line.strip_prefix(prefix)
        }
    })?;
    let name: String = declaration
        .trim_start_matches(['&', '*'])
        .chars()
        .take_while(|character| {
            character.is_alphanumeric()
                || matches!(character, '_' | '-' | '.' | ':' | '<' | '>' | '?')
        })
        .take(MAX_INDEX_SYMBOL_NAME_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn is_symbol_text_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "Dockerfile" | "Makefile" | "CMakeLists.txt" | "Justfile"
            )
        })
}

fn parse_reference_candidates(raw: &str) -> Vec<ParsedReference> {
    let mut fragments = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |fragment: &str| {
        let fragment = fragment.trim();
        if !fragment.is_empty() && seen.insert(fragment.to_string()) {
            fragments.push(fragment.to_string());
        }
    };

    push(raw);
    for token in raw.split_whitespace() {
        push(token);
    }
    for quote in ['\'', '"', '`'] {
        let mut positions = raw.match_indices(quote).map(|(position, _)| position);
        while let (Some(start), Some(end)) = (positions.next(), positions.next()) {
            if start + quote.len_utf8() < end {
                push(&raw[start + quote.len_utf8()..end]);
            }
        }
    }
    if let Some(start) = raw.find("file://") {
        let uri = &raw[start..];
        let end = uri.find(char::is_whitespace).unwrap_or(uri.len());
        push(&uri[..end]);
    }

    let mut result = Vec::new();
    let mut parsed_seen = HashSet::new();
    for fragment in fragments {
        let cleaned = clean_wrappers(&fragment);
        if let Some(parsed) = parse_reference_fragment(cleaned) {
            let key = (parsed.path.clone(), parsed.target);
            if parsed_seen.insert(key) {
                result.push(parsed);
            }
        }
    }
    result
}

fn clean_wrappers(mut text: &str) -> &str {
    text = text.trim();
    loop {
        text = text.trim_end_matches([',', ';', '!', '?']).trim();
        let bytes = text.as_bytes();
        let wrapped = bytes.len() >= 2
            && matches!(
                (bytes[0], bytes[bytes.len() - 1]),
                (b'(', b')')
                    | (b'[', b']')
                    | (b'{', b'}')
                    | (b'<', b'>')
                    | (b'\'', b'\'')
                    | (b'"', b'"')
                    | (b'`', b'`')
            );
        if wrapped {
            text = text[1..text.len() - 1].trim();
        } else {
            break;
        }
    }
    text.trim_matches(|character: char| {
        matches!(
            character,
            '\'' | '"' | '`' | '[' | ']' | '{' | '}' | '<' | '>'
        )
    })
}

fn parse_reference_fragment(fragment: &str) -> Option<ParsedReference> {
    let fragment = clean_wrappers(fragment);
    if fragment.is_empty() {
        return None;
    }

    if let Some(uri) = fragment.strip_prefix("file://") {
        return parse_file_uri(uri);
    }

    if let Some(parsed) = parse_stack_reference(fragment) {
        return Some(parsed);
    }

    let fragment = fragment.trim_end_matches(['.', ':']);
    let (path, target) = parse_colon_target(fragment);
    looks_like_path(path).then(|| ParsedReference {
        path: PathBuf::from(path),
        target,
    })
}

fn parse_file_uri(uri: &str) -> Option<ParsedReference> {
    let uri = uri.strip_prefix("localhost").unwrap_or(uri);
    let uri = uri.trim_end_matches([',', ';', ')', ']']);
    let (encoded_path, fragment) = uri.split_once('#').unwrap_or((uri, ""));
    let decoded = percent_decode(encoded_path)?;
    let target = parse_line_fragment(fragment);
    let path = PathBuf::from(decoded);
    path.is_absolute()
        .then_some(ParsedReference { path, target })
}

fn parse_line_fragment(fragment: &str) -> Option<SourceTarget> {
    let fragment = fragment.strip_prefix('L')?;
    let (line, column) = fragment
        .split_once(['C', ':'])
        .map_or((fragment, None), |(line, column)| (line, Some(column)));
    Some(SourceTarget {
        line: line.parse().ok()?,
        column: column.and_then(|column| column.parse().ok()).unwrap_or(1),
    })
}

fn parse_stack_reference(fragment: &str) -> Option<ParsedReference> {
    let close = fragment.rfind(')')?;
    if !fragment[close + 1..]
        .trim_matches(|character: char| matches!(character, ',' | ';' | '.'))
        .is_empty()
    {
        return None;
    }
    let open = fragment[..close].rfind('(')?;
    let coordinates = &fragment[open + 1..close];
    let (line, column) = coordinates
        .split_once([',', ':'])
        .map_or((coordinates, None), |(line, column)| (line, Some(column)));
    let line = line.trim().parse().ok()?;
    let column = column
        .and_then(|column| column.trim().parse().ok())
        .unwrap_or(1);
    let path = clean_wrappers(fragment[..open].trim());
    let path = path.split_whitespace().last().unwrap_or(path);
    looks_like_path(path).then(|| ParsedReference {
        path: PathBuf::from(path),
        target: Some(SourceTarget { line, column }),
    })
}

fn parse_colon_target(fragment: &str) -> (&str, Option<SourceTarget>) {
    let Some((before_last, last)) = fragment.rsplit_once(':') else {
        return (fragment, None);
    };
    let Ok(last_number) = last.parse::<usize>() else {
        return (fragment, None);
    };
    if let Some((path, line)) = before_last.rsplit_once(':')
        && let Ok(line) = line.parse::<usize>()
    {
        return (
            path,
            Some(SourceTarget {
                line,
                column: last_number,
            }),
        );
    }
    (
        before_last,
        Some(SourceTarget {
            line: last_number,
            column: 1,
        }),
    )
}

fn looks_like_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let path = Path::new(path);
    path.is_absolute()
        || path.components().count() > 1
        || path.extension().is_some()
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                matches!(
                    name,
                    "Cargo.toml" | "Package.swift" | "Makefile" | "Dockerfile" | "CMakeLists.txt"
                )
            })
}

fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn source_lines(text: &str) -> Vec<SourceLine> {
    let mut result = Vec::new();
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        if character != '\n' {
            continue;
        }
        let mut end = offset;
        if end > start && text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        result.push(SourceLine {
            number: result.len() + 1,
            range: start..end,
        });
        start = offset + 1;
    }
    result.push(SourceLine {
        number: result.len() + 1,
        range: start..text.len(),
    });
    result
}

fn clamp_target(target: SourceTarget, text: &str, lines: &[SourceLine]) -> SourceTarget {
    let line = target.line.clamp(1, lines.len().max(1));
    let line_range = &lines[line - 1].range;
    let characters = text[line_range.clone()].chars().count();
    let column = target.column.clamp(1, characters + 1);
    SourceTarget { line, column }
}

fn path_score(query: &str, path: &str) -> Option<u32> {
    let mut score = fuzzy_score(query, path)?;
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if let Some(file_score) = fuzzy_score(query, file_name) {
        score = score.max(file_score.saturating_add(2_000));
    }
    if file_name.eq_ignore_ascii_case(query) {
        score = score.saturating_add(5_000);
    }
    Some(score)
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<u32> {
    let candidate_lower = candidate.to_lowercase();
    let mut total = 0u32;
    for token in query.to_lowercase().split_whitespace() {
        let token_score = if let Some(position) = candidate_lower.find(token) {
            let boundary = position == 0
                || candidate_lower[..position]
                    .chars()
                    .next_back()
                    .is_some_and(|character| !character.is_alphanumeric());
            4_000u32
                .saturating_sub(position.min(2_000) as u32)
                .saturating_add(if boundary { 800 } else { 0 })
                .saturating_add((token.chars().count() as u32).saturating_mul(80))
        } else {
            subsequence_score(token, &candidate_lower)?
        };
        total = total.saturating_add(token_score);
    }
    Some(total.saturating_sub(candidate_lower.chars().count().min(1_000) as u32))
}

fn subsequence_score(query: &str, candidate: &str) -> Option<u32> {
    let mut query = query.chars();
    let mut wanted = query.next()?;
    let mut score = 0u32;
    let mut previous_match = None;
    let mut matched = 0usize;

    for (position, character) in candidate.chars().enumerate() {
        if character != wanted {
            continue;
        }
        matched += 1;
        score = score.saturating_add(180);
        if previous_match.is_some_and(|previous| previous + 1 == position) {
            score = score.saturating_add(220);
        }
        if position == 0
            || candidate
                .chars()
                .nth(position.saturating_sub(1))
                .is_some_and(|previous| !previous.is_alphanumeric())
        {
            score = score.saturating_add(300);
        }
        previous_match = Some(position);
        let Some(next) = query.next() else {
            return Some(
                score
                    .saturating_add((matched as u32).saturating_mul(40))
                    .saturating_sub(position.min(1_000) as u32),
            );
        };
        wanted = next;
    }
    None
}

fn excerpt(text: &str, max_characters: usize) -> String {
    let mut result: String = text.chars().take(max_characters).collect();
    if text.chars().count() > max_characters {
        result.push('…');
    }
    result
}

fn sanitize_agent_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\t' || !character.is_control() {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }

    let (content_limit, ellipsis) = if max_bytes >= '…'.len_utf8() {
        (max_bytes - '…'.len_utf8(), true)
    } else {
        (max_bytes, false)
    };
    let mut end = content_limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = text[..end].to_string();
    if ellipsis {
        result.push('…');
    }
    (result, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn write(path: &Path, contents: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn workspace() -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join(".git")).unwrap();
        temporary
    }

    #[test]
    fn resolves_common_terminal_reference_formats() {
        let workspace = workspace();
        let source = workspace.path().join("src/main.rs");
        write(&source, "fn main() {}\n");
        fs::create_dir_all(workspace.path().join("nested")).unwrap();
        let intelligence = CodeIntelligence::for_session(workspace.path().join("nested")).unwrap();

        let relative = intelligence
            .resolve_reference("  --> [src/main.rs:12:3],")
            .unwrap();
        assert_eq!(relative.relative_path, Path::new("src/main.rs"));
        assert_eq!(
            relative.target,
            Some(SourceTarget {
                line: 12,
                column: 3
            })
        );

        let absolute = intelligence
            .resolve_reference(&format!("{}:7", source.display()))
            .unwrap();
        assert_eq!(absolute.target, Some(SourceTarget { line: 7, column: 1 }));

        let uri = intelligence
            .resolve_reference(&format!("file://{}#L4C2", source.display()))
            .unwrap();
        assert_eq!(uri.target, Some(SourceTarget { line: 4, column: 2 }));

        let stack = intelligence
            .resolve_reference(&format!("at render ({}(9,5))", source.display()))
            .unwrap();
        assert_eq!(stack.target, Some(SourceTarget { line: 9, column: 5 }));
    }

    #[test]
    fn decodes_file_uris_and_rejects_traversal_and_symlink_escapes() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir_all(root.join(".git")).unwrap();
        write(&root.join("space name.rs"), "fn safe() {}\n");
        write(&parent.path().join("secret.rs"), "secret\n");
        let intelligence = CodeIntelligence::for_session(&root).unwrap();

        let encoded = root.join("space%20name.rs");
        let resolved = intelligence
            .resolve_reference(&format!("file://{}#L1", encoded.display()))
            .unwrap();
        assert_eq!(resolved.relative_path, Path::new("space name.rs"));

        assert!(matches!(
            intelligence.resolve_reference("../secret.rs:1"),
            Err(CodeIntelligenceError::OutsideWorkspace { .. })
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(parent.path().join("secret.rs"), root.join("escape.rs"))
                .unwrap();
            assert!(matches!(
                intelligence.open_reference("escape.rs"),
                Err(CodeIntelligenceError::OutsideWorkspace { .. })
            ));
        }
    }

    #[test]
    fn builds_a_git_aware_index_and_ignores_dependency_and_build_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        assert!(status.success());
        write(&workspace.path().join(".gitignore"), "ignored.rs\n");
        write(&workspace.path().join("src/main.rs"), "fn main() {}\n");
        write(
            &workspace.path().join("src/lib.rs"),
            "pub struct Library;\n",
        );
        write(&workspace.path().join("ignored.rs"), "ignored\n");
        write(&workspace.path().join("vendor/crate.rs"), "vendor\n");
        write(&workspace.path().join("build/output.rs"), "build\n");
        write(
            &workspace.path().join("node_modules/pkg/index.js"),
            "package\n",
        );

        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();
        let hits = intelligence.search("", 100);
        let paths: Vec<_> = hits
            .iter()
            .map(|hit| hit.relative_path.to_string_lossy())
            .collect();
        assert!(paths.iter().any(|path| path == "src/main.rs"));
        assert!(paths.iter().any(|path| path == "src/lib.rs"));
        assert!(!paths.iter().any(|path| path == "ignored.rs"));
        assert!(!paths.iter().any(|path| path.starts_with("vendor/")));
        assert!(!paths.iter().any(|path| path.starts_with("build/")));
        assert!(!paths.iter().any(|path| path.starts_with("node_modules/")));
    }

    #[test]
    fn ranks_exact_symbols_and_file_names_above_loose_matches() {
        let workspace = workspace();
        write(
            &workspace.path().join("src/code_intelligence.rs"),
            "pub struct CodeIntelligence;\nfn render_viewer() {}\n",
        );
        write(&workspace.path().join("docs/codebook.md"), "# Codebook\n");
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        let symbols = intelligence.search("CodeIntelligence", 10);
        assert_eq!(symbols[0].kind, SearchHitKind::Symbol);
        assert_eq!(symbols[0].line, Some(1));
        assert_eq!(
            symbols[0].relative_path,
            Path::new("src/code_intelligence.rs")
        );

        let files = intelligence.search("code intel", 10);
        assert_eq!(files[0].kind, SearchHitKind::File);
        assert_eq!(
            files[0].relative_path,
            Path::new("src/code_intelligence.rs")
        );

        let function = intelligence.search("render_viewer", 10);
        assert_eq!(function[0].kind, SearchHitKind::Symbol);
        assert_eq!(function[0].line, Some(2));
    }

    #[test]
    fn agent_search_distinguishes_definitions_mentions_and_files() {
        let workspace = workspace();
        write(
            &workspace.path().join("src/lib.rs"),
            "pub struct SessionLedger;\nimpl SessionLedger {\n    fn open() {}\n}\n",
        );
        write(
            &workspace.path().join("src/use_ledger.rs"),
            "use crate::SessionLedger;\nfn load() -> SessionLedger { todo!() }\n",
        );
        write(
            &workspace.path().join("docs/session-ledger.md"),
            "# SessionLedger notes\n",
        );
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        let definitions = intelligence
            .search_workspace("SessionLedger", WorkspaceSearchKind::Definitions, 10)
            .unwrap()
            .matches;
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].kind, WorkspaceMatchKind::Definition);
        assert_eq!(definitions[0].relative_path, Path::new("src/lib.rs"));
        assert_eq!(definitions[0].line, Some(1));

        let mentions = intelligence
            .search_workspace("SessionLedger", WorkspaceSearchKind::Mentions, 10)
            .unwrap()
            .matches;
        assert_eq!(mentions.len(), 4);
        assert!(
            mentions
                .iter()
                .all(|hit| hit.kind == WorkspaceMatchKind::Mention)
        );
        assert!(
            !mentions
                .iter()
                .any(|hit| hit.relative_path == Path::new("src/lib.rs") && hit.line == Some(1))
        );

        let files = intelligence
            .search_workspace("session ledger", WorkspaceSearchKind::Files, 10)
            .unwrap()
            .matches;
        assert_eq!(files[0].kind, WorkspaceMatchKind::File);
        assert_eq!(files[0].relative_path, Path::new("docs/session-ledger.md"));
    }

    #[test]
    fn source_excerpt_is_bounded_numbered_and_workspace_contained() {
        let workspace = workspace();
        write(
            &workspace.path().join("src/lib.rs"),
            "one\ntwo\nthree\nfour\nfive\n",
        );
        let outside = tempfile::NamedTempFile::new().unwrap();
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        let excerpt = intelligence
            .source_excerpt("src/lib.rs", Some(3), 1)
            .unwrap();
        assert_eq!(excerpt.relative_path, Path::new("src/lib.rs"));
        assert_eq!(excerpt.focus_line, 3);
        assert_eq!(
            excerpt.lines,
            vec![
                SourceExcerptLine {
                    number: 2,
                    text: "two".into(),
                    truncated: false,
                },
                SourceExcerptLine {
                    number: 3,
                    text: "three".into(),
                    truncated: false,
                },
                SourceExcerptLine {
                    number: 4,
                    text: "four".into(),
                    truncated: false,
                },
            ]
        );
        assert!(matches!(
            intelligence.source_excerpt(&outside.path().display().to_string(), Some(1), 1),
            Err(CodeIntelligenceError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn agent_search_rejects_empty_queries_and_caps_results() {
        let workspace = workspace();
        let mut content = String::new();
        for line in 0..150 {
            content.push_str(&format!("let needle_{line} = needle;\n"));
        }
        write(&workspace.path().join("src/lib.rs"), content);
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        assert!(matches!(
            intelligence.search_workspace("  ", WorkspaceSearchKind::All, 10),
            Err(CodeIntelligenceError::EmptySearchQuery)
        ));
        assert!(matches!(
            intelligence.search_workspace("x", WorkspaceSearchKind::All, 10),
            Err(CodeIntelligenceError::SearchQueryTooShort { minimum: 2 })
        ));
        let results = intelligence
            .search_workspace("needle", WorkspaceSearchKind::Mentions, usize::MAX)
            .unwrap();
        assert_eq!(results.matches.len(), MAX_AGENT_SEARCH_RESULTS);
        assert!(results.truncated);
    }

    #[test]
    fn short_lived_cache_reuses_indexes_but_refreshes_on_demand() {
        let workspace = workspace();
        let source = workspace.path().join("src/lib.rs");
        write(&source, "pub struct BeforeEdit;\n");
        let cache = CodeIntelligenceCache::new(Duration::from_secs(60));
        let first = cache.for_session(workspace.path(), false).unwrap();
        assert_eq!(
            first
                .search_workspace("BeforeEdit", WorkspaceSearchKind::Definitions, 10)
                .unwrap()
                .matches
                .len(),
            1
        );

        write(&source, "pub struct AfterEdit;\n");
        let reused = cache.for_session(workspace.path(), false).unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        assert!(
            reused
                .search_workspace("AfterEdit", WorkspaceSearchKind::Definitions, 10)
                .unwrap()
                .matches
                .is_empty()
        );
        assert_eq!(
            reused
                .source_excerpt("src/lib.rs", Some(1), 0)
                .unwrap()
                .lines[0]
                .text,
            "pub struct AfterEdit;"
        );

        let refreshed = cache.for_session(workspace.path(), true).unwrap();
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(
            refreshed
                .search_workspace("AfterEdit", WorkspaceSearchKind::Definitions, 10)
                .unwrap()
                .matches
                .len(),
            1
        );
    }

    #[test]
    fn cache_age_starts_after_the_lazy_index_finishes() {
        let workspace = workspace();
        write(&workspace.path().join("src/lib.rs"), "pub struct Cached;\n");
        let max_age = DEFAULT_INDEX_CACHE_AGE;
        assert!(max_age >= Duration::from_secs(5 * 60));
        let cache = CodeIntelligenceCache::default();
        let inserted = Instant::now();
        let first = cache
            .for_session_at(workspace.path(), false, inserted)
            .unwrap();

        // An unbuilt index has no stale snapshot to expire, even if repository
        // discovery happened a long time ago.
        let unbuilt_later = cache
            .for_session_at(workspace.path(), false, inserted + max_age * 2)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &unbuilt_later));

        assert_eq!(first.index_stats().definitions, 1);
        let indexed_at = *first.indexed_at.get().expect("index completion time");
        let still_fresh = cache
            .for_session_at(
                workspace.path(),
                false,
                indexed_at + max_age - Duration::from_millis(1),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first, &still_fresh));
        let expired = cache
            .for_session_at(
                workspace.path(),
                false,
                indexed_at + max_age + Duration::from_millis(1),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &expired));
    }

    #[test]
    fn source_excerpt_has_a_strict_model_context_budget() {
        let workspace = workspace();
        let long_line = "x".repeat(MAX_AGENT_EXCERPT_LINE_BYTES * 2);
        let contents = (0..20)
            .map(|_| long_line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        write(&workspace.path().join("generated.js"), contents);
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        let excerpt = intelligence
            .source_excerpt("generated.js", Some(10), 10)
            .unwrap();
        assert!(excerpt.truncated);
        assert!(excerpt.returned_bytes <= MAX_AGENT_EXCERPT_BYTES);
        assert_eq!(excerpt.max_bytes, MAX_AGENT_EXCERPT_BYTES);
        assert_eq!(
            excerpt.returned_bytes,
            excerpt
                .lines
                .iter()
                .map(|line| line.text.len())
                .sum::<usize>()
        );
        assert!(excerpt.lines.iter().any(|line| line.number == 10));
        assert!(excerpt.lines.iter().any(|line| line.truncated));

        let cache = CodeIntelligenceCache::default();
        let wire = cache
            .call_workspace_tool(
                workspace.path(),
                READ_SOURCE_TOOL,
                &json!({"path": "generated.js", "line": 10, "context_lines": 10}),
            )
            .unwrap();
        assert_eq!(wire["truncated"], true);
        assert!(wire["returnedBytes"].as_u64().unwrap() <= MAX_AGENT_EXCERPT_BYTES as u64);
        assert_eq!(wire["maxBytes"], MAX_AGENT_EXCERPT_BYTES);
    }

    #[test]
    fn agent_tool_schema_and_validation_share_exact_integer_semantics() {
        let workspace = workspace();
        write(&workspace.path().join("src/lib.rs"), "pub struct Needle;\n");
        let definitions = workspace_tool_definitions();
        let search = definitions
            .iter()
            .find(|definition| definition.name == SEARCH_WORKSPACE_TOOL)
            .unwrap();
        assert_eq!(
            search.input_schema["properties"]["limit"]["type"],
            "integer"
        );
        assert_eq!(
            search.input_schema["properties"]["kind"]["enum"],
            json!(["definitions", "mentions", "files", "all"])
        );
        let source = definitions
            .iter()
            .find(|definition| definition.name == READ_SOURCE_TOOL)
            .unwrap();
        assert_eq!(
            source.input_schema["properties"]["path"]["maxLength"],
            MAX_AGENT_PATH_ARGUMENT_CHARS
        );

        let cache = CodeIntelligenceCache::default();
        for arguments in [
            json!({"query": "Needle", "limit": 1.5}),
            json!({"query": "Needle", "limit": 0}),
            json!({"query": "Needle", "refresh": "yes"}),
            json!({"query": "Needle", "surprise": true}),
        ] {
            assert!(
                cache
                    .call_workspace_tool(workspace.path(), SEARCH_WORKSPACE_TOOL, &arguments)
                    .is_err()
            );
        }
        let result = cache
            .call_workspace_tool(
                workspace.path(),
                SEARCH_WORKSPACE_TOOL,
                &json!({"query": "Needle", "kind": "definitions", "limit": 1}),
            )
            .unwrap();
        assert_eq!(result["matches"][0]["kind"], "definition");

        let oversized_path = "x".repeat(MAX_AGENT_PATH_ARGUMENT_CHARS + 1);
        let path_error = cache
            .call_workspace_tool(
                workspace.path(),
                READ_SOURCE_TOOL,
                &json!({"path": oversized_path}),
            )
            .unwrap_err();
        assert!(path_error.len() <= MAX_AGENT_ERROR_BYTES);

        let maximum_path = "x".repeat(MAX_AGENT_PATH_ARGUMENT_CHARS);
        let filesystem_error = cache
            .call_workspace_tool(
                workspace.path(),
                READ_SOURCE_TOOL,
                &json!({"path": maximum_path}),
            )
            .unwrap_err();
        assert!(filesystem_error.len() <= MAX_AGENT_ERROR_BYTES);

        let oversized_key = "x".repeat(MAX_AGENT_ERROR_BYTES * 4);
        let unknown_error = cache
            .call_workspace_tool(
                workspace.path(),
                SEARCH_WORKSPACE_TOOL,
                &json!({"query": "Needle", oversized_key: true}),
            )
            .unwrap_err();
        assert_eq!(unknown_error, "tool arguments contain an unknown field");

        let oversized_kind = "x".repeat(MAX_AGENT_ERROR_BYTES * 4);
        let kind_error = cache
            .call_workspace_tool(
                workspace.path(),
                SEARCH_WORKSPACE_TOOL,
                &json!({"query": "Needle", "kind": oversized_kind}),
            )
            .unwrap_err();
        assert_eq!(
            kind_error,
            "kind must be one of definitions, mentions, files, or all"
        );
    }

    #[test]
    fn git_discovery_neutralizes_overrides_and_filters_before_its_cap() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success()
        );
        write(&workspace.path().join("build/generated.rs"), "ignored\n");
        write(
            &workspace.path().join("src/lib.rs"),
            "pub struct Included;\n",
        );
        assert!(
            Command::new("git")
                .args(["add", "-f", "build/generated.rs", "src/lib.rs"])
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success()
        );

        let discovery = git_paths_with_limit(workspace.path(), 1).unwrap();
        assert_eq!(discovery.paths, vec![PathBuf::from("src/lib.rs")]);
        assert!(discovery.truncated);

        let globally_bounded = git_paths_with_limits(workspace.path(), 100, 1).unwrap();
        assert!(globally_bounded.truncated);
        assert!(globally_bounded.paths.len() <= 1);

        let command = git_command(workspace.path());
        for variable in GIT_REPOSITORY_ENVIRONMENT {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == *variable && value.is_none()),
                "{variable} must be removed"
            );
        }
        let outside = tempfile::tempdir().unwrap();
        assert!(!valid_workspace_root(workspace.path(), outside.path()));
        assert!(valid_workspace_root(
            &workspace.path().join("src"),
            workspace.path()
        ));

        let fallback = tempfile::tempdir().unwrap();
        write(&fallback.path().join("one.rs"), "one\n");
        write(&fallback.path().join("two.rs"), "two\n");
        let fallback = filesystem_paths_with_limits(fallback.path(), 100, 1);
        assert!(fallback.truncated);
        assert!(fallback.paths.len() <= 1);
    }

    #[test]
    fn pathological_declaration_names_are_bounded_in_the_index() {
        let workspace = workspace();
        let name = "a".repeat(MAX_INDEX_SYMBOL_NAME_CHARS * 4);
        write(
            &workspace.path().join("src/lib.rs"),
            format!("pub struct {name};\n"),
        );
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();
        let index = intelligence.index();
        assert_eq!(index.symbols.len(), 1);
        assert_eq!(
            index.symbols[0].name.chars().count(),
            MAX_INDEX_SYMBOL_NAME_CHARS
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_handle_validation_rejects_a_post_open_symlink_swap() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        let requested = root.join("source.rs");
        let outside = parent.path().join("secret.rs");
        write(&requested, "pub struct Safe;\n");
        write(&outside, "secret\n");

        let result = open_contained_file_with(&root, &requested, || {
            fs::remove_file(&requested).unwrap();
            std::os::unix::fs::symlink(&outside, &requested).unwrap();
        });
        match result {
            Ok((mut file, _, _)) => {
                let mut text = String::new();
                file.read_to_string(&mut text).unwrap();
                assert_eq!(text, "pub struct Safe;\n");
            }
            Err(CodeIntelligenceError::OutsideWorkspace { .. })
            | Err(CodeIntelligenceError::PathChangedDuringRead { .. }) => {}
            Err(error) => panic!("unexpected contained-open error: {error}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_lossy_and_never_panic_mcp_serialization() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(OsString::from_vec(b"non-utf8-\xff.rs".to_vec()));
        let (display, lossy, truncated) = wire_path(&path);
        let result = json!({"path": display, "pathLossy": lossy, "pathTruncated": truncated});
        assert_eq!(result["pathLossy"], true);
        assert!(result["path"].as_str().is_some());
        serde_json::to_string(&result).expect("wire result remains serializable");
    }

    #[test]
    fn rejects_binary_invalid_utf8_and_oversize_files() {
        let workspace = workspace();
        write(&workspace.path().join("binary.dat"), [b'a', 0, b'b']);
        write(&workspace.path().join("invalid.txt"), [0xff, 0xfe]);
        write(
            &workspace.path().join("large.txt"),
            vec![b'x'; MAX_SOURCE_BYTES as usize + 1],
        );
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        assert!(matches!(
            intelligence.open_reference("binary.dat"),
            Err(CodeIntelligenceError::BinaryFile { .. })
        ));
        assert!(matches!(
            intelligence.open_reference("invalid.txt"),
            Err(CodeIntelligenceError::NotUtf8 { .. })
        ));
        assert!(matches!(
            intelligence.open_reference("large.txt"),
            Err(CodeIntelligenceError::TooLarge { .. })
        ));
    }

    #[test]
    fn source_snapshot_has_byte_ranges_and_clamped_character_targets() {
        let workspace = workspace();
        write(&workspace.path().join("source.rs"), "one\r\nhéllo\nlast\n");
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();

        let snapshot = intelligence.open_reference("source.rs:2:99").unwrap();
        assert_eq!(snapshot.lines.len(), 4);
        assert_eq!(snapshot.lines[0].range, 0..3);
        assert_eq!(&snapshot.text[snapshot.lines[1].range.clone()], "héllo");
        assert_eq!(snapshot.target, Some(SourceTarget { line: 2, column: 6 }));

        let clamped = intelligence.open_reference("source.rs:999:999").unwrap();
        assert_eq!(clamped.target, Some(SourceTarget { line: 4, column: 1 }));
    }

    #[test]
    fn no_reference_and_directories_have_specific_errors() {
        let workspace = workspace();
        fs::create_dir_all(workspace.path().join("src")).unwrap();
        let intelligence = CodeIntelligence::for_session(workspace.path()).unwrap();
        assert!(matches!(
            intelligence.resolve_reference("ordinary terminal output"),
            Err(CodeIntelligenceError::NoFileReference { .. })
        ));
        assert!(matches!(
            intelligence.open_reference("./src"),
            Err(CodeIntelligenceError::NotAFile { .. })
        ));
    }
}
