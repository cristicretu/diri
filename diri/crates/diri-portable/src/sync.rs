//! Two-way profile sync with a folder or a Git repository. No server: the
//! folder is whatever iCloud Drive, Syncthing or Dropbox keeps in step, and
//! a Git remote is any private repository. Diri merges per file against the
//! last synced state (three-way); Git is only the transport.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use diri_proto::portable::{SkippedItem, SyncSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::classify::{
    ArtifactClass, classify_profile_path, looks_like_secret_text, profile_allowed,
    redact_json_bytes,
};
use crate::files::{confined, read_bounded, sha256_hex, write_private};
use crate::merge::{doc_kind, merge3};

/// Profile path -> bytes.
pub type Snapshot = BTreeMap<String, Vec<u8>>;

/// Largest single profile file (a note, a skill).
const MAX_PROFILE_FILE: u64 = 16 * 1024 * 1024;
/// Profile documents the Engine produces and applies itself.
pub const DOCS: [&str; 4] = [
    "prefs.json",
    "recipes.json",
    "schedules.json",
    "projects.json",
];

/// Where this Mac keeps the file-backed parts of the profile.
#[derive(Clone, Debug)]
pub struct LocalProfile {
    pub notes_dir: PathBuf,
    pub app_skills_dir: PathBuf,
    pub init_lua: Option<PathBuf>,
    pub overrides_dir: PathBuf,
}

impl LocalProfile {
    fn local_path(&self, rel: &str) -> Option<PathBuf> {
        let class = classify_profile_path(rel)?;
        let (root, rest) = match class {
            ArtifactClass::Note => (&self.notes_dir, rel.strip_prefix("notes/")?),
            ArtifactClass::AppSkill => (&self.app_skills_dir, rel.strip_prefix("app-skills/")?),
            ArtifactClass::ManifestOverride => (
                &self.overrides_dir,
                rel.strip_prefix("manifests/overrides/")?,
            ),
            ArtifactClass::InitLua => return self.init_lua.clone(),
            _ => return None,
        };
        confined(root, rest).ok()
    }

    /// The file-backed profile as it is on this Mac. JSON files pass the
    /// secret-field redactor; a JSON file that does not parse is skipped.
    pub fn collect(&self) -> io::Result<(Snapshot, Vec<SkippedItem>)> {
        let mut snapshot = Snapshot::new();
        let mut skipped = Vec::new();
        let mut add = |rel: String, path: &Path, snapshot: &mut Snapshot| {
            let Some(class) = classify_profile_path(&rel) else {
                return;
            };
            debug_assert!(profile_allowed(class));
            let Ok(bytes) = read_bounded(path, MAX_PROFILE_FILE) else {
                skipped.push(SkippedItem {
                    item: rel,
                    reason: "unreadable or too large".into(),
                });
                return;
            };
            let bytes = if rel.ends_with(".json") {
                match redact_json_bytes(&bytes) {
                    Some(bytes) => bytes,
                    None => {
                        skipped.push(SkippedItem {
                            item: rel,
                            reason: "not valid JSON".into(),
                        });
                        return;
                    }
                }
            } else {
                if looks_like_secret_text(&String::from_utf8_lossy(&bytes)) {
                    skipped.push(SkippedItem {
                        item: rel,
                        reason: "looks like it contains a private key or token".into(),
                    });
                    return;
                }
                bytes
            };
            snapshot.insert(rel, bytes);
        };
        for (name, path) in files_in(&self.notes_dir) {
            add(format!("notes/{name}"), &path, &mut snapshot);
        }
        for (bundle, dir) in dirs_in(&self.app_skills_dir) {
            add(
                format!("app-skills/{bundle}/app.json"),
                &dir.join("app.json"),
                &mut snapshot,
            );
            for (name, path) in files_in(&dir.join("skills")) {
                add(
                    format!("app-skills/{bundle}/skills/{name}"),
                    &path,
                    &mut snapshot,
                );
            }
        }
        if let Some(init) = &self.init_lua
            && init.is_file()
        {
            add("config/init.lua".into(), init, &mut snapshot);
        }
        for (name, path) in files_in(&self.overrides_dir) {
            add(format!("manifests/overrides/{name}"), &path, &mut snapshot);
        }
        Ok((snapshot, skipped))
    }

