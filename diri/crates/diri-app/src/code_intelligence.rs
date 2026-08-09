//! Lightweight workspace intelligence for native file browsing.
//!
//! Callers cross one seam: create a [`CodeIntelligence`] value for a session,
//! then resolve/open terminal references or search its cached workspace index.
//! Git invocation, traversal safety, file bounds, language classification,
//! symbol heuristics, and ranking remain implementation details. All methods
//! are blocking by design so GPUI can dispatch them to its background executor.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

/// Source files larger than this are not loaded into the native viewer.
pub const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

const MAX_INDEX_FILES: usize = 50_000;
const MAX_SYMBOL_BYTES: u64 = 256 * 1024;
const MAX_SYMBOL_INDEX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 256;

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
}

impl CodeIntelligence {
    /// Resolve the session directory to a canonical Git root when possible,
    /// otherwise use the canonical session directory as the workspace root.
    /// Indexing is lazy: terminal references can open immediately, while the
    /// file tree and search index are built on their first use.
    pub fn for_session(cwd: impl AsRef<Path>) -> Result<Self, CodeIntelligenceError> {
        let requested = cwd.as_ref();
        let canonical = fs::canonicalize(requested).map_err(|error| {
            CodeIntelligenceError::WorkspaceUnavailable {
                path: requested.to_path_buf(),
                message: error.to_string(),
            }
        })?;
        let session_cwd = if canonical.is_file() {
            canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
                CodeIntelligenceError::WorkspaceUnavailable {
                    path: canonical.clone(),
                    message: "the session path has no parent directory".to_string(),
                }
            })?
        } else if canonical.is_dir() {
            canonical
        } else {
            return Err(CodeIntelligenceError::WorkspaceUnavailable {
                path: canonical,
                message: "the session path is not a directory".to_string(),
            });
        };

        let workspace_root = discover_workspace_root(&session_cwd)?;
        Ok(Self {
            workspace_root,
            session_cwd,
            index: OnceLock::new(),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    fn index(&self) -> &WorkspaceIndex {
        self.index.get_or_init(|| build_index(&self.workspace_root))
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
        let metadata =
            fs::metadata(&reference.absolute_path).map_err(|error| CodeIntelligenceError::Io {
                path: reference.absolute_path.clone(),
                operation: "inspect",
                message: error.to_string(),
            })?;
        if metadata.len() > MAX_SOURCE_BYTES {
            return Err(CodeIntelligenceError::TooLarge {
                path: reference.absolute_path,
                size: metadata.len(),
                limit: MAX_SOURCE_BYTES,
            });
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&reference.absolute_path)
            .and_then(|file| {
                file.take(MAX_SOURCE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| ())
            })
            .map_err(|error| CodeIntelligenceError::Io {
                path: reference.absolute_path.clone(),
                operation: "read",
                message: error.to_string(),
            })?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(CodeIntelligenceError::TooLarge {
                path: reference.absolute_path,
                size: bytes.len() as u64,
                limit: MAX_SOURCE_BYTES,
            });
        }
        if bytes.contains(&0) {
            return Err(CodeIntelligenceError::BinaryFile {
                path: reference.absolute_path,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| CodeIntelligenceError::NotUtf8 {
            path: reference.absolute_path.clone(),
        })?;
        let lines = source_lines(&text);
        let target = reference
            .target
            .map(|target| clamp_target(target, &text, &lines));
        let language = SourceLanguage::from_path(&reference.relative_path);

        Ok(SourceSnapshot {
            absolute_path: reference.absolute_path,
            relative_path: reference.relative_path,
            language,
            text,
            lines,
            target,
        })
    }
}

#[derive(Clone, Debug)]
struct WorkspaceIndex {
    files: Vec<IndexedFile>,
    symbols: Vec<IndexedSymbol>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexedFile {
    relative_path: PathBuf,
    display_path: String,
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

fn discover_workspace_root(session_cwd: &Path) -> Result<PathBuf, CodeIntelligenceError> {
    if let Ok(output) = Command::new("git")
        .current_dir(session_cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        && output.status.success()
    {
        let root = String::from_utf8_lossy(&output.stdout);
        let root = root.trim();
        if !root.is_empty() {
            return fs::canonicalize(root).map_err(|error| {
                CodeIntelligenceError::WorkspaceUnavailable {
                    path: PathBuf::from(root),
                    message: error.to_string(),
                }
            });
        }
    }

    for ancestor in session_cwd.ancestors() {
        if ancestor.join(".git").exists() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(session_cwd.to_path_buf())
}

fn build_index(workspace_root: &Path) -> WorkspaceIndex {
    let paths = git_paths(workspace_root).unwrap_or_else(|| filesystem_paths(workspace_root));
    let mut files = Vec::with_capacity(paths.len());
    let mut symbols = Vec::new();
    let mut symbol_bytes = 0u64;

    for relative_path in paths {
        if files.len() >= MAX_INDEX_FILES || ignored_path(&relative_path) {
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
            display_path,
        });
        let symbol_candidate =
            language != SourceLanguage::PlainText || is_symbol_text_file(&relative_path);
        if symbol_candidate
            && metadata.len() <= MAX_SYMBOL_BYTES
            && symbol_bytes.saturating_add(metadata.len()) <= MAX_SYMBOL_INDEX_BYTES
        {
            symbol_bytes = symbol_bytes.saturating_add(metadata.len());
            symbols.extend(index_symbols(&absolute_path, &relative_path, language));
        }
    }

    files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    symbols.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.line.cmp(&right.line))
    });
    WorkspaceIndex { files, symbols }
}

fn git_paths(workspace_root: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .take(MAX_INDEX_FILES)
            .map(bytes_to_path)
            .collect(),
    )
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn filesystem_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut pending = vec![workspace_root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        if result.len() >= MAX_INDEX_FILES {
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
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
                pending.push(path);
            } else if file_type.is_file() {
                result.push(relative.to_path_buf());
                if result.len() >= MAX_INDEX_FILES {
                    break;
                }
            }
        }
    }
    result
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

fn index_symbols(
    absolute_path: &Path,
    relative_path: &Path,
    language: SourceLanguage,
) -> Vec<IndexedSymbol> {
    let Ok(file) = File::open(absolute_path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file.take(MAX_SYMBOL_BYTES).read_to_end(&mut bytes).is_err() || bytes.contains(&0) {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };

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
