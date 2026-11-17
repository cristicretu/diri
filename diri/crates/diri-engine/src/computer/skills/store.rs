//! App Skills on disk, owner-only, one directory per app:
//!
//! ```text
//! app-skills/<bundle-id>/app.json             app name, versions seen
//! app-skills/<bundle-id>/skills/<name>.json   the current version
//! app-skills/<bundle-id>/skills/<name>.v<N>.json  earlier versions (rollback)
//! app-skills/<bundle-id>/runs.jsonl           redacted run log
//! ```
//!
//! Every path component is validated before it is joined, and a symlinked
//! app or skills directory is refused rather than followed.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{AppRef, Skill};

/// Earlier versions kept per skill.
pub(super) const KEPT_VERSIONS: usize = 5;
/// Versions of the app remembered in `app.json`.
const KEPT_APP_VERSIONS: usize = 10;
/// `runs.jsonl` is cut back to its newer half past this size.
const RUNS_LOG_CAP: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct AppMeta {
    pub bundle_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub versions_seen: Vec<String>,
}

/// One line of `runs.jsonl`. Never argument values or typed text.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct RunRecord {
    pub at: u64,
    pub skill: String,
    pub version: u32,
    /// `ok`, `failed`, `verify_ok`, `verify_failed`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<usize>,
    pub duration_ms: u64,
}

pub(super) struct Store {
    root: PathBuf,
}

