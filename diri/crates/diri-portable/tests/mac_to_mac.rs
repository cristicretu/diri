//! Mac A (`/Users/alice/work/repo`) -> Mac B (`/Users/bob/code/repo`), with
//! real-shaped Claude Code and Codex transcripts, notes, App Skills,
//! recipes, schedules and planted secrets. Fixture homes only; no real
//! `~`, no network.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use diri_portable::bundle::KdfParams;
use diri_portable::docs::{self, LocalProject};
use diri_portable::migrate::{self, ExportInput, ImportTarget};
use diri_portable::pathmap::claude_project_slug;
use diri_portable::sync::{LocalProfile, Snapshot};
use diri_proto::portable::{PathMapping, PlannedAction, ProjectMatch};
use diri_proto::schedules::ScheduleRecord;
use diri_proto::{Resumability, SessionRecord, SessionStatus};
use serde_json::{Value, json};

const CLAUDE_ID: &str = "7f3c2a10-1111-4abc-8def-0123456789ab";
const CODEX_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const SECRETS: [&str; 6] = [
    "sk-ant-oat01-PLANTED",
    "PLANTED-OAUTH-REFRESH",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
    "PLANTED-PHONE-TOKEN",
    "PLANTED-MCP-KEY",
    "PLANTED-ENV-SECRET",
];

struct Mac {
    _temp: tempfile::TempDir,
    home: PathBuf,
}

impl Mac {
    fn new(user: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("Users").join(user);
        fs::create_dir_all(&home).unwrap();
        Self { _temp: temp, home }
    }

    fn support(&self) -> PathBuf {
        self.home.join("Library/Application Support/Dirijor")
    }

    fn profile(&self) -> LocalProfile {
        LocalProfile {
            notes_dir: self.support().join("notes"),
            app_skills_dir: self.support().join("computer-use/app-skills"),
            init_lua: Some(self.home.join(".config/diri/init.lua")),
            overrides_dir: self.support().join("manifests/overrides"),
        }
    }

