//! Independently paced SSH usage refreshes. Local transcript writes never wait
//! for these requests; successful per-host snapshots replace their predecessors.
use super::{RemoteUsageSnapshot, RemoteUsageStatus, UsageSnapshot};
use diri_client::DaemonClient;
use diri_proto::{HostsConfig, paths::DirijorPaths, remote_pty::TranscriptUsageResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_HOSTS: usize = 64;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct SavedUsage {
    destination: String,
    result: TranscriptUsageResult,
}

pub(crate) async fn watch_remote_usage(
    client: Arc<DaemonClient>,
    home: PathBuf,
    tx: watch::Sender<UsageSnapshot>,
) {
    let cache_path = DirijorPaths::usage_cache_file(&home).with_file_name("remote-usage-v1.json");
    let mut cached = load_cache(&cache_path);
    loop {
        let hosts = HostsConfig::load(DirijorPaths::hosts_config_file(&home));
        let mut destinations = HashSet::new();
        let hosts = hosts
            .hosts
            .into_iter()
            .filter(|host| destinations.insert(host.ssh.clone()))
            .take(MAX_HOSTS)
            .collect::<Vec<_>>();
        let valid = hosts
            .iter()
            .map(|host| host.id.as_str())
            .collect::<HashSet<_>>();
        cached.retain(|id, _| valid.contains(id.as_str()));
        let mut snapshots = hosts
            .iter()
            .map(|host| {
                let data = cached
                    .get(&host.id)
                    .filter(|saved| saved.destination == destination_key(&host.ssh))
                    .map(|saved| Arc::new(saved.result.clone()));
                RemoteUsageSnapshot {
                    host: host.id.clone(),
                    name: host.display_name().to_owned(),
                    status: RemoteUsageStatus::Loading,
                    data,
                }
            })
            .collect::<Vec<_>>();
        publish(&tx, &snapshots);
        // Two bounded cold-path requests at a time; no periodic Holder work.
        for batch in hosts.chunks(2) {
            let mut tasks = JoinSet::new();
            for host in batch {
                let client = Arc::clone(&client);
                let host = host.clone();
                tasks.spawn(async move {
                    let result = async {
                        client.wait_until_connected(Duration::from_secs(5)).await?;
                        client.host_usage(host.id.clone()).await
                    }
                    .await;
                    (host, result)
                });
            }
            while let Some(completed) = tasks.join_next().await {
                let Ok((host, result)) = completed else {
                    continue;
                };
                let Some(snapshot) = snapshots.iter_mut().find(|entry| entry.host == host.id)
                else {
                    continue;
                };
                if let Some(result) = apply_result(snapshot, result.ok()) {
                    cached.insert(
                        host.id,
                        SavedUsage {
                            destination: destination_key(&host.ssh),
                            result,
                        },
                    );
                }
                publish(&tx, &snapshots);
            }
        }
        // A failed write does not discard the in-memory last successful data.
        let _ = save_cache(&cache_path, &cached);
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

fn apply_result(
    snapshot: &mut RemoteUsageSnapshot,
    result: Option<TranscriptUsageResult>,
) -> Option<TranscriptUsageResult> {
    match result.filter(|result| result.validate().is_ok()) {
        Some(result) => {
            snapshot.status = RemoteUsageStatus::Ready;
            snapshot.data = Some(Arc::new(result.clone()));
            Some(result)
        }
        None => {
            snapshot.status = RemoteUsageStatus::Unavailable;
            None
        }
    }
}

fn publish(tx: &watch::Sender<UsageSnapshot>, snapshots: &[RemoteUsageSnapshot]) {
    tx.send_modify(|usage| usage.remote = snapshots.to_vec());
}

fn destination_key(ssh: &str) -> String {
    Sha256::digest(ssh.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn load_cache(path: &Path) -> BTreeMap<String, SavedUsage> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return BTreeMap::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_CACHE_BYTES {
        return BTreeMap::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(mut entries) = serde_json::from_slice::<BTreeMap<String, SavedUsage>>(&bytes) else {
        return BTreeMap::new();
    };
    if entries.len() > MAX_HOSTS {
        return BTreeMap::new();
    }
    entries.retain(|_, saved| saved.result.validate().is_ok());
    entries
}

fn save_cache(path: &Path, values: &BTreeMap<String, SavedUsage>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(values)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(std::io::Error::other("remote usage cache limit exceeded"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp)?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_refresh_retains_durable_usage_and_success_replaces_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("remote-usage-v1.json");
        let data = TranscriptUsageResult {
            source_id: "a".repeat(32),
            collected_at: 1_788_523_200,
            buckets: Vec::new(),
        };
        let mut snapshot = RemoteUsageSnapshot {
            host: "forge".into(),
            name: "Forge".into(),
            status: RemoteUsageStatus::Loading,
            data: None,
        };
        let first = apply_result(&mut snapshot, Some(data.clone())).unwrap();
        let saved = BTreeMap::from([(
            "forge".into(),
            SavedUsage {
                destination: destination_key("you@forge"),
                result: first,
            },
        )]);
        save_cache(&path, &saved).unwrap();
        let loaded = load_cache(&path);
        assert_eq!(loaded["forge"].result, data);
        assert_ne!(
            loaded["forge"].destination,
            destination_key("you@another-machine")
        );
        assert!(apply_result(&mut snapshot, None).is_none());
        assert_eq!(snapshot.status, RemoteUsageStatus::Unavailable);
        assert_eq!(snapshot.data.as_deref(), Some(&data));
        let mut invalid = data.clone();
        invalid.source_id.clear();
        assert!(apply_result(&mut snapshot, Some(invalid)).is_none());
        assert_eq!(snapshot.data.as_deref(), Some(&data));
        let fresh = TranscriptUsageResult {
            collected_at: data.collected_at + 300,
            ..data
        };
        apply_result(&mut snapshot, Some(fresh.clone())).unwrap();
        assert_eq!(snapshot.status, RemoteUsageStatus::Ready);
        assert_eq!(snapshot.data.as_deref(), Some(&fresh));
    }
}
