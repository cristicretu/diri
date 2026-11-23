//! Two Macs syncing one profile through a shared folder and through a
//! local bare Git repository. Fixture directories only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use diri_portable::sync::{self, LocalProfile, Snapshot, SyncInput};
use serde_json::{Value, json};

struct Mac {
    root: PathBuf,
    name: &'static str,
}

impl Mac {
    fn new(base: &Path, name: &'static str) -> Self {
        let root = base.join(name);
        fs::create_dir_all(root.join("notes")).unwrap();
        Self { root, name }
    }

    fn profile(&self) -> LocalProfile {
        LocalProfile {
            notes_dir: self.root.join("notes"),
            app_skills_dir: self.root.join("app-skills"),
            init_lua: Some(self.root.join("init.lua")),
            overrides_dir: self.root.join("overrides"),
        }
    }

    fn note(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.root.join("notes").join(name)).ok()
    }

    fn set_note(&self, name: &str, text: &str) {
        fs::write(self.root.join("notes").join(name), text).unwrap();
    }

    fn base(&self) -> PathBuf {
        self.root.join("sync-base.json")
    }

    fn sync_folder(&self, folder: &Path, docs: Snapshot) -> sync::SyncOutcome {
        let profile = self.profile();
        let base = self.base();
        sync::sync_folder(
            &SyncInput {
                profile: &profile,
                docs,
                base_path: &base,
                device: self.name,
                stamp: self.name,
            },
            folder,
        )
        .unwrap()
    }

    fn sync_git(&self, location: &str, docs: Snapshot) -> sync::SyncOutcome {
        let profile = self.profile();
        let base = self.base();
        sync::sync_git(
            &SyncInput {
                profile: &profile,
                docs,
                base_path: &base,
                device: self.name,
                stamp: self.name,
            },
            location,
            &self.root.join("clone"),
        )
        .unwrap()
    }
}

fn prefs(value: Value) -> Snapshot {
    [("prefs.json".to_owned(), serde_json::to_vec(&value).unwrap())].into()
}

#[test]
fn folder_sync_moves_edits_both_ways_and_keeps_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("iCloud/Diri");
    let a = Mac::new(temp.path(), "mac-a");
    let b = Mac::new(temp.path(), "mac-b");
    a.set_note("plan.md", "# Plan\n");
    fs::write(a.root.join("init.lua"), "-- a\n").unwrap();

    let first = a.sync_folder(&folder, prefs(json!({"terminalTheme": "Dusk"})));
    assert!(first.summary.pushed >= 3);
    let second = b.sync_folder(&folder, prefs(json!({"reduceMotion": true})));
    assert_eq!(b.note("plan.md").as_deref(), Some("# Plan\n"));
    assert_eq!(
        fs::read_to_string(b.root.join("init.lua")).unwrap(),
        "-- a\n"
    );
    // Preferences merge per key and come back for the Engine to apply.
    assert_eq!(
        second.docs["prefs.json"],
        json!({"terminalTheme": "Dusk", "reduceMotion": true})
    );
    assert!(second.summary.prefs_pending);

    // Different notes edited on each Mac both survive.
    a.set_note("a-only.md", "from a\n");
    b.set_note("plan.md", "# Plan\n\nedited on b\n");
    let merged = json!({"terminalTheme": "Dusk", "reduceMotion": true});
    b.sync_folder(&folder, prefs(merged.clone()));
    a.sync_folder(&folder, prefs(merged.clone()));
    b.sync_folder(&folder, prefs(merged.clone()));
    assert_eq!(
        a.note("plan.md").as_deref(),
        Some("# Plan\n\nedited on b\n")
    );
    assert_eq!(b.note("a-only.md").as_deref(), Some("from a\n"));

    // The same note edited on both: each Mac keeps its own, the other copy
    // is saved under conflicts/ in the folder.
    a.set_note("plan.md", "A wins here\n");
    b.set_note("plan.md", "B wins here\n");
    a.sync_folder(&folder, prefs(merged.clone()));
    let outcome = b.sync_folder(&folder, prefs(merged.clone()));
    assert_eq!(outcome.summary.conflicts, vec!["notes/plan.md".to_owned()]);
    assert_eq!(b.note("plan.md").as_deref(), Some("B wins here\n"));
    assert_eq!(
        fs::read_to_string(folder.join("conflicts/notes/plan.md.mac-b")).unwrap(),
        "A wins here\n"
    );

    // A deletion propagates, into the notes trash.
    a.sync_folder(&folder, prefs(merged.clone()));
    fs::remove_file(a.root.join("notes/a-only.md")).unwrap();
    a.sync_folder(&folder, prefs(merged.clone()));
    let outcome = b.sync_folder(&folder, prefs(merged.clone()));
    assert_eq!(outcome.summary.deleted, 1);
    assert!(b.note("a-only.md").is_none());
    assert!(b.root.join("notes/.trash/a-only.md").is_file());

    // Settled: another round moves nothing.
    a.sync_folder(&folder, prefs(merged.clone()));
    let quiet = b.sync_folder(&folder, prefs(merged));
    assert_eq!((quiet.summary.pulled, quiet.summary.pushed), (0, 0));
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn git_sync_commits_once_per_change_and_merges_concurrent_edits() {
    if !git_available() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("profile.git");
    let status = Command::new("git")
        .args(["init", "--quiet", "--bare", "--initial-branch=main"])
        .arg(&remote)
        .status()
        .unwrap();
    assert!(status.success());
    let location = remote.to_string_lossy().into_owned();
    let a = Mac::new(temp.path(), "mac-a");
    let b = Mac::new(temp.path(), "mac-b");
    a.set_note("plan.md", "# Plan\n");

    let first = a.sync_git(&location, prefs(json!({"terminalTheme": "Dusk"})));
    assert!(first.summary.commit.is_some());
    let pulled = b.sync_git(&location, prefs(json!({"terminalTheme": "Dusk"})));
    assert_eq!(b.note("plan.md").as_deref(), Some("# Plan\n"));
    assert!(pulled.summary.commit.is_none(), "nothing new to commit");

    // Both Macs edit different notes before either syncs.
    a.set_note("a.md", "a\n");
    b.set_note("b.md", "b\n");
    let theme = prefs(json!({"terminalTheme": "Dusk"}));
    a.sync_git(&location, theme.clone());
    b.sync_git(&location, theme.clone());
    a.sync_git(&location, theme.clone());
    assert_eq!(a.note("b.md").as_deref(), Some("b\n"));
    assert_eq!(b.note("a.md").as_deref(), Some("a\n"));

    // An unchanged profile makes no commit.
    let before = git_log_len(&a.root.join("clone"));
    let quiet = a.sync_git(&location, theme);
    assert!(quiet.summary.commit.is_none());
    assert_eq!(git_log_len(&a.root.join("clone")), before);

    // The repository holds only profile files.
    let files = Command::new("git")
        .current_dir(a.root.join("clone"))
        .args(["ls-files"])
        .output()
        .unwrap();
    let files = String::from_utf8(files.stdout).unwrap();
    for line in files.lines() {
        assert!(
            line == "diri-profile.json" || line == "prefs.json" || line.starts_with("notes/"),
            "unexpected file in the repository: {line}"
        );
    }
}

fn git_log_len(clone: &Path) -> usize {
    let out = Command::new("git")
        .current_dir(clone)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
