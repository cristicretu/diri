//! Measured, not estimated: what a sandbox's process tree holds in memory
//! and what its private state holds on disk.
//!
//! macOS reads `phys_footprint` (`proc_pid_rusage`, the number Activity
//! Monitor's Memory column shows), which counts a process's dirty and
//! compressed pages but not shared, clean library mappings, so summing it
//! over a tree does not double count the browser binary. Linux reads PSS
//! from `smaps_rollup`, which splits shared pages between their users.

use std::path::Path;

use diri_proto::sandbox::SandboxUsage;

/// `root` and every descendant, depth first, `root` first.
pub fn process_tree(root: u32) -> Vec<u32> {
    let mut tree = vec![root];
    let mut index = 0;
    // A browser has a handful of helpers; the bound only guards a cycle.
    while index < tree.len() && tree.len() < 512 {
        let pid = tree[index];
        for child in children(pid) {
            if !tree.contains(&child) {
                tree.push(child);
            }
        }
        index += 1;
    }
    tree
}

pub fn tree_usage(root: u32, state_dir: &Path) -> SandboxUsage {
    let tree = process_tree(root);
    let live: Vec<u32> = tree
        .into_iter()
        .filter(|pid| memory_bytes(*pid).is_some())
        .collect();
    SandboxUsage {
        memory_bytes: live.iter().filter_map(|pid| memory_bytes(*pid)).sum(),
        processes: live.len() as u32,
        disk_bytes: disk_bytes(state_dir),
    }
}

/// Bytes allocated on disk under `dir` (blocks, not apparent length, so a
/// sparse file counts what it really costs). Symlinks are not followed.
pub fn disk_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            total += meta.blocks() * 512;
        }
        #[cfg(not(unix))]
        {
            total += meta.len();
        }
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(&path)
        {
            stack.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    total
}

#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    let mut buffer = vec![0_i32; 256];
    let bytes = unsafe {
        libc::proc_listchildpids(
            pid as i32,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    // Despite the name, it answers a count of pids on current macOS; older
    // releases answered bytes. Either way never read past the buffer.
    let count = (bytes as usize).min(buffer.len());
    buffer
        .into_iter()
        .take(count)
        .filter(|pid| *pid > 0)
        .map(|pid| pid as u32)
        .collect()
}

#[cfg(target_os = "macos")]
pub fn memory_bytes(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let result = unsafe {
        libc::proc_pid_rusage(
            pid as i32,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    (result == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

#[cfg(target_os = "linux")]
fn children(pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|candidate| {
            std::fs::read_to_string(format!("/proc/{candidate}/stat"))
                .ok()
                .and_then(|stat| {
                    // The command can hold spaces and parentheses; fields
                    // resume after the last ')'.
                    let rest = &stat[stat.rfind(')')? + 2..];
                    rest.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(pid)
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub fn memory_bytes(pid: u32) -> Option<u64> {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    rollup
        .lines()
        .find_map(|line| line.strip_prefix("Pss:"))
        .and_then(|value| {
            value
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|kib| kib * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn children(_pid: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn memory_bytes(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_this_process_and_a_child() {
        let own = std::process::id();
        assert!(memory_bytes(own).is_some_and(|bytes| bytes > 0));
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let tree = process_tree(own);
        assert!(tree.contains(&child.id()), "{tree:?} lacks {}", child.id());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn disk_bytes_counts_allocated_blocks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(disk_bytes(dir.path()) < 64 * 1024);
        std::fs::write(dir.path().join("a"), vec![7_u8; 100_000]).unwrap();
        assert!(disk_bytes(dir.path()) >= 100_000);
    }
}