    /// Writes one file-backed profile path on this Mac.
    pub fn write(&self, rel: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self
            .local_path(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, rel.to_owned()))?;
        write_private(&path, bytes)
    }

    /// Removes one file-backed profile path. Notes go to the notes `.trash`
    /// so a sync can never destroy writing.
    pub fn remove(&self, rel: &str) -> io::Result<()> {
        let path = self
            .local_path(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, rel.to_owned()))?;
        if !path.exists() {
            return Ok(());
        }
        if rel.starts_with("notes/") {
            let trash = self.notes_dir.join(".trash");
            diri_platform::private::create_dir_all(&trash)?;
            let name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();
            return fs::rename(&path, trash.join(name));
        }
        fs::remove_file(path)
    }

    pub fn existing(&self, rel: &str) -> Option<PathBuf> {
        self.local_path(rel)
    }
}

fn files_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    out.sort();
    out
}

fn dirs_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    out.sort();
    out
}

/// Reads every profile file under a synced root. Anything outside the
/// profile layout (conflict copies, `.git`, foreign files) is ignored.
pub fn read_tree(root: &Path) -> io::Result<Snapshot> {
    let mut snapshot = Snapshot::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') || (prefix.is_empty() && name == "conflicts") {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() && rel.matches('/').count() < 4 {
                stack.push((entry.path(), rel));
            } else if kind.is_file() && classify_profile_path(&rel).is_some() {
                snapshot.insert(rel, read_bounded(&entry.path(), MAX_PROFILE_FILE)?);
            }
        }
    }
    Ok(snapshot)
}

/// The last synced state, kept on this Mac: one hash per path, plus the
/// documents' content for the per-record merge.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncBase {
    pub hashes: BTreeMap<String, String>,
    pub docs: BTreeMap<String, Value>,
}

impl SyncBase {
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_private(path, &serde_json::to_vec(self).map_err(io::Error::other)?)
    }
}

/// What one sync decides. `None` bytes delete.
#[derive(Debug, Default)]
pub struct SyncPlan {
    pub to_local: BTreeMap<String, Option<Vec<u8>>>,
    pub to_remote: BTreeMap<String, Option<Vec<u8>>>,
    /// Copies of the other side's version, written under `conflicts/`.
    pub conflict_copies: BTreeMap<String, Vec<u8>>,
    pub conflicts: Vec<String>,
    pub base: SyncBase,
}

/// Three-way plan between this Mac's profile, the synced copy and the last
/// synced state.
pub fn plan(local: &Snapshot, remote: &Snapshot, base: &SyncBase, stamp: &str) -> SyncPlan {
    let mut out = SyncPlan::default();
    let paths: BTreeSet<&String> = local
        .keys()
        .chain(remote.keys())
        .chain(base.hashes.keys())
        .collect();
    // Documents compare by content, not formatting: the Engine hands them
    // over compact, the synced copy holds them pretty-printed.
    let canonical = |rel: &str, bytes: Option<&Vec<u8>>| -> Option<Vec<u8>> {
        let bytes = bytes?;
        if doc_kind(rel).is_none() {
            return Some(bytes.clone());
        }
        Some(
            serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|value| serde_json::to_vec_pretty(&value).ok())
                .unwrap_or_else(|| bytes.clone()),
        )
    };
    for rel in paths {
        let l_canonical = canonical(rel, local.get(rel));
        let r_canonical = canonical(rel, remote.get(rel));
        let l = l_canonical.as_ref();
        let r = r_canonical.as_ref();
        let hl = l.map(|bytes| sha256_hex(bytes));
        let hr = r.map(|bytes| sha256_hex(bytes));
        let hb = base.hashes.get(rel).cloned();
        let merged: Option<Vec<u8>> = if hl == hr {
            l.cloned()
        } else if let Some(kind) = doc_kind(rel) {
            let parse = |bytes: Option<&Vec<u8>>| {
                bytes.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            };
            let (value, conflicts) = if base.hashes.is_empty() && r.is_some() {
                // This Mac's first sync with a copy that already has a
                // profile: a fresh Mac's defaults must not override the
                // settings it came to fetch. Both sides' members are kept;
                // the synced copy wins where they differ.
                let (value, _) = merge3(kind, None, parse(r).as_ref(), parse(l).as_ref());
                (value, Vec::new())
            } else {
                merge3(
                    kind,
                    base.docs.get(rel),
                    parse(l).as_ref(),
                    parse(r).as_ref(),
                )
            };
            out.conflicts.extend(
                conflicts
                    .into_iter()
                    .map(|member| format!("{rel}: {member}")),
            );
            Some(serde_json::to_vec_pretty(&value).unwrap_or_default())
        } else if hl == hb {
            r.cloned()
        } else if hr == hb {
            l.cloned()
        } else {
            // Both sides changed one file differently. Keep this Mac's; the
            // other side's version is saved beside it in the synced copy. A
            // deletion here never beats an edit there.
            match (l, r) {
                (Some(l), Some(r)) => {
                    out.conflicts.push(rel.clone());
                    out.conflict_copies
                        .insert(format!("conflicts/{rel}.{stamp}"), r.clone());
                    Some(l.clone())
                }
                (None, Some(r)) => Some(r.clone()),
                (Some(l), None) => Some(l.clone()),
                (None, None) => None,
            }
        };
        if merged.as_ref() != l {
            out.to_local.insert(rel.clone(), merged.clone());
        }
        if merged.as_ref() != r {
            out.to_remote.insert(rel.clone(), merged.clone());
        }
        if let Some(bytes) = &merged {
            out.base.hashes.insert(rel.clone(), sha256_hex(bytes));
            if doc_kind(rel).is_some()
                && let Ok(value) = serde_json::from_slice(bytes)
            {
                out.base.docs.insert(rel.clone(), value);
            }
        }
    }
    out
}