/// A bundle id as a directory name: `com.example.App`.
pub(super) fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// A skill name: snake_case, starting with a letter, at most 64 characters.
pub(super) fn valid_skill_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn now_nonce() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn app_dir(&self, bundle_id: &str) -> io::Result<PathBuf> {
        if !valid_bundle_id(bundle_id) {
            return Err(invalid(format!("{bundle_id:?} is not a bundle id")));
        }
        let dir = self.root.join(bundle_id);
        refuse_symlink(&dir)?;
        Ok(dir)
    }

    fn skills_dir(&self, bundle_id: &str) -> io::Result<PathBuf> {
        let dir = self.app_dir(bundle_id)?.join("skills");
        refuse_symlink(&dir)?;
        Ok(dir)
    }

    fn skill_path(&self, bundle_id: &str, name: &str, version: Option<u32>) -> io::Result<PathBuf> {
        if !valid_skill_name(name) {
            return Err(invalid(format!("{name:?} is not a skill name")));
        }
        let file = match version {
            Some(n) => format!("{name}.v{n}.json"),
            None => format!("{name}.json"),
        };
        Ok(self.skills_dir(bundle_id)?.join(file))
    }

    fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        let temporary = path.with_extension(format!("tmp-{}", now_nonce()));
        diri_platform::private::replace_via(&temporary, path, bytes)
    }

    fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn app(&self, bundle_id: &str) -> io::Result<Option<AppMeta>> {
        Self::read_json(&self.app_dir(bundle_id)?.join("app.json"))
    }

    /// Records the app's name and, when known, a version it was seen at.
    pub fn note_app(&self, app: &AppRef, version: Option<&str>) -> io::Result<()> {
        let dir = self.app_dir(&app.bundle_id)?;
        diri_platform::private::create_dir_all(&dir)?;
        let mut meta = self.app(&app.bundle_id)?.unwrap_or_else(|| AppMeta {
            bundle_id: app.bundle_id.clone(),
            ..AppMeta::default()
        });
        let before = meta.clone();
        if !app.name.is_empty() {
            meta.name = app.name.clone();
        }
        if let Some(version) = version
            && !meta.versions_seen.iter().any(|seen| seen == version)
        {
            meta.versions_seen.push(version.to_owned());
            let excess = meta.versions_seen.len().saturating_sub(KEPT_APP_VERSIONS);
            meta.versions_seen.drain(..excess);
        }
        if meta == before && dir.join("app.json").exists() {
            return Ok(());
        }
        Self::write_json(&dir.join("app.json"), &meta)
    }

    pub fn load(&self, bundle_id: &str, name: &str) -> io::Result<Option<Skill>> {
        Self::read_json(&self.skill_path(bundle_id, name, None)?)
    }

    pub fn load_version(
        &self,
        bundle_id: &str,
        name: &str,
        version: u32,
    ) -> io::Result<Option<Skill>> {
        match self.load(bundle_id, name)? {
            Some(current) if current.version == version => Ok(Some(current)),
            _ => Self::read_json(&self.skill_path(bundle_id, name, Some(version))?),
        }
    }

    /// Saves `skill` as the next version of its name: the current one is
    /// archived and only the newest [`KEPT_VERSIONS`] archives are kept.
    /// Returns the version written.
    pub fn save(&self, skill: &mut Skill) -> io::Result<u32> {
        let bundle_id = skill.app.bundle_id.clone();
        let dir = self.skills_dir(&bundle_id)?;
        diri_platform::private::create_dir_all(&dir)?;
        let current = self.load(&bundle_id, &skill.name)?;
        let previous = current.as_ref().map_or(0, |current| current.version);
        let highest = self
            .archived_versions(&bundle_id, &skill.name)?
            .into_iter()
            .max()
            .unwrap_or(0)
            .max(previous);
        if let Some(current) = &current {
            Self::write_json(
                &self.skill_path(&bundle_id, &skill.name, Some(current.version))?,
                current,
            )?;
        }
        skill.version = highest + 1;
        Self::write_json(&self.skill_path(&bundle_id, &skill.name, None)?, skill)?;
        let mut archived = self.archived_versions(&bundle_id, &skill.name)?;
        archived.sort_unstable();
        let excess = archived.len().saturating_sub(KEPT_VERSIONS);
        for version in &archived[..excess] {
            let _ =
                std::fs::remove_file(self.skill_path(&bundle_id, &skill.name, Some(*version))?);
        }
        Ok(skill.version)
    }

    /// Rewrites the current skill in place (stats, status), only while it is
    /// still `version`: a save that raced the run wins.
    pub fn update(
        &self,
        bundle_id: &str,
        name: &str,
        version: u32,
        change: impl FnOnce(&mut Skill),
    ) -> io::Result<Option<Skill>> {
        let Some(mut skill) = self.load(bundle_id, name)? else {
            return Ok(None);
        };
        if skill.version != version {
            return Ok(Some(skill));
        }
        change(&mut skill);
        Self::write_json(&self.skill_path(bundle_id, name, None)?, &skill)?;
        Ok(Some(skill))
    }

    pub fn archived_versions(&self, bundle_id: &str, name: &str) -> io::Result<Vec<u32>> {
        let dir = self.skills_dir(bundle_id)?;
        let prefix = format!("{name}.v");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        Ok(entries
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name();
                let file = file.to_str()?;
                file.strip_prefix(&prefix)?
                    .strip_suffix(".json")?
                    .parse::<u32>()
                    .ok()
            })
            .collect())
    }

    /// Deletes a skill and its archives; the app's directory goes too once
    /// it has no skills left. Returns whether the skill existed.
    pub fn forget(&self, bundle_id: &str, name: &str) -> io::Result<bool> {
        let current = self.skill_path(bundle_id, name, None)?;
        let existed = current.exists();
        let _ = std::fs::remove_file(&current);
        for version in self.archived_versions(bundle_id, name)? {
            let _ = std::fs::remove_file(self.skill_path(bundle_id, name, Some(version))?);
        }
        if self.skill_names(bundle_id)?.is_empty() {
            let _ = std::fs::remove_dir_all(self.app_dir(bundle_id)?);
        }
        Ok(existed)
    }

    pub fn skill_names(&self, bundle_id: &str) -> io::Result<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(self.skills_dir(bundle_id)?) else {
            return Ok(Vec::new());
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name();
                let name = file.to_str()?.strip_suffix(".json")?.to_owned();
                (valid_skill_name(&name)).then_some(name)
            })
            .collect();
        names.sort();
        Ok(names)
    }

    pub fn apps(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut apps: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| valid_bundle_id(name))
            .collect();
        apps.sort();
        apps
    }

    pub fn append_run(&self, bundle_id: &str, record: &RunRecord) -> io::Result<()> {
        let dir = self.app_dir(bundle_id)?;
        diri_platform::private::create_dir_all(&dir)?;
        let path = dir.join("runs.jsonl");
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > RUNS_LOG_CAP) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let kept = lines[lines.len() / 2..].join("\n") + "\n";
            let temporary = path.with_extension(format!("tmp-{}", now_nonce()));
            diri_platform::private::replace_via(&temporary, &path, kept)?;
        }
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        diri_platform::private::append(&path)?.write_all(&line)
    }

    /// The newest `limit` runs of `name`, newest last.
    pub fn runs(&self, bundle_id: &str, name: &str, limit: usize) -> io::Result<Vec<RunRecord>> {
        let path = self.app_dir(bundle_id)?.join("runs.jsonl");
        let Ok(file) = std::fs::File::open(&path) else {
            return Ok(Vec::new());
        };
        let mut runs: Vec<RunRecord> = io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<RunRecord>(&line).ok())
            .filter(|run| run.skill == name)
            .collect();
        let excess = runs.len().saturating_sub(limit);
        runs.drain(..excess);
        Ok(runs)
    }
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(invalid(format!(
            "{} is a symlink; App Skills never follow one",
            path.display()
        ))),
        _ => Ok(()),
    }
}
