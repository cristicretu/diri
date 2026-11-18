//! Skill runs found in Claude Code and Codex transcripts, for Settings › Skills.
//!
//! A run is an explicit `Skill` (or Diri `get_skill`) tool call, a `/name`
//! slash command, or an agent reading exactly the `SKILL.md` of a skill with
//! a read-only tool or command. The same skill counts once per turn, so a
//! skill that is loaded and then reread is one run. Slash commands are kept
//! with their source: only the caller knows which names are installed skills
//! rather than built-in commands such as `/clear`.
//!
//! Files are read incrementally like the usage ledger: each transcript keeps
//! its offset, identity, and the runs it held, persisted next to the usage
//! cache, so a refresh reads only appended bytes.

use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use diri_proto::paths::DirijorPaths;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    cache,
    parser::{CompleteLines, tail_hash},
    store::{UsageProvider, file_identity, transcript_files},
    timestamp::parse_timestamp,
};

/// Days of runs kept: the longest range Settings offers is a year.
pub const RETENTION_DAYS: i64 = 366;
const CACHE_VERSION: u32 = 1;
const CACHE_FILE_NAME: &str = "skill-usage-cache.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SkillSource {
    /// A `Skill` tool call (or Diri's `get_skill`).
    Tool,
    /// A `/name` slash command; may name a built-in command, not a skill.
    Slash,
    /// The agent read one skill's `SKILL.md`.
    Read,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillRun {
    /// Unix seconds.
    pub at: i64,
    /// The skill's name without a `plugin:` prefix.
    pub name: String,
    pub source: SkillSource,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FileEntry {
    size: u64,
    modified_ns: u64,
    device: Option<u64>,
    inode: Option<u64>,
    offset: u64,
    tail_hash: u64,
    /// Skills already counted in the turn the file ended in.
    turn_skills: Vec<String>,
    runs: Vec<SkillRun>,
}

impl FileEntry {
    fn new_turn(&mut self) {
        self.turn_skills.clear();
    }

    fn record(&mut self, at: i64, name: &str, source: SkillSource) {
        let name = normalize(name);
        if name.is_empty() || self.turn_skills.iter().any(|seen| seen == name) {
            return;
        }
        self.turn_skills.push(name.to_owned());
        self.runs.push(SkillRun {
            at,
            name: name.to_owned(),
            source,
        });
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct CacheFile {
    version: u32,
    files: BTreeMap<String, FileEntry>,
}

pub struct SkillUsageStore {
    roots: Vec<(PathBuf, UsageProvider)>,
    cache_file: PathBuf,
    cache: CacheFile,
}

impl SkillUsageStore {
    #[must_use]
    pub fn for_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        let cache_file = DirijorPaths::usage_cache_file(home).with_file_name(CACHE_FILE_NAME);
        Self::with_paths(
            vec![
                (home.join(".claude/projects"), UsageProvider::Claude),
                (home.join(".config/claude/projects"), UsageProvider::Claude),
                (home.join(".codex/sessions"), UsageProvider::Codex),
            ],
            cache_file,
        )
    }

    #[must_use]
    pub fn with_paths(roots: Vec<(PathBuf, UsageProvider)>, cache_file: PathBuf) -> Self {
        let cache = fs::read(&cache_file)
            .ok()
            .and_then(|data| serde_json::from_slice::<CacheFile>(&data).ok())
            .filter(|cache| cache.version == CACHE_VERSION)
            .unwrap_or_default();
        Self {
            roots,
            cache_file,
            cache,
        }
    }

    /// Reads appended transcript bytes and returns every run of the last
    /// [`RETENTION_DAYS`], oldest file first.
    pub fn refresh(&mut self, now: i64) -> Vec<SkillRun> {
        let cutoff = now - RETENTION_DAYS * 86_400;
        let mut live = HashSet::new();
        let mut changed = self.cache.version != CACHE_VERSION;
        self.cache.version = CACHE_VERSION;
        for (path, provider) in transcript_files(&self.roots) {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| {
                    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
                });
            // A transcript last written before the window holds no run in it.
            if i64::try_from(modified_ns / 1_000_000_000).unwrap_or(i64::MAX) < cutoff {
                continue;
            }
            let key = path.to_string_lossy().into_owned();
            live.insert(key.clone());
            let (device, inode) = file_identity(&metadata);
            let entry = self.cache.files.entry(key).or_default();
            let same_file = match (device, inode, entry.device, entry.inode) {
                (Some(a), Some(b), Some(c), Some(d)) => a == c && b == d,
                _ => true,
            };
            if same_file && entry.size == metadata.len() && entry.modified_ns == modified_ns {
                continue;
            }
            let appended = same_file
                && metadata.len() >= entry.offset
                && tail_hash(&path, entry.offset).is_ok_and(|hash| hash == entry.tail_hash);
            if !appended {
                *entry = FileEntry::default();
            }
            changed = true;
            match scan_file(&path, provider, entry) {
                Ok(consumed) => {
                    entry.offset += consumed;
                    entry.tail_hash = tail_hash(&path, entry.offset).unwrap_or(0);
                    entry.size = metadata.len();
                    entry.modified_ns = modified_ns;
                    entry.device = device;
                    entry.inode = inode;
                }
                // Read it from the start next time.
                Err(_) => *entry = FileEntry::default(),
            }
        }
        let before = self.cache.files.len();
        self.cache.files.retain(|path, _| live.contains(path));
        changed |= self.cache.files.len() != before;
        for entry in self.cache.files.values_mut() {
            entry.runs.retain(|run| run.at >= cutoff);
        }
        if changed {
            let _ = cache::save(&self.cache_file, &self.cache);
        }
        self.cache
            .files
            .values()
            .flat_map(|entry| entry.runs.iter().cloned())
            .collect()
    }
}

fn scan_file(path: &Path, provider: UsageProvider, entry: &mut FileEntry) -> io::Result<u64> {
    let mut lines = CompleteLines::open(path, entry.offset)?;
    while let Some(line) = lines.next_line()? {
        match provider {
            UsageProvider::Claude => claude_line(line, entry),
            UsageProvider::Codex => codex_line(line, entry),
        }
    }
    Ok(lines.consumed())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memmem::find(haystack, needle).is_some()
}

fn timestamp(object: &Value) -> Option<i64> {
    object
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
}

fn claude_line(line: &[u8], entry: &mut FileEntry) {
    // Cheap byte probes first: transcripts run to gigabytes.
    let tool = contains(line, b"\"tool_use\"")
        && (contains(line, b"\"Skill\"")
            || contains(line, b"get_skill\"")
            || contains(line, b"SKILL.md"));
    let prompt = contains(line, b"\"type\":\"user\"") && !contains(line, b"\"tool_result\"");
    if !tool && !prompt {
        return;
    }
    let Ok(object) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let Some(at) = timestamp(&object) else {
        return;
    };
    let content = object.pointer("/message/content");
    match object.get("type").and_then(Value::as_str) {
        Some("user") if prompt => {
            // Skill bodies Claude Code injects after a slash command are meta.
            if object.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return;
            }
            entry.new_turn();
            let text = match content {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return,
            };
            for name in slash_commands(&text) {
                entry.record(at, name, SkillSource::Slash);
            }
        }
        Some("assistant") if tool => {
            let Some(Value::Array(items)) = content else {
                return;
            };
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let tool = item.get("name").and_then(Value::as_str).unwrap_or("");
                let input = |key: &str| {
                    item.pointer(&format!("/input/{key}"))
                        .and_then(Value::as_str)
                };
                if tool == "Skill" {
                    if let Some(name) = input("skill") {
                        entry.record(at, name, SkillSource::Tool);
                    }
                } else if tool.ends_with("__get_skill") {
                    if let Some(name) = input("name") {
                        entry.record(at, name, SkillSource::Tool);
                    }
                } else if tool == "Read" {
                    if let Some(name) = input("file_path").and_then(skill_from_path) {
                        entry.record(at, &name, SkillSource::Read);
                    }
                } else if tool == "Bash"
                    && let Some(command) = input("command")
                {
                    for name in skills_read_by(command) {
                        entry.record(at, &name, SkillSource::Read);
                    }
                }
            }
        }
        _ => {}
    }
}