/// One finished sync.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub summary: SyncSummary,
    /// Documents (see [`DOCS`]) whose merged content differs from this
    /// Mac's: the Engine applies them.
    pub docs: BTreeMap<String, Value>,
}

/// Applies a plan: local files through `profile`, documents returned,
/// remote files under `root`.
fn apply(
    plan: &SyncPlan,
    profile: &LocalProfile,
    root: &Path,
    device: &str,
) -> io::Result<SyncOutcome> {
    let mut outcome = SyncOutcome::default();
    for (rel, bytes) in &plan.to_local {
        if DOCS.contains(&rel.as_str()) {
            if let Some(bytes) = bytes
                && let Ok(value) = serde_json::from_slice(bytes)
            {
                outcome.docs.insert(rel.clone(), value);
                outcome.summary.pulled += 1;
            }
            continue;
        }
        match bytes {
            Some(bytes) => {
                profile.write(rel, bytes)?;
                outcome.summary.pulled += 1;
            }
            None => {
                profile.remove(rel)?;
                outcome.summary.deleted += 1;
            }
        }
    }
    let mut wrote_remote = false;
    for (rel, bytes) in &plan.to_remote {
        let path = confined(root, rel)?;
        match bytes {
            Some(bytes) => {
                write_private(&path, bytes)?;
                outcome.summary.pushed += 1;
            }
            None => {
                if path.exists() {
                    fs::remove_file(&path)?;
                    outcome.summary.deleted += 1;
                }
            }
        }
        wrote_remote = true;
    }
    for (rel, bytes) in &plan.conflict_copies {
        write_private(&confined(root, rel)?, bytes)?;
        wrote_remote = true;
    }
    if wrote_remote {
        let marker = serde_json::json!({ "format": 1, "device": device });
        write_private(
            &root.join("diri-profile.json"),
            &serde_json::to_vec_pretty(&marker).unwrap_or_default(),
        )?;
    }
    outcome.summary.conflicts = plan.conflicts.clone();
    outcome.summary.prefs_pending =
        outcome.docs.contains_key("prefs.json") || outcome.docs.contains_key("recipes.json");
    Ok(outcome)
}

/// What a sync reads from this Mac: the file-backed profile plus the
/// Engine's documents in portable form.
pub struct SyncInput<'a> {
    pub profile: &'a LocalProfile,
    pub docs: Snapshot,
    pub base_path: &'a Path,
    pub device: &'a str,
    /// Conflict copy suffix, e.g. `alice-mbp-20261123T101500`.
    pub stamp: &'a str,
}