    fn path(&self, rel: &str) -> String {
        self.home.join(rel).to_string_lossy().into_owned()
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn checkout(&self, rel: &str, origin: &str) {
        self.write(
            &format!("{rel}/.git/config"),
            &format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {origin}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"),
        );
        self.write(&format!("{rel}/README.md"), "# repo\n");
    }
}

fn record(value: Value) -> SessionRecord {
    let mut base = json!({
        "kind": {"claudeCode": {}},
        "projectID": "p_old",
        "titleSource": 3,
        "status": {"idle": {}},
        "resumability": "resumable",
        "createdAt": 1_790_000_000_000.0,
        "updatedAt": 1_790_000_500_000.0,
        "pinned": false,
    });
    base.as_object_mut()
        .unwrap()
        .extend(value.as_object().unwrap().clone());
    serde_json::from_value(base).unwrap()
}

/// Mac A: alice's projects, sessions, transcripts, profile and secrets.
fn mac_a() -> (Mac, Vec<SessionRecord>, Vec<LocalProject>, Snapshot) {
    let a = Mac::new("alice");
    a.checkout("work/repo", "git@github.com:acme/repo.git");
    a.checkout("work/site", "https://github.com/acme/site.git");
    let repo = a.path("work/repo");
    let worktree = a.path("work/repo-feature");
    a.checkout("work/repo-feature", "git@github.com:acme/repo.git");

    // A Claude Code conversation in the main checkout.
    let claude_dir = a
        .home
        .join(".claude/projects")
        .join(claude_project_slug(&repo));
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_lines = [
        json!({"type": "user", "cwd": repo, "sessionId": CLAUDE_ID, "message": {"role": "user", "content": format!("fix the bug in {repo}/src/lib.rs")}}),
        json!({"type": "assistant", "cwd": repo, "sessionId": CLAUDE_ID, "message": {"role": "assistant", "content": "Done."}}),
    ];
    let claude_text: String = claude_lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(claude_dir.join(format!("{CLAUDE_ID}.jsonl")), &claude_text).unwrap();

    // A Codex rollout in the linked worktree.
    let codex_dir = a.home.join(".codex/sessions/2026/11/01");
    fs::create_dir_all(&codex_dir).unwrap();
    let rollout = codex_dir.join(format!("rollout-2026-11-01T10-00-00-{CODEX_ID}.jsonl"));
    fs::write(
        &rollout,
        format!(
            "{}\n{}\n",
            json!({"timestamp": "2026-11-01T10:00:00Z", "type": "session_meta", "payload": {"id": CODEX_ID, "cwd": worktree, "originator": "codex_cli_rs"}}),
            json!({"type": "turn_context", "payload": {"cwd": worktree, "model": "gpt-5"}}),
        ),
    )
    .unwrap();

    // Secrets that must never travel.
    a.write(".claude/.credentials.json", r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-PLANTED","refreshToken":"PLANTED-OAUTH-REFRESH"}}"#);
    a.write(
        ".codex/auth.json",
        r#"{"OPENAI_API_KEY":"sk-ant-oat01-PLANTED"}"#,
    );
    a.write(
        ".ssh/id_ed25519",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n",
    );
    a.write(
        "Library/Application Support/Dirijor/accounts.json",
        r#"{"profiles":[{"token":"PLANTED-OAUTH-REFRESH"}]}"#,
    );
    a.write(
        "Library/Application Support/Dirijor/phone-access.token",
        "PLANTED-PHONE-TOKEN",
    );
    a.write(
        "Library/Application Support/Dirijor/mcp-http.key",
        "PLANTED-MCP-KEY",
    );
    a.write("Library/Application Support/Dirijor/daemon.sock", "");
    a.write("Library/Application Support/Dirijor/logs/s_1.bin", "swap");

    // Profile files.
    a.write(
        "Library/Application Support/Dirijor/notes/20261101-090000-ab12.md",
        "# Plan\n\nShip the migration.\n",
    );
    a.write(
        "Library/Application Support/Dirijor/notes/20261102-090000-cd34.md",
        "# Same name on both\n\nalice's version\n",
    );
    a.write(
        "Library/Application Support/Dirijor/notes/.trash/old.md",
        "deleted",
    );
    a.write(
        "Library/Application Support/Dirijor/computer-use/app-skills/com.apple.Notes/app.json",
        r#"{"bundle_id":"com.apple.Notes","name":"Notes"}"#,
    );
    a.write(
        "Library/Application Support/Dirijor/computer-use/app-skills/com.apple.Notes/skills/new-note.json",
        r#"{"name":"new-note","version":2,"steps":[]}"#,
    );
    a.write(
        "Library/Application Support/Dirijor/computer-use/app-skills/com.apple.Notes/skills/new-note.v1.json",
        r#"{"name":"new-note","version":1,"steps":[]}"#,
    );
    a.write(
        "Library/Application Support/Dirijor/computer-use/app-skills/com.apple.Notes/runs.jsonl",
        "{\"skill\":\"new-note\"}\n",
    );
    a.write(
        ".config/diri/init.lua",
        "diri.on('ready', function() end)\n",
    );
    a.write(
        "Library/Application Support/Dirijor/manifests/overrides/claude-code.json",
        r#"{"id":"claude-code","launch":{"env":{"ANTHROPIC_API_KEY":"PLANTED-ENV-SECRET","CLAUDE_CODE_NO_FLICKER":"1"}}}"#,
    );

    let sessions = vec![
        record(json!({
            "id": "s_claude", "cwd": repo, "title": "Fix the parser", "projectID": "p_repo",
            "agentSessionID": CLAUDE_ID, "pinned": true, "gitBranch": "main",
            "transcriptPath": claude_dir.join(format!("{CLAUDE_ID}.jsonl")),
            "memoryBytes": 1234,
            "accountProfile": {"id": "work", "label": "Work", "agent": "claude-code", "configHome": a.path(".claude-work")},
        })),
        record(json!({
            "id": "s_codex", "kind": {"codex": {}}, "cwd": worktree, "worktreePath": worktree,
            "title": "Review the parser fix", "agentSessionID": CODEX_ID, "parent": "s_claude",
            "projectID": "p_repo",
        })),
        record(
            json!({"id": "s_shell", "kind": {"shell": {}}, "cwd": a.path("work/site"), "title": "site"}),
        ),
        record(
            json!({"id": "s_remote", "cwd": "/home/alice/x", "title": "On forge", "host": "forge"}),
        ),
    ];
    let projects = vec![
        LocalProject {
            id: "p_repo".into(),
            root: repo.clone(),
            name: "repo".into(),
        },
        LocalProject {
            id: "p_site".into(),
            root: a.path("work/site"),
            name: "site".into(),
        },
    ];
    let home = a.home.to_string_lossy().into_owned();
    let prefs = json!({
        "terminalTheme": "Dusk",
        "windowPlacement": {"x": 10},
        "phoneTailnetIdentity": true,
        "talkVoiceCommand": "curl -H 'Authorization: Bearer PLANTED-ENV-SECRET'",
        "sidebarPinnedProjects": ["p_repo"],
        "sidebarPinnedSessions": ["s_claude"],
        "conversationTitles": {CLAUDE_ID: "Parser fix"},
        "launchRecipes": {"version": 1, "nextId": 2, "items": [{
            "id": "recipe-1", "name": "Review PR", "agent": {"claudeCode": {}},
            "project": {"kind": "tracked", "id": "p_repo", "last_known_root": repo},
            "worktree": {"kind": "fresh"}, "initialPrompt": "Review the open PR"
        }]},
    });
    let (prefs_doc, recipes_doc, _) =
        docs::export_prefs(&home, prefs.as_object().unwrap(), &projects);
    let schedule: ScheduleRecord = serde_json::from_value(json!({
        "id": "sch_1", "title": "Morning review", "when": {"kind": "cron", "expr": "0 9 * * 1-5"},
        "spawn": {"kind": {"claudeCode": {}}, "cwd": repo, "initialPrompt": "review", "accountProfileId": "work"},
        "createdAt": 1.0, "revision": 1,
    }))
    .unwrap();
    let (schedules_doc, _) = docs::export_schedules(&home, &[schedule]);
    let mut snapshot = Snapshot::new();
    snapshot.insert("prefs.json".into(), serde_json::to_vec(&prefs_doc).unwrap());
    snapshot.insert(
        "recipes.json".into(),
        serde_json::to_vec(&recipes_doc).unwrap(),
    );
    snapshot.insert(
        "schedules.json".into(),
        serde_json::to_vec(&schedules_doc).unwrap(),
    );
    snapshot.insert(
        "projects.json".into(),
        serde_json::to_vec(&docs::export_projects(&home, &projects)).unwrap(),
    );
    (a, sessions, projects, snapshot)
}

fn export(
    a: &Mac,
    sessions: &[SessionRecord],
    projects: &[LocalProject],
    docs: &Snapshot,
    out: &Path,
    passphrase: Option<&str>,
) -> diri_proto::portable::MigrateExportResult {
    let profile = a.profile();
    migrate::export(
        ExportInput {
            home: &a.home,
            device: "Alice's MacBook Pro",
            diri_version: "0.12.0",
            profile: &profile,
            docs: docs.clone(),
            sessions: sessions.to_vec(),
            projects: projects.to_vec(),
            skip_transcripts: false,
            kdf: KdfParams::TEST,
        },
        out,
        passphrase,
    )
    .unwrap()
}

fn project_id(root: &str) -> String {
    format!(
        "p_{}",
        &diri_portable::files::sha256_hex(root.as_bytes())[..12]
    )
}

fn import(
    b: &Mac,
    bundle: &Path,
    passphrase: Option<&str>,
    maps: &[PathMapping],
    existing: &[SessionRecord],
    dry_run: bool,
) -> migrate::ImportOutput {
    let profile = b.profile();
    migrate::import(
        bundle,
        passphrase,
        maps,
        dry_run,
        ImportTarget {
            home: &b.home,
            profile: &profile,
            existing: existing.to_vec(),
            live: HashSet::new(),
            local_projects: Vec::new(),
            schedules: Vec::new(),
            prefs: serde_json::Map::new(),
            project_id: &project_id,
            scan_roots: Vec::new(),
        },
    )
    .unwrap()
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    out
}

/// Mac B: bob keeps the repo in `~/code/repo` and the site where alice did.
fn mac_b() -> Mac {
    let b = Mac::new("bob");
    b.checkout("code/repo", "https://github.com/acme/repo");
    b.checkout("work/site", "https://github.com/acme/site.git");
    // A note bob already has under the same name, with different text.
    b.write(
        "Library/Application Support/Dirijor/notes/20261102-090000-cd34.md",
        "# Same name on both\n\nbob's version\n",
    );
    b
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn a_plain_bundle_carries_no_secret() {
    let (a, sessions, projects, docs) = mac_a();
    let out = a.home.join("out.diribundle");
    let result = export(&a, &sessions, &projects, &docs, &out, None);
    assert!(!result.encrypted);
    assert_eq!(result.counts.sessions, 3, "the remote session stays behind");
    assert_eq!(result.counts.transcripts, 2);
    assert_eq!(result.counts.notes, 2);
    assert_eq!(result.counts.skills, 1);
    assert_eq!(result.counts.recipes, 1);
    assert_eq!(result.counts.schedules, 1);
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("remote host"))
    );
    let bytes = fs::read(&out).unwrap();
    for secret in SECRETS {
        assert!(!contains(&bytes, secret), "{secret} leaked into the bundle");
    }
    for leaked in [
        "accountProfile",
        "configHome",
        "memoryBytes",
        "runs.jsonl",
        ".trash",
        "windowPlacement",
        "phoneTailnetIdentity",
        "daemon.sock",
        "accounts.json",
    ] {
        assert!(!contains(&bytes, leaked), "{leaked} leaked into the bundle");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&out).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn alice_to_bob_resumes_every_conversation_and_imports_once() {
    let (a, sessions, projects, docs) = mac_a();
    let bundle = a.home.join("alice.diribundle");
    let exported = export(
        &a,
        &sessions,
        &projects,
        &docs,
        &bundle,
        Some("hunter2 correct horse"),
    );
    assert!(exported.encrypted);
    let sealed = fs::read(&bundle).unwrap();
    assert!(!contains(&sealed, "Ship the migration"));
    let b = mac_b();
    let bob = |rel: &str| b.path(rel);

    // Dry run: a plan, and nothing on Mac B changes.
    let before = tree(&b.home);
    let plan = import(&b, &bundle, Some("hunter2 correct horse"), &[], &[], true).plan;
    assert_eq!(tree(&b.home), before, "a dry run wrote something");
    assert!(plan.dry_run && !plan.prefs_pending);
    assert_eq!(plan.source.device, "Alice's MacBook Pro");
    assert_eq!(plan.source.home, a.home.to_string_lossy());
    let by_from: BTreeMap<_, _> = plan.projects.iter().map(|p| (p.from.as_str(), p)).collect();
    let repo = by_from["~/work/repo"];
    assert_eq!(repo.how, ProjectMatch::Origin);
    assert_eq!(repo.to, bob("code/repo"));
    assert_eq!(repo.git_origin.as_deref(), Some("github.com/acme/repo"));
    assert_eq!(by_from["~/work/site"].how, ProjectMatch::Same);
    assert_eq!(plan.added.sessions, 3);
    assert_eq!(plan.added.transcripts, 2);
    assert_eq!(plan.added.notes, 1);
    assert_eq!(
        plan.conflicts,
        vec!["notes/20261102-090000-cd34.md".to_owned()]
    );

    // Wrong passphrase fails closed.
    let profile = b.profile();
    let wrong = migrate::import(
        &bundle,
        Some("nope"),
        &[],
        true,
        ImportTarget {
            home: &b.home,
            profile: &profile,
            existing: vec![],
            live: HashSet::new(),
            local_projects: vec![],
            schedules: vec![],
            prefs: serde_json::Map::new(),
            project_id: &project_id,
            scan_roots: vec![],
        },
    );
    assert_eq!(wrong.err().unwrap().code(), "bundle_wrong_passphrase");

    // Import for real.
    let output = import(&b, &bundle, Some("hunter2 correct horse"), &[], &[], false);
    let plan = &output.plan;
    assert_eq!(plan.added.sessions, 3);
    assert_eq!(plan.added.transcripts, 2);
    assert!(plan.prefs_pending);
    let records: BTreeMap<_, _> = output
        .sessions
        .iter()
        .map(|r| (r.id.0.as_str(), r))
        .collect();

    // Claude: the record and the transcript follow the repo to ~/code/repo.
    let claude = records["s_claude"];
    assert_eq!(claude.cwd, bob("code/repo"));
    assert_eq!(claude.agent_session_id.as_deref(), Some(CLAUDE_ID));
    assert!(claude.pinned);
    assert!(claude.account_profile.is_none());
    assert!(claude.memory_bytes.is_none());
    assert_eq!(claude.resumability, Resumability::Resumable);
    assert!(matches!(&claude.status, SessionStatus::Exited(_)));
    assert_eq!(claude.project_id.0, project_id(&bob("code/repo")));
    let claude_path = b
        .home
        .join(".claude/projects")
        .join(claude_project_slug(&bob("code/repo")))
        .join(format!("{CLAUDE_ID}.jsonl"));
    assert_eq!(
        claude.transcript_path.as_deref(),
        Some(claude_path.to_str().unwrap())
    );
    let text = fs::read_to_string(&claude_path).unwrap();
    for line in text.lines() {
        let line: Value = serde_json::from_str(line).unwrap();
        assert_eq!(line["cwd"], bob("code/repo"));
        assert_eq!(line["sessionId"], CLAUDE_ID);
    }
    // Message content is the user's history: untouched.
    assert!(text.contains(&format!(
        "fix the bug in {}/src/lib.rs",
        a.path("work/repo")
    )));

    // Codex: the worktree does not exist on Mac B, so it resumes in the repo.
    let codex = records["s_codex"];
    assert_eq!(
        codex.parent.as_ref().map(|p| p.0.as_str()),
        Some("s_claude"),
        "lineage survives"
    );
    assert_eq!(codex.cwd, bob("code/repo"));
    assert!(codex.worktree_path.is_none());
    let rollout = b.home.join(format!(
        ".codex/sessions/2026/11/01/rollout-2026-11-01T10-00-00-{CODEX_ID}.jsonl"
    ));
    let rollout_text = fs::read_to_string(&rollout).unwrap();
    let meta: Value = serde_json::from_str(rollout_text.lines().next().unwrap()).unwrap();
    assert_eq!(meta["payload"]["cwd"], bob("code/repo"));
    assert_eq!(meta["payload"]["id"], CODEX_ID);
    assert_eq!(codex.resumability, Resumability::Resumable);

    // The shell session keeps its folder.
    assert_eq!(records["s_shell"].cwd, bob("work/site"));
    assert!(!records.contains_key("s_remote"));

    // Profile: notes and skills arrive, runs log and trash do not; bob's
    // own note is kept.
    let notes = b.home.join("Library/Application Support/Dirijor/notes");
    assert_eq!(
        fs::read_to_string(notes.join("20261101-090000-ab12.md")).unwrap(),
        "# Plan\n\nShip the migration.\n"
    );
    assert!(
        fs::read_to_string(notes.join("20261102-090000-cd34.md"))
            .unwrap()
            .contains("bob's version")
    );
    assert!(!notes.join(".trash").exists());
    let skills = b
        .home
        .join("Library/Application Support/Dirijor/computer-use/app-skills/com.apple.Notes");
    assert!(skills.join("skills/new-note.json").is_file());
    assert!(skills.join("skills/new-note.v1.json").is_file());
    assert!(!skills.join("runs.jsonl").exists());
    assert!(b.home.join(".config/diri/init.lua").is_file());
    let override_text = fs::read_to_string(
        b.home
            .join("Library/Application Support/Dirijor/manifests/overrides/claude-code.json"),
    )
    .unwrap();
    assert!(override_text.contains("CLAUDE_CODE_NO_FLICKER"));
    assert!(!override_text.contains("PLANTED"));

    // Preferences: portable keys only, project ids resolved on Mac B.
    let prefs = output.pending_prefs.as_ref().unwrap();
    assert_eq!(prefs["terminalTheme"], "Dusk");
    assert!(!prefs.contains_key("windowPlacement"));
    assert!(!prefs.contains_key("talkVoiceCommand"));
    assert_eq!(
        prefs["sidebarPinnedProjects"],
        json!([project_id(&bob("code/repo"))])
    );
    assert_eq!(prefs["conversationTitles"][CLAUDE_ID], "Parser fix");
    let recipe = &prefs["launchRecipes"]["items"][0];
    assert_eq!(recipe["project"]["last_known_root"], bob("code/repo"));
    assert_eq!(output.schedules.len(), 1);
    assert_eq!(output.schedules[0].spec.spawn.cwd, bob("code/repo"));
    assert!(output.schedules[0].spec.spawn.account_profile_id.is_none());

    // No secret arrived anywhere on Mac B.
    for (path, bytes) in tree(&b.home) {
        for secret in SECRETS {
            assert!(!contains(&bytes, secret), "{secret} reached {path}");
        }
        assert!(
            !path.contains("credentials")
                && !path.ends_with(".sock")
                && !path.contains("auth.json"),
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&claude_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(claude_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    // Importing the same bundle again changes nothing.
    let after_first = tree(&b.home);
    let again = import(
        &b,
        &bundle,
        Some("hunter2 correct horse"),
        &[],
        &output.sessions,
        false,
    );
    assert_eq!(tree(&b.home), after_first);
    assert_eq!(again.plan.added.sessions, 0);
    assert_eq!(again.plan.added.transcripts, 0);
    assert_eq!(again.plan.updated, 0);
    assert!(again.sessions.is_empty());
    assert!(
        again
            .plan
            .sessions
            .iter()
            .all(|s| s.action == PlannedAction::Keep)
    );

    // A newer local copy is never overwritten by an older exported one.
    let mut newer = output.sessions.clone();
    newer[0].title = "Renamed on Mac B".into();
    newer[0].updated_at.0 += 10_000.0;
    let third = import(
        &b,
        &bundle,
        Some("hunter2 correct horse"),
        &[],
        &newer,
        false,
    );
    assert!(third.sessions.is_empty());
}

#[test]
fn an_unfound_project_can_be_mapped_by_hand() {
    let (a, sessions, projects, docs) = mac_a();
    let bundle = a.home.join("plain.diribundle");
    export(&a, &sessions, &projects, &docs, &bundle, None);
    let b = Mac::new("bob");
    b.checkout("elsewhere/repo-copy", "git@gitlab.com:other/unrelated.git");
    let plan = import(&b, &bundle, None, &[], &[], true).plan;
    let repo = plan
        .projects
        .iter()
        .find(|p| p.from == "~/work/repo")
        .unwrap();
    assert_eq!(repo.how, ProjectMatch::Unresolved);
    assert_eq!(repo.to, b.path("work/repo"), "home remap is the fallback");
    let maps = [PathMapping {
        from: "~/work/repo".into(),
        to: "~/elsewhere/repo-copy".into(),
    }];
    let output = import(&b, &bundle, None, &maps, &[], false);
    let repo = output
        .plan
        .projects
        .iter()
        .find(|p| p.from == "~/work/repo")
        .unwrap();
    assert_eq!(repo.how, ProjectMatch::Mapped);
    let claude = output
        .sessions
        .iter()
        .find(|r| r.id.0 == "s_claude")
        .unwrap();
    assert_eq!(claude.cwd, b.path("elsewhere/repo-copy"));
    assert!(
        b.home
            .join(".claude/projects")
            .join(claude_project_slug(&b.path("elsewhere/repo-copy")))
            .join(format!("{CLAUDE_ID}.jsonl"))
            .is_file()
    );
}
