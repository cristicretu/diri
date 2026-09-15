//! Short-lived transcript collection. Never linked into the Holder event loop.
use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, EnvironmentVariable, TranscriptUsageRequest, TranscriptUsageResult,
};
use diri_usage::transcripts::{ScanPaths, SystemClock, UsageProvider, UsageStore};
use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub fn collect(
    executable: &Path,
    request: &TranscriptUsageRequest,
) -> io::Result<TranscriptUsageResult> {
    request.validate().map_err(io::Error::other)?;
    let captured = crate::environment::capture(
        &EnvironmentCaptureRequest {
            cwd: Some("~".into()),
            timeout_millis: 5_000,
        },
        executable,
    )?;
    let home = captured
        .environment
        .iter()
        .find(|v| v.name == "HOME")
        .map(|v| PathBuf::from(&v.value))
        .ok_or_else(|| io::Error::other("remote usage HOME is unavailable"))?;
    let roots = roots(&home, &captured.environment, request)?;
    let cache_root = crate::paths::StatePaths::resolve()?.root.join("usage-v1");
    collect_at(roots, &cache_root)
}

fn roots(
    home: &Path,
    environment: &[EnvironmentVariable],
    request: &TranscriptUsageRequest,
) -> io::Result<Vec<(PathBuf, UsageProvider)>> {
    let mut roots = ScanPaths::for_home(home).roots;
    roots.push((home.join(".codex/archived_sessions"), UsageProvider::Codex));
    for (name, provider, suffixes) in [
        (
            "CLAUDE_CONFIG_DIR",
            UsageProvider::Claude,
            &["projects"][..],
        ),
        (
            "CODEX_HOME",
            UsageProvider::Codex,
            &["sessions", "archived_sessions"][..],
        ),
    ] {
        if let Some(value) = environment
            .iter()
            .find(|v| v.name == name && !v.value.is_empty())
        {
            let base = PathBuf::from(&value.value);
            if !base.is_absolute() || base.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(io::Error::other(
                    "remote usage provider directory must be absolute",
                ));
            }
            for suffix in suffixes {
                roots.push((base.join(suffix), provider));
            }
        }
    }
    for profile in &request.profiles {
        let base = profile.config_home.strip_prefix("~/").map_or_else(
            || PathBuf::from(&profile.config_home),
            |suffix| home.join(suffix),
        );
        match profile.provider.as_str() {
            "claude" => roots.push((base.join("projects"), UsageProvider::Claude)),
            "codex" => {
                roots.push((base.join("sessions"), UsageProvider::Codex));
                roots.push((base.join("archived_sessions"), UsageProvider::Codex));
            }
            _ => return Err(io::Error::other("invalid usage provider")),
        }
    }
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    roots.dedup();
    // Validate every existing ancestor, not just the projects/sessions leaf.
    for (path, _) in &roots {
        for ancestor in path.ancestors() {
            crate::paths::reject_symlink(ancestor)?;
        }
    }
    Ok(roots)
}

fn collect_at(
    roots: Vec<(PathBuf, UsageProvider)>,
    cache_root: &Path,
) -> io::Result<TranscriptUsageResult> {
    crate::paths::ensure_private_dir(cache_root)?;
    let _lock = crate::state::acquire_lock(&cache_root.join("scan.lock"))
        .map_err(|_| io::Error::other("remote usage scan is already running"))?;
    let identity = cache_root.join("source-id");
    crate::paths::reject_symlink(&identity)?;
    let source_id = if identity.exists() {
        use std::io::Read;
        let mut value = String::new();
        std::fs::File::open(&identity)?
            .take(33)
            .read_to_string(&mut value)?;
        value
    } else {
        use std::io::Write;
        let value = crate::state::random_hex(16)?;
        let mut file = crate::paths::create_private_file(&identity)?;
        file.write_all(value.as_bytes())?;
        value
    };
    let cache_file = cache_root.join("transcripts.json");
    crate::paths::reject_symlink(&cache_file)?;
    if let Ok(metadata) = std::fs::metadata(&cache_file)
        && (!metadata.is_file() || metadata.len() > 64 * 1024 * 1024)
    {
        return Err(io::Error::other("remote usage cache size limit exceeded"));
    }
    let mut store = UsageStore::with_paths_and_clock(ScanPaths { roots, cache_file }, SystemClock);
    let snapshot = store.refresh_remote()?;
    snapshot
        .history
        .remote_summary(snapshot.updated_at, source_id)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn collector_reuses_the_shared_ledger_and_returns_only_usage() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let transcripts = root.join("sessions");
        std::fs::create_dir(&transcripts).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Derive a current ISO date from the public dashboard projection.
        let date = diri_usage::transcripts::dashboard::date_label((now / 86_400) as i64);
        let context = serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.4"}});
        let usage = serde_json::json!({"timestamp":format!("{date}T12:00:00Z"),"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}}});
        std::fs::write(
            transcripts.join("a.jsonl"),
            format!("{context}\n{usage}\n{{\"prompt\":\"DO_NOT_EXPORT\"}}\n"),
        )
        .unwrap();
        let cache = root.join("cache");
        let roots = vec![(transcripts, UsageProvider::Codex)];
        let first = collect_at(roots.clone(), &cache).unwrap();
        let second = collect_at(roots, &cache).unwrap();
        assert_eq!(first.buckets, second.buckets);
        assert_eq!(first.buckets.len(), 1);
        assert_eq!(first.buckets[0].input, 60);
        assert_eq!(first.buckets[0].cache_read, 40);
        assert!(
            !serde_json::to_string(&first)
                .unwrap()
                .contains("DO_NOT_EXPORT")
        );
        assert_eq!(
            std::fs::metadata(cache.join("transcripts.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn provider_overrides_are_remote_and_symlinks_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let custom = home.join("work");
        let env = vec![EnvironmentVariable {
            name: "CODEX_HOME".into(),
            value: custom.to_str().unwrap().into(),
        }];
        let paths = roots(&home, &env, &TranscriptUsageRequest::default()).unwrap();
        assert!(paths.contains(&(custom.join("sessions"), UsageProvider::Codex)));
        std::os::unix::fs::symlink(&custom, home.join(".codex")).unwrap();
        assert!(roots(&home, &env, &TranscriptUsageRequest::default()).is_err());
    }
}