fn local_snapshot(input: &SyncInput<'_>) -> io::Result<Snapshot> {
    let (mut local, _skipped) = input.profile.collect()?;
    for (rel, bytes) in &input.docs {
        if DOCS.contains(&rel.as_str()) {
            local.insert(rel.clone(), bytes.clone());
        }
    }
    Ok(local)
}

/// Syncs with a folder another tool keeps in step.
pub fn sync_folder(input: &SyncInput<'_>, root: &Path) -> io::Result<SyncOutcome> {
    diri_platform::private::create_dir_all(root)?;
    let local = local_snapshot(input)?;
    let remote = read_tree(root)?;
    let base = SyncBase::load(input.base_path);
    let plan = plan(&local, &remote, &base, input.stamp);
    let outcome = apply(&plan, input.profile, root, input.device)?;
    plan.base.save(input.base_path)?;
    Ok(outcome)
}

/// Runs `git` in `dir` with fixed arguments, never prompting.
fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "core.sshCommand=ssh -o BatchMode=yes",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git failed");
        Err(io::Error::other(format!(
            "git {}: {}",
            args[0],
            first.trim()
        )))
    }
}

/// The upstream branch to follow, if the remote has any commits.
fn upstream_branch(clone: &Path) -> Option<String> {
    if let Ok(head) = git(
        clone,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return head.strip_prefix("origin/").map(str::to_owned);
    }
    ["main", "master"]
        .into_iter()
        .find(|branch| {
            git(
                clone,
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/remotes/origin/{branch}"),
                ],
            )
            .is_ok()
        })
        .map(str::to_owned)
}

