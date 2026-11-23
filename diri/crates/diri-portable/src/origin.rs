//! Finding a project on a new Mac by its Git `origin` remote. Reads
//! `.git/config` directly: no `git` process per candidate directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Folders people keep checkouts in, relative to home.
pub const COMMON_ROOTS: [&str; 12] = [
    "code",
    "work",
    "src",
    "dev",
    "projects",
    "Projects",
    "Developer",
    "fun",
    "git",
    "repos",
    "Documents/GitHub",
    "Documents/Code",
];

/// `origin`'s URL for the checkout at `root` (a main checkout or a linked
/// worktree), as written in its config.
pub fn read_origin(root: &Path) -> Option<String> {
    let config = git_common_dir(root)?.join("config");
    let text = fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "url"
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    // A linked worktree: `.git` is "gitdir: <main>/.git/worktrees/<name>".
    let text = fs::read_to_string(&dot_git).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        root.join(gitdir)
    };
    match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => {
            let common = PathBuf::from(common.trim());
            Some(if common.is_absolute() {
                common
            } else {
                gitdir.join(common)
            })
        }
        Err(_) => Some(gitdir),
    }
}

/// A comparable form of a remote URL: `host/owner/repo`, lowercase host,
/// no scheme, user, port, or `.git`. `git@github.com:a/b.git`,
/// `https://github.com/a/b` and `ssh://git@github.com:22/a/b.git` agree.
pub fn normalize_remote(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/').to_owned();
    if let Some(stripped) = url.strip_suffix(".git") {
        url = stripped.to_owned();
    }
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        (host.to_owned(), path.to_owned())
    } else if let Some((authority, path)) = url.split_once(':')
        && !authority.contains('/')
    {
        let host = authority.rsplit('@').next().unwrap_or(authority);
        (host.to_owned(), path.to_owned())
    } else {
        return url;
    };
    format!(
        "{}/{}",
        host.to_ascii_lowercase(),
        path.trim_start_matches('/')
    )
}

/// Checkouts under `roots` (each searched two levels deep), keyed by
/// normalized origin. The first found wins for a duplicate origin.
pub fn scan_checkouts(roots: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let visit = |dir: &Path, found: &mut BTreeMap<String, PathBuf>| {
        if let Some(origin) = read_origin(dir) {
            found
                .entry(normalize_remote(&origin))
                .or_insert_with(|| dir.to_path_buf());
        }
    };
    for root in roots {
        visit(root, &mut found);
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten().take(500) {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_dir()) || hidden(&path) {
                continue;
            }
            visit(&path, &mut found);
            if path.join(".git").exists() {
                continue;
            }
            let Ok(children) = fs::read_dir(&path) else {
                continue;
            };
            for child in children.flatten().take(500) {
                let child = child.path();
                if child.is_dir() && !hidden(&child) {
                    visit(&child, &mut found);
                }
            }
        }
    }
    found
}

fn hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name.starts_with('.') || name == "node_modules" || name == "target")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalize_across_spellings() {
        let expected = "github.com/acme/repo";
        for url in [
            "git@github.com:acme/repo.git",
            "https://github.com/acme/repo",
            "https://user@GitHub.com/acme/repo.git/",
            "ssh://git@github.com:22/acme/repo.git",
        ] {
            assert_eq!(normalize_remote(url), expected, "{url}");
        }
    }

    #[test]
    fn finds_checkouts_by_origin_including_worktrees() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("code/repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(
            repo.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:acme/repo.git\n",
        )
        .unwrap();
        let worktree = home.path().join("wt");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(
            read_origin(&worktree).as_deref(),
            Some("git@github.com:acme/repo.git")
        );
        let found = scan_checkouts(&[home.path().join("code")]);
        assert_eq!(found.get("github.com/acme/repo"), Some(&repo));
    }
}
