//! Mac-to-Mac migration: one bundle with the portable profile, every local
//! session record and its Claude Code or Codex conversation. Import remaps
//! the home folder and each project (by `--map`, by path, or by Git origin),
//! moves transcripts to where the CLIs look for them on this Mac, and merges
//! without duplicating or overwriting anything already here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};

use diri_proto::portable::{
    BundleSource, MigrateExportResult, MigrateImportPlan, PathMapping, PlannedAction,
    PlannedProject, PlannedSession, PortableCounts, ProjectMatch, SkippedItem,
};
use diri_proto::schedules::ScheduleRecord;
use diri_proto::{DateMillis, ProjectId, SessionRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::bundle::{BundleError, BundleReader, BundleWriter, KdfParams};
use crate::docs::{self, ImportedSchedule, LocalProject, ProjectEntry};
use crate::files::{WriteOutcome, read_bounded, sha256_hex, write_if_absent};
use crate::origin::{COMMON_ROOTS, normalize_remote, scan_checkouts};
use crate::pathmap::{PathMap, claude_transcript_path, replace_prefix, to_portable};
use crate::sessions::{self, Decision};
use crate::sync::{LocalProfile, Snapshot};
use crate::transcript::{MAX_TRANSCRIPT_BYTES, codex_rollout_id, rewrite_cwd};

/// Most transcript bytes one bundle carries.
pub const MAX_TOTAL_TRANSCRIPT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleManifest {
    format: u32,
    device: String,
    source_home: String,
    exported_at: f64,
    diri_version: String,
    counts: PortableCounts,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptRef {
    /// Bundle path.
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionsDoc {
    format: u32,
    source_home: String,
    sessions: Vec<Value>,
    projects: Vec<ProjectEntry>,
    /// Session id -> its transcript in this bundle.
    transcripts: BTreeMap<String, TranscriptRef>,
    /// Exporting Mac's project id -> its `~/` root, so a linked worktree
    /// outside its repository still finds its project.
    #[serde(default)]
    project_ids: BTreeMap<String, String>,
}

/// What this Mac hands to an export.
pub struct ExportInput<'a> {
    pub home: &'a Path,
    pub device: &'a str,
    pub diri_version: &'a str,
    pub profile: &'a LocalProfile,
    /// Portable documents (`prefs.json`, `recipes.json`, ...).
    pub docs: Snapshot,
    pub sessions: Vec<SessionRecord>,
    pub projects: Vec<LocalProject>,
    pub skip_transcripts: bool,
    pub kdf: KdfParams,
}

/// The file a session's conversation lives in on this Mac, if found.
fn find_transcript(home: &Path, record: &SessionRecord) -> Option<PathBuf> {
    let kind = sessions::transcript_kind(&record.kind)?;
    let id = record.agent_session_id.as_deref()?;
    if !crate::files::safe_relative(id) || id.contains('/') {
        return None;
    }
    let file_name = format!("{id}.jsonl");
    if let Some(path) = record.transcript_path.as_deref().map(PathBuf::from)
        && path.is_absolute()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&file_name))
        && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
    {
        return Some(path);
    }
    match kind {
        "claude" => {
            let direct = claude_transcript_path(home, &record.cwd, id);
            if direct.is_file() {
                return Some(direct);
            }
            fs::read_dir(home.join(".claude/projects"))
                .ok()?
                .flatten()
                .map(|dir| dir.path().join(&file_name))
                .find(|path| fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
        }
        _ => {
            let root = home.join(".codex/sessions");
            let mut years: Vec<_> = fs::read_dir(&root)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .collect();
            years.sort();
            for year in years.iter().rev() {
                for month in sorted_dirs(year).iter().rev() {
                    for day in sorted_dirs(month).iter().rev() {
                        for entry in fs::read_dir(day).into_iter().flatten().flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if codex_rollout_id(&name) == Some(id) {
                                return Some(entry.path());
                            }
                        }
                    }
                }
            }
            None
        }
    }
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// The bundle path for a transcript found at `path`.
fn bundle_transcript_path(kind: &str, id: &str, path: &Path) -> String {
    if kind == "codex" {
        let parts: Vec<String> = path
            .iter()
            .rev()
            .take(4)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if parts.len() == 4
            && parts[1..]
                .iter()
                .all(|p| p.bytes().all(|b| b.is_ascii_digit()))
        {
            return format!(
                "transcripts/codex/{}/{}/{}/{}",
                parts[3], parts[2], parts[1], parts[0]
            );
        }
        return format!("transcripts/codex/1970/01/01/rollout-{id}.jsonl");
    }
    format!("transcripts/claude/{id}.jsonl")
}

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or_default()
}