/// Syncs through a Git repository. `clone` is Diri's private working copy
/// of `location`; the profile is merged into it, committed once, pushed.
/// A rejected push (another Mac pushed first) retries once from fetch. The
/// base advances only after a push lands.
pub fn sync_git(input: &SyncInput<'_>, location: &str, clone: &Path) -> io::Result<SyncOutcome> {
    if location.trim().is_empty() || location.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Choose a Git repository",
        ));
    }
    if !clone.join(".git").is_dir() {
        if clone.exists() {
            fs::remove_dir_all(clone)?;
        }
        let parent = clone
            .parent()
            .ok_or_else(|| io::Error::other("clone has no parent"))?;
        diri_platform::private::create_dir_all(parent)?;
        let target = clone.to_string_lossy().into_owned();
        git(parent, &["clone", "--quiet", "--", location, &target])?;
    }
    let mut total = SyncOutcome::default();
    for attempt in 0..2 {
        git(clone, &["fetch", "--quiet", "--prune", "origin"])?;
        let branch = upstream_branch(clone);
        match &branch {
            Some(branch) => {
                git(
                    clone,
                    &[
                        "checkout",
                        "--quiet",
                        "-B",
                        branch,
                        &format!("origin/{branch}"),
                    ],
                )?;
                git(
                    clone,
                    &["reset", "--quiet", "--hard", &format!("origin/{branch}")],
                )?;
            }
            None => {
                // An empty repository: start `main`.
                let _ = git(clone, &["checkout", "--quiet", "--orphan", "main"]);
            }
        }
        git(clone, &["clean", "-fdq"])?;
        let local = local_snapshot(input)?;
        let remote = read_tree(clone)?;
        let base = SyncBase::load(input.base_path);
        let plan = plan(&local, &remote, &base, input.stamp);
        let outcome = apply(&plan, input.profile, clone, input.device)?;
        total.summary.pulled += outcome.summary.pulled;
        total.summary.deleted += outcome.summary.deleted;
        total.summary.conflicts.extend(outcome.summary.conflicts);
        total.summary.prefs_pending |= outcome.summary.prefs_pending;
        total.docs.extend(outcome.docs);
        let branch = branch.unwrap_or_else(|| "main".to_owned());
        git(clone, &["add", "-A"])?;
        let changed = !git(clone, &["status", "--porcelain"])?.is_empty();
        if !changed {
            plan.base.save(input.base_path)?;
            return Ok(total);
        }
        let message = format!("diri: sync from {}", input.device);
        git(
            clone,
            &[
                "-c",
                "user.name=Diri",
                "-c",
                "user.email=diri@localhost",
                "commit",
                "--quiet",
                "-m",
                &message,
            ],
        )?;
        match git(
            clone,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        ) {
            Ok(_) => {
                plan.base.save(input.base_path)?;
                total.summary.pushed += outcome.summary.pushed;
                total.summary.commit = git(clone, &["rev-parse", "--short", "HEAD"]).ok();
                return Ok(total);
            }
            Err(error) if attempt == 0 => {
                eprintln!("diri-portable: push rejected, retrying from fetch: {error}");
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop returns on its second attempt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(entries: &[(&str, &str)]) -> Snapshot {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn plan_moves_one_sided_changes_and_keeps_both_on_conflict() {
        let base_snap = snap(&[
            ("notes/a.md", "a"),
            ("notes/b.md", "b"),
            ("notes/c.md", "c"),
        ]);
        let base = SyncBase {
            hashes: base_snap
                .iter()
                .map(|(k, v)| (k.clone(), sha256_hex(v)))
                .collect(),
            docs: BTreeMap::new(),
        };
        let local = snap(&[
            ("notes/a.md", "a2"),
            ("notes/b.md", "b"),
            ("notes/c.md", "c-local"),
            ("notes/new.md", "n"),
        ]);
        let remote = snap(&[("notes/a.md", "a"), ("notes/c.md", "c-remote")]);
        let plan = plan(&local, &remote, &base, "mac-x");
        // a: local edit pushed. b: remote deletion pulled. new: pushed.
        assert_eq!(
            plan.to_remote.get("notes/a.md"),
            Some(&Some(b"a2".to_vec()))
        );
        assert_eq!(plan.to_local.get("notes/b.md"), Some(&None));
        assert_eq!(
            plan.to_remote.get("notes/new.md"),
            Some(&Some(b"n".to_vec()))
        );
        // c: both edited. Local kept and pushed, remote saved as a copy.
        assert_eq!(plan.conflicts, vec!["notes/c.md".to_owned()]);
        assert_eq!(
            plan.to_remote.get("notes/c.md"),
            Some(&Some(b"c-local".to_vec()))
        );
        assert!(!plan.to_local.contains_key("notes/c.md"));
        assert_eq!(
            plan.conflict_copies.get("conflicts/notes/c.md.mac-x"),
            Some(&b"c-remote".to_vec())
        );
    }

    #[test]
    fn a_fresh_mac_takes_the_synced_preferences() {
        let local = snap(&[(
            "prefs.json",
            r#"{"terminalTheme":"Default","reduceMotion":false}"#,
        )]);
        let remote = snap(&[("prefs.json", r#"{"terminalTheme":"Dusk"}"#)]);
        let first = plan(&local, &remote, &SyncBase::default(), "s");
        let merged: Value =
            serde_json::from_slice(first.to_local["prefs.json"].as_ref().unwrap()).unwrap();
        assert_eq!(
            merged,
            serde_json::json!({"terminalTheme": "Dusk", "reduceMotion": false})
        );
    }

    #[test]
    fn a_second_plan_after_applying_is_empty() {
        let local = snap(&[
            ("notes/a.md", "a"),
            ("prefs.json", r#"{"terminalTheme":"x"}"#),
        ]);
        let remote = snap(&[
            ("notes/b.md", "b"),
            ("prefs.json", r#"{"reduceMotion":true}"#),
        ]);
        let first = plan(&local, &remote, &SyncBase::default(), "s");
        let mut local2 = local.clone();
        let mut remote2 = remote.clone();
        for (rel, bytes) in &first.to_local {
            match bytes {
                Some(b) => local2.insert(rel.clone(), b.clone()),
                None => local2.remove(rel),
            };
        }
        for (rel, bytes) in &first.to_remote {
            match bytes {
                Some(b) => remote2.insert(rel.clone(), b.clone()),
                None => remote2.remove(rel),
            };
        }
        assert_eq!(local2, remote2);
        let prefs: Value = serde_json::from_slice(&local2["prefs.json"]).unwrap();
        assert_eq!(
            prefs,
            serde_json::json!({"terminalTheme": "x", "reduceMotion": true})
        );
        let second = plan(&local2, &remote2, &first.base, "s");
        assert!(second.to_local.is_empty() && second.to_remote.is_empty());
    }
}
