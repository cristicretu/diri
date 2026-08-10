use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-env-changed=DIRI_REMOTE_BUILD_ID");
    let build_id = std::env::var("DIRI_REMOTE_BUILD_ID").unwrap_or_else(|_| source_build_id());
    assert!(
        !build_id.is_empty()
            && build_id.len() <= 128
            && build_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "DIRI_REMOTE_BUILD_ID must be a safe version path component"
    );
    println!("cargo:rustc-env=DIRI_REMOTE_EFFECTIVE_BUILD_ID={build_id}");
}

fn source_build_id() -> String {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace = manifest.join("../..");
    let roots = [
        manifest.join("Cargo.toml"),
        manifest.join("build.rs"),
        manifest.join("src"),
        workspace.join("Cargo.lock"),
        workspace.join("crates/diri-proto/Cargo.toml"),
        workspace.join("crates/diri-proto/src"),
        workspace.join("crates/diri-pty/Cargo.toml"),
        workspace.join("crates/diri-pty/src"),
        workspace.join("crates/diri-terminal-state/Cargo.toml"),
        workspace.join("crates/diri-terminal-state/src"),
    ];
    let mut files = Vec::new();
    for root in roots {
        collect_files(&root, &mut files);
    }
    files.sort();
    let mut digest = Sha256::new();
    for name in ["TARGET", "PROFILE", "OPT_LEVEL"] {
        digest.update(name.as_bytes());
        digest.update(*b"=");
        digest.update(std::env::var(name).unwrap_or_default().as_bytes());
        digest.update([0]);
    }
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        digest.update(
            path.strip_prefix(&workspace)
                .unwrap_or(&path)
                .as_os_str()
                .as_encoded_bytes(),
        );
        digest.update([0]);
        digest.update(fs::read(&path).unwrap_or_else(|error| {
            panic!("cannot hash Helper source {}: {error}", path.display())
        }));
        digest.update([0]);
    }
    let hash = digest.finalize();
    let suffix = hash[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-dev-{suffix}",
        std::env::var("CARGO_PKG_VERSION").expect("version")
    )
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        files.push(path.to_path_buf());
        return;
    }
    let mut entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("cannot list Helper source {}: {error}", path.display()))
        .map(|entry| entry.expect("source directory entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for entry in entries {
        collect_files(&entry, files);
    }
}
