use std::fs;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

fn main() {
    let manifest_dir = PathBuf::from("manifests");
    println!("cargo:rerun-if-changed={}", manifest_dir.display());

    let mut paths = fs::read_dir(&manifest_dir)
        .expect("read canonical Agent catalog")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut digest = Sha256::new();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path
            .file_name()
            .expect("Agent manifest file name")
            .to_string_lossy();
        let bytes = fs::read(&path).expect("read Agent manifest");
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }

    let catalog_id = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("cargo:rustc-env=DIRI_AGENT_CATALOG_BUILD_ID={catalog_id}");
}
