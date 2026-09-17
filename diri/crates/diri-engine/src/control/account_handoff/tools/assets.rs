//! Local Codex plugin code is shared separately from account-owned runtime state.
//! Link only cache assets; never share app-server state or authentication files.
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Link {
    source: PathBuf,
    target: PathBuf,
}
struct Budget {
    entries: usize,
    bytes: u64,
}
fn error() -> ControlError {
    ControlError::bad_request(
        "The local Codex plugin caches conflict or cannot be shared safely. Keep their installed versions consistent before switching.",
    )
}

fn owned(path: &Path) -> Result<fs::Metadata, ControlError> {
    let metadata = fs::metadata(path).map_err(|_| error())?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o022 != 0 {
        return Err(error());
    }
    Ok(metadata)
}

fn plan(handoffs: &[PreparedHandoff]) -> Result<Vec<Link>, ControlError> {
    let mut links = BTreeMap::new();
    let mut visited = std::collections::HashSet::new();
    let mut budget = Budget {
        entries: 0,
        bytes: 0,
    };
    for handoff in handoffs {
        if handoff.source.kind != AgentKind::CODEX
            || handoff.source.host.is_some()
            || handoff.source_location.root == handoff.target_location.root
            || !visited.insert(handoff.source_location.root.clone())
        {
            continue;
        }
        let source = handoff.source_location.root.join("plugins/cache");
        let target = handoff.target_location.root.join("plugins/cache");
        match fs::symlink_metadata(&source) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(error()),
            Ok(_) => (),
        }
        walk(&source, &target, &mut links, &mut budget, 0)?;
    }
    Ok(links.into_values().collect())
}

fn walk(
    source: &Path,
    target: &Path,
    links: &mut BTreeMap<PathBuf, Link>,
    budget: &mut Budget,
    depth: usize,
) -> Result<(), ControlError> {
    budget.entries += 1;
    if budget.entries > 4096 || depth > 12 {
        return Err(error());
    }
    let source = source.canonicalize().map_err(|_| error())?;
    let source_meta = owned(&source)?;
    if !source_meta.is_dir() && !source_meta.is_file() {
        return Err(error());
    }
    if let Some(previous) = links.get(target) {
        // Two source accounts may share a cache; distinct caches must be reconciled explicitly.
        return if previous.source == source {
            Ok(())
        } else {
            Err(error())
        };
    }
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            links.insert(
                target.to_owned(),
                Link {
                    source,
                    target: target.to_owned(),
                },
            );
            Ok(())
        }
        Err(_) => Err(error()),
        Ok(_) => {
            let resolved = target.canonicalize().map_err(|_| error())?;
            if resolved == source {
                return Ok(());
            }
            let target_meta = owned(target)?;
            if source_meta.is_dir() && target_meta.is_dir() {
                for entry in fs::read_dir(&source).map_err(|_| error())? {
                    let entry = entry.map_err(|_| error())?;
                    walk(
                        &entry.path(),
                        &target.join(entry.file_name()),
                        links,
                        budget,
                        depth + 1,
                    )?;
                }
                Ok(())
            } else if source_meta.is_file() && target_meta.is_file() {
                budget.bytes = budget
                    .bytes
                    .saturating_add(source_meta.len())
                    .saturating_add(target_meta.len());
                if source_meta.len() != target_meta.len() || budget.bytes > 128 * 1024 * 1024 {
                    return Err(error());
                }
                use sha2::{Digest, Sha256};
                let digest = |path: &Path| -> Result<_, ControlError> {
                    let input = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                        .open(path)
                        .map_err(|_| error())?;
                    let metadata = input.metadata().map_err(|_| error())?;
                    if !metadata.is_file()
                        || metadata.uid() != unsafe { libc::geteuid() }
                        || metadata.len() > 128 * 1024 * 1024
                    {
                        return Err(error());
                    }
                    let mut input = input.take(metadata.len() + 1);
                    let mut total = 0_u64;
                    let mut hash = Sha256::new();
                    let mut buffer = [0_u8; 65536];
                    loop {
                        let count = input.read(&mut buffer).map_err(|_| error())?;
                        if count == 0 {
                            break;
                        }
                        total += count as u64;
                        if total > metadata.len() {
                            return Err(error());
                        }
                        hash.update(&buffer[..count]);
                    }
                    Ok(hash.finalize())
                };
                if digest(&source)? == digest(target)? {
                    Ok(())
                } else {
                    Err(error())
                }
            } else {
                Err(error())
            }
        }
    }
}

pub(super) fn preflight(handoffs: &[PreparedHandoff]) -> Result<(), ControlError> {
    plan(handoffs)?;
    Ok(())
}

pub(super) fn install(handoffs: &[PreparedHandoff]) -> Result<(), ControlError> {
    for link in plan(handoffs)? {
        // At most plugins and cache are missing above a planned link; the profile
        // root has already been validated by the Engine account binding.
        let mut missing = Vec::new();
        let mut parent = link.target.parent().ok_or_else(error)?;
        while fs::symlink_metadata(parent).is_err() {
            missing.push(parent.to_owned());
            parent = parent.parent().ok_or_else(error)?;
        }
        owned(parent)?;
        for parent in missing.iter().rev() {
            private_directory(parent, true)?;
        }
        symlink(&link.source, &link.target).map_err(|_| error())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_plugins_share_assets_without_replacing_existing_versions() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir_all(source.join("plugin/1")).unwrap();
        fs::write(source.join("plugin/1/code"), b"fixture").unwrap();
        let mut links = BTreeMap::new();
        let mut budget = Budget {
            entries: 0,
            bytes: 0,
        };
        walk(&source, &target, &mut links, &mut budget, 0).unwrap();
        assert_eq!(links.len(), 1);
        let link = links.into_values().next().unwrap();
        symlink(link.source, link.target).unwrap();
        let mut links = BTreeMap::new();
        walk(&target, &source, &mut links, &mut budget, 0).unwrap();
        assert!(
            links.is_empty(),
            "switching back must not create a symlink cycle"
        );
        fs::remove_file(&target).unwrap();
        fs::create_dir_all(target.join("plugin/1")).unwrap();
        fs::write(target.join("plugin/1/code"), b"different").unwrap();
        assert!(walk(&source, &target, &mut links, &mut budget, 0).is_err());
        assert_eq!(
            fs::read(target.join("plugin/1/code")).unwrap(),
            b"different"
        );
    }
}
