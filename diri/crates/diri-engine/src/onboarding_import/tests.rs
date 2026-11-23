//! A fixture home with fourteen transcripts: seven real conversations and
//! seven kinds of junk. Discovery must suggest exactly the five newest real
//! ones, offer the other two for browsing, and drop every piece of junk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::json;

use super::*;

pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    pub home: PathBuf,
    pub anara: PathBuf,
    pub diri: PathBuf,
}

const HOUR: u64 = 3_600;

fn base() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

pub(crate) fn uuid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn touch(path: &Path, hours_ago: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(base() - Duration::from_secs(hours_ago * HOUR))
        .unwrap();
}

fn claude_lines(cwd: &Path, prompt: &str, reply: Option<&str>, sidechain: bool) -> String {
    let cwd = cwd.to_string_lossy();
    let mut lines = vec![
        json!({"type": "permission-mode", "permissionMode": "default"}),
        json!({
            "type": "user", "cwd": cwd, "isSidechain": sidechain,
            "message": {"role": "user", "content": prompt},
        }),
    ];
    if let Some(reply) = reply {
        lines.push(json!({
            "type": "assistant", "cwd": cwd, "isSidechain": sidechain,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "name": "Read"},
                {"type": "text", "text": reply},
            ]},
        }));
    }
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = diri_platform::canonicalize(temp.path()).unwrap();
        let anara = home.join("code/anara");
        let diri = home.join("code/diri");
        std::fs::create_dir_all(&anara).unwrap();
        std::fs::create_dir_all(&diri).unwrap();
        Self {
            _temp: temp,
            home,
            anara,
            diri,
        }
    }

    pub(crate) fn claude(
        &self,
        n: u32,
        cwd: &Path,
        prompt: &str,
        reply: Option<&str>,
        hours_ago: u64,
    ) -> PathBuf {
        let dir = self
            .home
            .join(".claude/projects")
            .join(cwd.to_string_lossy().replace(['/', '.'], "-"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.jsonl", uuid(n)));
        std::fs::write(&path, claude_lines(cwd, prompt, reply, false)).unwrap();
        touch(&path, hours_ago);
        path
    }

    fn claude_raw(&self, name: &str, body: String, hours_ago: u64) -> PathBuf {
        let dir = self.home.join(".claude/projects/-raw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        touch(&path, hours_ago);
        path
    }

    pub(crate) fn codex(
        &self,
        n: u32,
        cwd: &Path,
        prompt: &str,
        reply: Option<&str>,
        subagent: bool,
        hours_ago: u64,
    ) -> PathBuf {
        let dir = self.home.join(".codex/sessions/2027/01/15");
        std::fs::create_dir_all(&dir).unwrap();
        let id = uuid(n);
        let path = dir.join(format!("rollout-2027-01-15T10-00-00-{id}.jsonl"));
        let mut meta = json!({"id": id, "cwd": cwd.to_string_lossy()});
        if subagent {
            meta["source"] = json!({"subagent": {"thread_spawn": {"parent_thread_id": uuid(1)}}});
        }
        let message = |role: &str, kind: &str, text: &str| {
            json!({"type": "response_item", "payload": {
                "type": "message", "role": role,
                "content": [{"type": kind, "text": text}],
            }})
        };
        let mut lines = vec![
            json!({"type": "session_meta", "payload": meta}),
            message("developer", "input_text", "<permissions instructions>"),
            message(
                "user",
                "input_text",
                "# AGENTS.md instructions for the repo",
            ),
            message(
                "user",
                "input_text",
                "<environment_context>cwd</environment_context>",
            ),
            message("user", "input_text", prompt),
        ];
        if let Some(reply) = reply {
            lines.push(message("assistant", "output_text", reply));
        }
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, body).unwrap();
        touch(&path, hours_ago);
        path
    }

    /// Seven real conversations (ids 1–7, newest first by id) and seven
    /// pieces of junk, interleaved in time so junk is never simply oldest.
    pub(crate) fn standard() -> Self {
        let f = Self::new();
        let gone = f.home.join("code/deleted");
        let scratch = f.home.join("work/scratchpad");
        std::fs::create_dir_all(&scratch).unwrap();

        f.claude(
            1,
            &f.anara,
            "Fix the login redirect",
            Some("Done — the redirect now keeps the query string."),
            1,
        );
        f.codex(
            2,
            &f.diri,
            "Speed up the sidebar",
            Some("Profiled it: the row cache was rebuilt per frame."),
            false,
            2,
        );
        f.claude(
            3,
            &f.diri,
            "Write the release notes",
            Some("Drafted the 0.12 notes."),
            3,
        );
        f.claude(
            4,
            &f.anara,
            "Add dark mode",
            Some("Dark mode tokens are in."),
            5,
        );
        f.codex(
            5,
            &f.anara,
            "Migrate the billing table",
            Some("Migration written and tested."),
            false,
            6,
        );
        f.claude(
            6,
            &f.diri,
            "Investigate flaky CI",
            Some("It was a socket timeout."),
            8,
        );
        f.codex(7, &f.diri, "Rename the crate", Some("Renamed."), false, 9);

        // Junk, each newer than some real conversation.
        f.claude_raw(
            &format!("{}.jsonl", uuid(20)),
            claude_lines(&f.anara, "subagent work", Some("ok"), true),
            0,
        );
        f.claude_raw(
            &format!("{}/subagents/agent-a1.jsonl", uuid(1)),
            claude_lines(&f.anara, "nested subagent", Some("ok"), true),
            0,
        );
        f.claude(21, &f.anara, "Typed and quit", None, 1);
        f.claude(
            22,
            Path::new("/private/tmp/experiment"),
            "tmp work",
            Some("ok"),
            2,
        );
        f.claude(23, &scratch, "scratch work", Some("ok"), 3);
        f.claude(24, &gone, "deleted project", Some("ok"), 4);
        f.codex(25, &f.anara, "child thread", Some("ok"), true, 4);
        f.codex(
            26,
            Path::new("/tmp/codex"),
            "tmp codex",
            Some("ok"),
            false,
            7,
        );
        f
    }
}

fn ids(candidates: &[Candidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| candidate.entry.id.clone())
        .collect()
}

#[test]
fn suggests_the_five_newest_real_conversations_and_offers_the_rest() {
    let f = Fixture::standard();
    let found = discover(&f.home, &HashSet::new());
    assert_eq!(ids(&found.suggested), (1..=5).map(uuid).collect::<Vec<_>>());
    assert_eq!(ids(&found.more), vec![uuid(6), uuid(7)]);
    assert!(!found.truncated);

    let first = &found.suggested[0];
    assert_eq!(first.entry.kind, HistoryKind::ClaudeCode);
    assert_eq!(first.entry.cwd, f.anara.to_string_lossy());
    assert_eq!(first.entry.title.as_deref(), Some("Fix the login redirect"));
    assert_eq!(
        first.preview.as_deref(),
        Some("Done — the redirect now keeps the query string.")
    );
    // Codex: the injected AGENTS.md and environment blocks are not the
    // conversation's title.
    let codex = &found.suggested[1];
    assert_eq!(codex.entry.kind, HistoryKind::Codex);
    assert_eq!(codex.entry.title.as_deref(), Some("Speed up the sidebar"));
    assert_eq!(
        codex.preview.as_deref(),
        Some("Profiled it: the row cache was rebuilt per frame.")
    );
}

#[test]
fn every_kind_of_junk_is_rejected_for_its_own_reason() {
    let f = Fixture::standard();
    let all: Vec<_> = {
        let mut listed = list_claude(&f.home.join(".claude/projects"));
        listed.extend(list_codex(&f.home.join(".codex/sessions")));
        listed
    };
    // The nested subagent folder is never even listed.
    assert!(
        all.iter()
            .all(|file| !file.path.to_string_lossy().contains("/subagents/"))
    );
    let reason = |n: u32| {
        let file = all
            .iter()
            .find(|file| file.path.to_string_lossy().contains(&uuid(n)))
            .unwrap();
        inspect(&f.home, file).err()
    };
    assert_eq!(reason(20), Some(Rejection::Subagent));
    assert_eq!(reason(21), Some(Rejection::Empty));
    assert_eq!(reason(22), Some(Rejection::Scratch));
    assert_eq!(reason(23), Some(Rejection::Scratch));
    assert_eq!(reason(24), Some(Rejection::MissingFolder));
    assert_eq!(reason(25), Some(Rejection::Subagent));
    assert_eq!(reason(26), Some(Rejection::Scratch));
    assert_eq!(reason(1), None);
}

#[test]
fn tracked_conversations_are_never_offered_again() {
    let f = Fixture::standard();
    let tracked: HashSet<String> = [uuid(1), uuid(3)].into();
    let found = discover(&f.home, &tracked);
    assert_eq!(
        ids(&found.suggested),
        vec![uuid(2), uuid(4), uuid(5), uuid(6), uuid(7)]
    );
    assert!(found.more.is_empty());
}

#[test]
fn a_fresh_machine_finds_nothing() {
    let f = Fixture::new();
    let found = discover(&f.home, &HashSet::new());
    assert!(found.suggested.is_empty() && found.more.is_empty());
}

#[test]
fn the_scan_is_bounded_on_a_large_history() {
    let f = Fixture::new();
    for n in 0..(SUGGESTED + MAX_MORE + 40) as u32 {
        f.claude(1_000 + n, &f.diri, "work", Some("done"), u64::from(n) + 1);
    }
    let started = std::time::Instant::now();
    let found = discover(&f.home, &HashSet::new());
    assert_eq!(found.suggested.len(), SUGGESTED);
    assert_eq!(found.more.len(), MAX_MORE);
    assert!(found.truncated);
    // Newest first across the whole list.
    let times: Vec<_> = found
        .suggested
        .iter()
        .chain(&found.more)
        .map(|candidate| candidate.entry.last_active_at)
        .collect();
    assert!(times.windows(2).all(|pair| pair[0] >= pair[1]));
    eprintln!("bounded scan: {:?}", started.elapsed());
}

#[test]
fn scratch_paths_are_recognised_outside_home_only() {
    let home = Path::new("/Users/someone");
    assert!(is_scratch_path(home, "/tmp/x"));
    assert!(is_scratch_path(home, "/private/tmp"));
    assert!(is_scratch_path(home, "/private/var/folders/ab/T/x"));
    assert!(is_scratch_path(home, "/Users/someone/w/scratchpad"));
    assert!(!is_scratch_path(home, "/Users/someone/code/anara"));
    // A home that itself lives in a temp folder (fixtures, CI) is not junk.
    assert!(!is_scratch_path(Path::new("/tmp/home"), "/tmp/home/code"));
}

#[test]
fn import_validation_rereads_identity_instead_of_trusting_the_request() {
    let f = Fixture::standard();
    let found = discover(&f.home, &HashSet::new());
    let wire =
        |candidate: &Candidate| crate::control::history_entry_to_wire(candidate.entry.clone());
    let good = wire(&found.suggested[0]);
    assert_eq!(validate_import(&f.home, &good, &HashSet::new()), Ok(()));
    assert_eq!(
        validate_import(&f.home, &good, &[good.id.clone()].into()),
        Err(Rejection::Tracked)
    );
    // Moving the conversation to another folder is refused.
    let mut moved = good.clone();
    moved.cwd = f.diri.to_string_lossy().into_owned();
    assert_eq!(
        validate_import(&f.home, &moved, &HashSet::new()),
        Err(Rejection::Unreadable)
    );
    // A transcript outside the provider's store is refused.
    let mut outside = good.clone();
    let stray = f.home.join(format!("{}.jsonl", good.id));
    std::fs::copy(&good.transcript_path, &stray).unwrap();
    outside.transcript_path = stray.to_string_lossy().into_owned();
    assert_eq!(
        validate_import(&f.home, &outside, &HashSet::new()),
        Err(Rejection::Unreadable)
    );
    // Codex identity comes from its session_meta line.
    let codex = wire(&found.suggested[1]);
    assert_eq!(validate_import(&f.home, &codex, &HashSet::new()), Ok(()));
    let mut renamed = codex.clone();
    renamed.id = uuid(99);
    assert_eq!(
        validate_import(&f.home, &renamed, &HashSet::new()),
        Err(Rejection::Unreadable)
    );
}

#[test]
fn imported_records_are_resumable_and_name_no_process() {
    let f = Fixture::standard();
    let found = discover(&f.home, &HashSet::new());
    let entry = crate::control::history_entry_to_wire(found.suggested[0].entry.clone());
    let project = crate::registry::session_project_id(&entry.cwd, None);
    let record = imported_record("s-1", &entry, project);
    assert_eq!(record.agent_session_id.as_deref(), Some(uuid(1).as_str()));
    assert_eq!(record.resumability, Resumability::Resumable);
    assert!(matches!(&record.status, SessionStatus::Exited(info) if info.is_imported()));
    assert_eq!(record.title, "Fix the login redirect");
    assert_eq!(record.updated_at, entry.last_active_at);
    assert!(record.hibernation.is_none() && record.memory_bytes.is_none());
}

/// Prints what discovery finds in the developer's real home and how long it
/// took. Read-only. `cargo test -p diri-engine live_import_candidates -- --ignored --nocapture`
#[test]
#[ignore = "reads the developer's real ~/.claude and ~/.codex"]
fn live_import_candidates() {
    let home = diri_platform::home_dir().unwrap();
    let started = std::time::Instant::now();
    let found = discover(&home, &HashSet::new());
    let elapsed = started.elapsed();
    for candidate in found.suggested.iter().chain(found.more.iter().take(10)) {
        eprintln!(
            "{:?} {} {:?} | {:?}",
            candidate.entry.kind,
            Path::new(&candidate.entry.cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            candidate.entry.title,
            candidate.preview
        );
    }
    eprintln!(
        "suggested {} more {} truncated {} in {elapsed:?}",
        found.suggested.len(),
        found.more.len(),
        found.truncated
    );
}