fn codex_line(line: &[u8], entry: &mut FileEntry) {
    if contains(line, b"\"turn_context\"") {
        if let Ok(object) = serde_json::from_slice::<Value>(line)
            && object.get("type").and_then(Value::as_str) == Some("turn_context")
        {
            entry.new_turn();
        }
        return;
    }
    if !contains(line, b"SKILL.md") || !contains(line, b"\"response_item\"") {
        return;
    }
    let Ok(object) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if object.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(at) = timestamp(&object) else {
        return;
    };
    let Some(payload) = object.get("payload") else {
        return;
    };
    let command = match payload.get("type").and_then(Value::as_str) {
        Some("function_call") => payload
            .get("arguments")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some("custom_tool_call") => payload
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some("local_shell_call") => payload
            .pointer("/action/command")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        _ => None,
    };
    if let Some(command) = command {
        for name in skills_read_by(&command) {
            entry.record(at, &name, SkillSource::Read);
        }
    }
}

/// `plugin:name` → `name`.
#[must_use]
pub fn normalize(name: &str) -> &str {
    name[name.rfind(':').map_or(0, |index| index + 1)..].trim()
}

/// Names in `<command-name>/name</command-name>` tags.
fn slash_commands(text: &str) -> impl Iterator<Item = &str> {
    text.split("<command-name>").skip(1).filter_map(|rest| {
        let name = rest.split("</command-name>").next()?.trim();
        let name = name.strip_prefix('/')?;
        (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
    })
}

fn skill_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// The skill folders named by `…/<name>/SKILL.md` in `text`, in order.
fn skill_folders(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (index, _) in text.match_indices("/SKILL.md") {
        let before = &text[..index];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| !skill_char(*c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let name = &before[start..];
        if !name.is_empty() && name != "." && name != ".." && !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn skill_from_path(path: &str) -> Option<String> {
    if !path.trim_end().ends_with("/SKILL.md") {
        return None;
    }
    skill_folders(path).pop()
}

const READ_VERBS: [&str; 10] = [
    "cat",
    "sed",
    "head",
    "tail",
    "nl",
    "bat",
    "less",
    "more",
    "awk",
    "Get-Content",
];
const WRITE_MARKS: [&str; 8] = [
    "apply_patch",
    "sed -i",
    "tee ",
    "rm ",
    "mv ",
    "cp ",
    "<<",
    "perl -i",
];

/// Skills whose `SKILL.md` a shell command reads, or none when the command
/// may write one or reads it in a way that is not loading it (`ls`, `rg`).
fn skills_read_by(command: &str) -> Vec<String> {
    let reads = command
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|word| READ_VERBS.contains(&word));
    if !reads || WRITE_MARKS.iter().any(|mark| command.contains(mark)) || redirects(command) {
        return Vec::new();
    }
    skill_folders(command)
}

/// An output redirection (`>`, `>>`), but not `2>`, `&>`, `=>` or `->`.
fn redirects(command: &str) -> bool {
    let bytes = command.as_bytes();
    bytes.iter().enumerate().any(|(index, &byte)| {
        byte == b'>'
            && !index
                .checked_sub(1)
                .is_some_and(|before| matches!(bytes[before], b'2' | b'&' | b'=' | b'-' | b'>'))
    })
}

/// One skill's runs over a range of local days.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillTally {
    pub name: String,
    pub total: u32,
    /// One count per day, oldest first.
    pub daily: Vec<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillUsage {
    /// Local epoch day of the first count.
    pub first_day: i64,
    pub days: usize,
    /// Most used first; ties by name.
    pub skills: Vec<SkillTally>,
}

/// Bins `runs` into the `days` local days ending with `today`. Skill names
/// compare case-insensitively; the first spelling seen is shown.
pub fn summarize(
    runs: &[SkillRun],
    days: usize,
    today: i64,
    offset: &dyn Fn(i64) -> i64,
    keep: &dyn Fn(&SkillRun) -> bool,
) -> SkillUsage {
    let first_day = today - days as i64 + 1;
    let mut tallies: BTreeMap<String, SkillTally> = BTreeMap::new();
    for run in runs {
        let day = (run.at + offset(run.at)).div_euclid(86_400) - first_day;
        if day < 0 || day >= days as i64 || !keep(run) {
            continue;
        }
        let tally = tallies
            .entry(run.name.to_lowercase())
            .or_insert_with(|| SkillTally {
                name: run.name.clone(),
                total: 0,
                daily: vec![0; days],
            });
        tally.total += 1;
        tally.daily[day as usize] += 1;
    }
    let mut skills: Vec<_> = tallies.into_values().collect();
    skills.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    SkillUsage {
        first_day,
        days,
        skills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(temp: &Path) -> SkillUsageStore {
        SkillUsageStore::with_paths(
            vec![
                (temp.join(".claude/projects"), UsageProvider::Claude),
                (temp.join(".codex/sessions"), UsageProvider::Codex),
            ],
            temp.join("cache/skills.json"),
        )
    }

    const NOW: i64 = 1_795_000_000;

    fn stamp(offset: i64) -> String {
        let seconds = NOW - offset;
        let day = seconds.div_euclid(86_400);
        let (y, m, d) = super::super::store::civil_from_days(day);
        let rest = seconds.rem_euclid(86_400);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
            rest / 3_600,
            rest % 3_600 / 60,
            rest % 60
        )
    }

    #[test]
    fn claude_counts_tools_slash_commands_and_reads_once_per_turn() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let t = stamp(60);
        let lines = [
            format!(
                r#"{{"type":"user","timestamp":"{t}","message":{{"role":"user","content":"design this"}}}}"#
            ),
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Skill","input":{{"skill":"plugin:frontend-design"}}}}]}}}}"#
            ),
            // Rereading the same skill in the same turn is not another run.
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"/h/.claude/skills/frontend-design/SKILL.md"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"cat ~/.claude/skills/dataviz/SKILL.md"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","timestamp":"{t}","message":{{"role":"user","content":"<command-name>/clear</command-name>"}}}}"#
            ),
            format!(
                r#"{{"type":"user","timestamp":"{t}","isMeta":true,"message":{{"role":"user","content":"Base directory for this skill"}}}}"#
            ),
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Skill","input":{{"skill":"frontend-design"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"sed -i '' s/a/b/ skills/dataviz/SKILL.md"}}}}]}}}}"#
            ),
        ];
        fs::write(dir.join("a.jsonl"), lines.join("\n") + "\n").unwrap();
        let runs = store(temp.path()).refresh(NOW);
        let names: Vec<_> = runs
            .iter()
            .map(|run| (run.name.as_str(), run.source))
            .collect();
        assert_eq!(
            names,
            [
                ("frontend-design", SkillSource::Tool),
                ("dataviz", SkillSource::Read),
                ("clear", SkillSource::Slash),
                ("frontend-design", SkillSource::Tool),
            ]
        );
    }

    #[test]
    fn codex_reads_count_each_skill_and_appends_are_incremental() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".codex/sessions/2026/11/18");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rollout.jsonl");
        let t = stamp(30);
        let first = format!(
            "{{\"type\":\"turn_context\",\"timestamp\":\"{t}\",\"payload\":{{}}}}\n{}\n",
            serde_json::json!({"type":"response_item","timestamp":t,"payload":{"type":"custom_tool_call","name":"exec","input":"text(await tools.exec_command({cmd:\"cat /h/.codex/skills/a/SKILL.md /h/.codex/skills/b/SKILL.md 2>/dev/null\"}))"}})
        );
        fs::write(&file, &first).unwrap();
        let mut store = store(temp.path());
        assert_eq!(store.refresh(NOW).len(), 2);
        let appended = format!(
            "{first}{{\"type\":\"turn_context\",\"timestamp\":\"{t}\",\"payload\":{{}}}}\n{}\n{}\n",
            serde_json::json!({"type":"response_item","timestamp":t,"payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"sed -n 1,80p skills/a/SKILL.md\"]}"}}),
            serde_json::json!({"type":"response_item","timestamp":t,"payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"rg -l foo skills/c/SKILL.md\"]}"}}),
        );
        fs::write(&file, appended).unwrap();
        let runs = store.refresh(NOW);
        assert_eq!(runs.iter().filter(|run| run.name == "a").count(), 2);
        assert!(runs.iter().all(|run| run.name != "c"));
        // The persisted cache serves an unchanged file without reading it.
        let reloaded =
            super::SkillUsageStore::with_paths(store.roots.clone(), store.cache_file.clone());
        assert_eq!(reloaded.cache.files.len(), 1);
    }

    #[test]
    fn summary_bins_local_days_and_sorts_by_runs() {
        let run = |at, name: &str| SkillRun {
            at,
            name: name.into(),
            source: SkillSource::Tool,
        };
        let today = NOW.div_euclid(86_400);
        let runs = [
            run(NOW, "b"),
            run(NOW - 86_400, "B"),
            run(NOW, "a"),
            run(NOW - 40 * 86_400, "a"),
        ];
        let usage = summarize(&runs, 30, today, &|_| 0, &|_| true);
        assert_eq!(usage.skills.len(), 2);
        assert_eq!(usage.skills[0].name, "b");
        assert_eq!(usage.skills[0].daily[29], 1);
        assert_eq!(usage.skills[0].daily[28], 1);
        assert_eq!(usage.skills[1].total, 1);
    }

    #[test]
    fn helpers_parse_names_conservatively() {
        assert_eq!(normalize("diri:notes"), "notes");
        assert_eq!(
            slash_commands("<command-name>/claude-api</command-name>").collect::<Vec<_>>(),
            ["claude-api"]
        );
        assert!(skills_read_by("ls skills/a/SKILL.md").is_empty());
        assert!(skills_read_by("cat x/SKILL.md > y").is_empty());
        assert_eq!(skills_read_by("head -50 x/SKILL.md 2>&1"), ["x"]);
        assert_eq!(skill_from_path("/a/b/SKILL.md").as_deref(), Some("b"));
    }
}