fn count_profile(profile: &Snapshot) -> PortableCounts {
    let doc_len = |rel: &str, field: &str| {
        profile
            .get(rel)
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            .and_then(|doc| doc.get(field).and_then(Value::as_array).map(Vec::len))
            .unwrap_or(0) as u32
    };
    PortableCounts {
        notes: profile.keys().filter(|k| k.starts_with("notes/")).count() as u32,
        skills: profile
            .keys()
            .filter(|k| {
                k.starts_with("app-skills/") && k.contains("/skills/") && !is_version_file(k)
            })
            .count() as u32,
        recipes: doc_len("recipes.json", "items"),
        schedules: doc_len("schedules.json", "schedules"),
        projects: doc_len("projects.json", "projects"),
        ..PortableCounts::default()
    }
}

fn is_version_file(rel: &str) -> bool {
    rel.rsplit_once(".v").is_some_and(|(_, rest)| {
        rest.trim_end_matches(".json")
            .bytes()
            .all(|b| b.is_ascii_digit())
    })
}

/// Writes a migration bundle to `out`.
pub fn export(
    input: ExportInput<'_>,
    out: &Path,
    passphrase: Option<&str>,
) -> io::Result<MigrateExportResult> {
    let home = input.home.to_string_lossy().into_owned();
    let (mut profile, mut skipped) = input.profile.collect()?;
    for (rel, bytes) in &input.docs {
        profile.insert(rel.clone(), bytes.clone());
    }
    let mut counts = count_profile(&profile);
    let mut doc = SessionsDoc {
        format: 1,
        source_home: home.clone(),
        projects: docs::read_projects(&docs::export_projects(&home, &input.projects)),
        project_ids: input
            .projects
            .iter()
            .map(|p| (p.id.clone(), to_portable(&home, &p.root)))
            .collect(),
        ..SessionsDoc::default()
    };
    let mut transcripts: Vec<(String, PathBuf)> = Vec::new();
    let mut transcript_bytes = 0u64;
    for record in &input.sessions {
        if !sessions::exportable(record) {
            skipped.push(SkippedItem {
                item: format!("“{}”", record.title),
                reason: "runs on a remote host".into(),
            });
            continue;
        }
        let Some(value) = sessions::sanitize(record, &home) else {
            continue;
        };
        doc.sessions.push(value);
        counts.sessions += 1;
        if input.skip_transcripts {
            continue;
        }
        let (Some(kind), Some(id)) = (
            sessions::transcript_kind(&record.kind),
            record.agent_session_id.as_deref(),
        ) else {
            continue;
        };
        let Some(path) = find_transcript(input.home, record) else {
            continue;
        };
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
        if size > MAX_TRANSCRIPT_BYTES || transcript_bytes + size > MAX_TOTAL_TRANSCRIPT_BYTES {
            skipped.push(SkippedItem {
                item: format!("Conversation of “{}”", record.title),
                reason: "too large to carry".into(),
            });
            continue;
        }
        transcript_bytes += size;
        let rel = bundle_transcript_path(kind, id, &path);
        doc.transcripts.insert(
            record.id.0.clone(),
            TranscriptRef {
                path: rel.clone(),
                sha256: String::new(),
            },
        );
        transcripts.push((rel, path));
    }
    // Read transcripts once, hash them for the index, then stream.
    let mut loaded: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen = HashSet::new();
    for (rel, path) in transcripts {
        if !seen.insert(rel.clone()) {
            continue;
        }
        match read_bounded(&path, MAX_TRANSCRIPT_BYTES) {
            Ok(bytes) => {
                for reference in doc.transcripts.values_mut().filter(|r| r.path == rel) {
                    reference.sha256 = sha256_hex(&bytes);
                }
                loaded.push((rel, bytes));
            }
            Err(error) => {
                doc.transcripts.retain(|_, r| r.path != rel);
                skipped.push(SkippedItem {
                    item: rel,
                    reason: error.to_string(),
                });
            }
        }
    }
    counts.transcripts = loaded.len() as u32;
    let manifest = BundleManifest {
        format: 1,
        device: input.device.to_owned(),
        source_home: home.clone(),
        exported_at: now_millis(),
        diri_version: input.diri_version.to_owned(),
        counts: counts.clone(),
    };
    let path = if out.is_dir() {
        let date = chrono_like_date();
        let device: String = input
            .device
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        out.join(format!("Diri-{device}-{date}.diribundle"))
    } else {
        out.to_path_buf()
    };
    let parent = path.parent().unwrap_or(Path::new("."));
    let temporary = parent.join(format!(
        ".{}.{}.partial",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> io::Result<()> {
        let file = diri_platform::private::create(&temporary)?;
        let mut writer = BundleWriter::create_with(BufWriter::new(file), passphrase, input.kdf)?;
        writer.add("bundle.json", &serde_json::to_vec_pretty(&manifest)?)?;
        writer.add("sessions.json", &serde_json::to_vec(&doc)?)?;
        for (rel, bytes) in &profile {
            writer.add(&format!("profile/{rel}"), bytes)?;
        }
        for (rel, bytes) in &loaded {
            writer.add(rel, bytes)?;
        }
        let file = writer.finish()?.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(MigrateExportResult {
        bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or_default(),
        path: path.to_string_lossy().into_owned(),
        encrypted: passphrase.is_some_and(|p| !p.is_empty()),
        counts,
        skipped,
    })
}

fn chrono_like_date() -> String {
    // Civil date from Unix days (Howard Hinnant's algorithm): no date crate.
    let days = (now_millis() / 86_400_000.0).floor() as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// This Mac, as an import sees it.
pub struct ImportTarget<'a> {
    pub home: &'a Path,
    pub profile: &'a LocalProfile,
    /// Records already here.
    pub existing: Vec<SessionRecord>,
    /// Ids with a live process here.
    pub live: HashSet<String>,
    pub local_projects: Vec<LocalProject>,
    pub schedules: Vec<ScheduleRecord>,
    /// This Mac's `prefs.json`.
    pub prefs: Map<String, Value>,
    /// This Mac's project id for an absolute root.
    pub project_id: &'a dyn Fn(&str) -> String,
    /// More folders to look for checkouts in.
    pub scan_roots: Vec<PathBuf>,
}

/// What the Engine applies after a non-dry-run import.
#[derive(Debug, Default)]
pub struct ImportOutput {
    pub plan: MigrateImportPlan,
    /// Records to insert (add or update), already remapped and ended.
    pub sessions: Vec<SessionRecord>,
    pub schedules: Vec<ImportedSchedule>,
    /// Portable preferences for the app to apply.
    pub pending_prefs: Option<Map<String, Value>>,
    /// Project roots that exist here, to register.
    pub projects: Vec<String>,
}

fn read_json<T: for<'de> Deserialize<'de>>(bytes: &[u8], what: &str) -> Result<T, BundleError> {
    serde_json::from_slice(bytes).map_err(|_| BundleError::Corrupt(format!("{what} does not read")))
}

/// Plans (and unless `dry_run`, performs) an import.
pub fn import(
    bundle: &Path,
    passphrase: Option<&str>,
    maps: &[PathMapping],
    dry_run: bool,
    target: ImportTarget<'_>,
) -> Result<ImportOutput, BundleError> {
    let file = fs::File::open(bundle)?;
    let mut reader = BundleReader::open(BufReader::new(file), passphrase)?;
    let encrypted = reader.encrypted();
    let mut next = || reader.next_entry();
    let Some((name, bytes)) = next()? else {
        return Err(BundleError::Corrupt("it is empty".into()));
    };
    if name != "bundle.json" {
        return Err(BundleError::Corrupt("bundle.json is not first".into()));
    }
    let manifest: BundleManifest = read_json(&bytes, "bundle.json")?;
    let Some((name, bytes)) = next()? else {
        return Err(BundleError::Corrupt("sessions.json is missing".into()));
    };
    if name != "sessions.json" {
        return Err(BundleError::Corrupt("sessions.json is not second".into()));
    }
    let doc: SessionsDoc = read_json(&bytes, "sessions.json")?;
    drop(bytes);

    let home = target.home.to_string_lossy().into_owned();
    let mut map = PathMap::new(&doc.source_home, &home);
    for mapping in maps {
        map.add(&mapping.from, &mapping.to);
    }

    // Projects: each exported root, resolved on this Mac.
    let mut exported_roots: BTreeMap<String, Option<String>> = doc
        .projects
        .iter()
        .map(|p| (p.root.clone(), p.git_origin.clone()))
        .collect();
    let incoming: Vec<SessionRecord> = doc
        .sessions
        .iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect();
    for record in &incoming {
        if doc.project_ids.contains_key(&record.project_id.0) {
            continue;
        }
        if !exported_roots
            .keys()
            .any(|root| replace_prefix(&record.cwd, root, "").is_some())
        {
            exported_roots.insert(record.cwd.clone(), None);
        }
    }
    let mut checkouts: Option<BTreeMap<String, PathBuf>> = None;
    let mut projects: Vec<PlannedProject> = Vec::new();
    for (root, origin) in &exported_roots {
        let explicit = map.rule_for(root).is_some();
        let expanded = map.remap(root);
        let (to, how) = if explicit {
            (expanded, ProjectMatch::Mapped)
        } else if Path::new(&expanded).is_dir() {
            (expanded, ProjectMatch::Same)
        } else {
            let found = origin.as_deref().and_then(|origin| {
                let checkouts = checkouts.get_or_insert_with(|| {
                    let mut roots: Vec<PathBuf> = COMMON_ROOTS
                        .iter()
                        .map(|r| target.home.join(r))
                        .chain(target.scan_roots.iter().cloned())
                        .collect();
                    for project in &target.local_projects {
                        if let Some(parent) = Path::new(&project.root).parent() {
                            roots.push(parent.to_path_buf());
                        }
                    }
                    roots.sort();
                    roots.dedup();
                    scan_checkouts(&roots)
                });
                checkouts.get(&normalize_remote(origin)).cloned()
            });
            match found {
                Some(found) => {
                    let found = found.to_string_lossy().into_owned();
                    map.add(root, &found);
                    (found, ProjectMatch::Origin)
                }
                None => (expanded, ProjectMatch::Unresolved),
            }
        };
        projects.push(PlannedProject {
            from: root.clone(),
            to,
            how,
            git_origin: origin.as_deref().map(normalize_remote),
            sessions: 0,
        });
    }
    let project_of = |projects: &[PlannedProject], portable_cwd: &str| -> Option<usize> {
        projects
            .iter()
            .enumerate()
            .filter(|(_, p)| replace_prefix(portable_cwd, &p.from, "").is_some())
            .max_by_key(|(_, p)| p.from.len())
            .map(|(i, _)| i)
    };

    // Sessions.
    let existing: HashMap<&str, &SessionRecord> = target
        .existing
        .iter()
        .map(|r| (r.id.0.as_str(), r))
        .collect();
    let mut plan = MigrateImportPlan {
        source: BundleSource {
            device: manifest.device.clone(),
            home: manifest.source_home.clone(),
            exported_at: Some(DateMillis(manifest.exported_at)),
            diri_version: manifest.diri_version.clone(),
            encrypted,
        },
        dry_run,
        bundle: manifest.counts.clone(),
        ..MigrateImportPlan::default()
    };
    let mut out = ImportOutput::default();
    // Bundle transcript path -> (destination, cwd remap from -> to).
    let mut transcript_targets: HashMap<String, (PathBuf, String, String)> = HashMap::new();
    for mut record in incoming {
        let project = doc
            .project_ids
            .get(&record.project_id.0)
            .and_then(|root| projects.iter().position(|p| &p.from == root))
            .or_else(|| project_of(&projects, &record.cwd));
        if let Some(index) = project {
            projects[index].sessions += 1;
        }
        let mapped_cwd = map.remap(&record.cwd);
        let project_root = project
            .map(|i| projects[i].to.clone())
            .unwrap_or_else(|| mapped_cwd.clone());
        let mut note = None;
        let cwd = if Path::new(&mapped_cwd).is_dir() || project.is_none() {
            mapped_cwd.clone()
        } else if Path::new(&project_root).is_dir() {
            note = Some("Worktree not found here; resumes in the project folder".to_owned());
            project_root.clone()
        } else {
            note = Some("Project folder not found here".to_owned());
            mapped_cwd.clone()
        };
        record.cwd = cwd.clone();
        record.worktree_path = record
            .worktree_path
            .as_deref()
            .map(|p| map.remap(p))
            .filter(|p| Path::new(p).is_dir());
        record.project_id = ProjectId((target.project_id)(&project_root));
        record.status = sessions::ended();
        let transcript = doc.transcripts.get(&record.id.0);
        let destination = transcript.and_then(|t| {
            let id = record.agent_session_id.as_deref()?;
            Some(match sessions::transcript_kind(&record.kind)? {
                "claude" => claude_transcript_path(target.home, &cwd, id),
                _ => target
                    .home
                    .join(".codex/sessions")
                    .join(t.path.strip_prefix("transcripts/codex/")?),
            })
        });
        record.transcript_path = destination
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        record.resumability = sessions::imported_resumability(&record.kind, destination.is_some());
        let decision = sessions::decide(
            existing.get(record.id.0.as_str()).copied(),
            target.live.contains(&record.id.0),
            &record,
        );
        let action = match decision {
            Decision::Add => {
                plan.added.sessions += 1;
                PlannedAction::Add
            }
            Decision::Update => {
                plan.updated += 1;
                PlannedAction::Update
            }
            Decision::Keep => {
                plan.kept += 1;
                PlannedAction::Keep
            }
        };
        if decision != Decision::Keep {
            if let (Some(t), Some(destination)) = (transcript, &destination) {
                let original_cwd =
                    crate::pathmap::expand(&doc.source_home, &original_cwd_of(&doc, &record.id.0));
                transcript_targets.insert(
                    t.path.clone(),
                    (destination.clone(), original_cwd, cwd.clone()),
                );
            }
            out.sessions.push(record.clone());
        }
        plan.sessions.push(PlannedSession {
            id: record.id.0.clone(),
            title: record.title.clone(),
            kind: record.kind.id().to_owned(),
            cwd,
            action,
            resumable: destination.is_some()
                && note.as_deref() != Some("Project folder not found here"),
            note,
        });
    }
    plan.projects = projects.clone();
    for project in &projects {
        if project.how != ProjectMatch::Unresolved && Path::new(&project.to).is_dir() {
            out.projects.push(project.to.clone());
        }
    }

    // Profile and transcripts, in bundle order.
    let mut profile = Snapshot::new();
    let mut conflicts: BTreeSet<String> = BTreeSet::new();
    while let Some((name, bytes)) = next()? {
        if let Some(rel) = name.strip_prefix("profile/") {
            profile.insert(rel.to_owned(), bytes);
            continue;
        }
        if dry_run {
            // The plan needs only the index; skip decrypting the rest.
            break;
        }
        let Some((destination, from, to)) = transcript_targets.get(&name) else {
            continue;
        };
        let rewritten = rewrite_cwd(&bytes, |cwd| {
            let mapped = map.remap(cwd);
            if cwd == from { to.clone() } else { mapped }
        });
        match write_if_absent(destination, &rewritten, false)? {
            WriteOutcome::Written => plan.added.transcripts += 1,
            WriteOutcome::Same => {}
            WriteOutcome::KeptExisting => {
                conflicts.insert(to_portable(&home, &destination.to_string_lossy()));
            }
        }
    }
    if dry_run {
        plan.added.transcripts = transcript_targets
            .values()
            .filter(|(destination, _, _)| !destination.exists())
            .count() as u32;
    }

    // File-backed profile: never overwrite.
    for (rel, bytes) in &profile {
        if crate::sync::DOCS.contains(&rel.as_str()) {
            continue;
        }
        let Some(path) = target.profile.existing(rel) else {
            continue;
        };
        match write_if_absent(&path, bytes, dry_run)? {
            WriteOutcome::Written => {
                if rel.starts_with("notes/") {
                    plan.added.notes += 1;
                } else if rel.contains("/skills/") && !is_version_file(rel) {
                    plan.added.skills += 1;
                }
            }
            WriteOutcome::Same => plan.kept += 1,
            WriteOutcome::KeptExisting => {
                conflicts.insert(rel.clone());
            }
        }
    }
    // Schedules: by id, or an identical spec already here.
    if let Some(doc) = profile
        .get("schedules.json")
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
    {
        for schedule in docs::import_schedules(&doc, &map) {
            let here = target
                .schedules
                .iter()
                .any(|s| s.id == schedule.id || s.spec == schedule.spec);
            if !here {
                plan.added.schedules += 1;
                out.schedules.push(schedule);
            }
        }
    }
    // Preferences and recipes, for the app.
    let prefs_doc = profile
        .get("prefs.json")
        .and_then(|b| serde_json::from_slice::<Value>(b).ok());
    let recipes_doc = profile
        .get("recipes.json")
        .and_then(|b| serde_json::from_slice::<Value>(b).ok());
    if prefs_doc.is_some() || recipes_doc.is_some() {
        let pending = docs::import_prefs(
            &prefs_doc.unwrap_or_else(|| json!({})),
            recipes_doc.as_ref(),
            &target.prefs,
            &map,
            target.project_id,
        );
        let before = target
            .prefs
            .get("launchRecipes")
            .and_then(|b| b.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let after = pending
            .get("launchRecipes")
            .and_then(|b| b.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        plan.added.recipes = after.saturating_sub(before) as u32;
        let changes = pending
            .iter()
            .any(|(key, value)| target.prefs.get(key) != Some(value));
        if changes {
            plan.prefs_pending = !dry_run;
            out.pending_prefs = Some(pending);
        }
    }
    plan.added.projects = out
        .projects
        .iter()
        .filter(|root| !target.local_projects.iter().any(|p| &p.root == *root))
        .count() as u32;
    plan.conflicts = conflicts.into_iter().collect();
    out.plan = plan;
    Ok(out)
}

fn original_cwd_of(doc: &SessionsDoc, id: &str) -> String {
    doc.sessions
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|s| s.get("cwd").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}
