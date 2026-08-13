use std::fs;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

fn main() {
    let manifest_dir = PathBuf::from("manifests");
    println!("cargo:rerun-if-changed={}", manifest_dir.display());

    // A packaging that ships the crate without its catalog (vendoring, a sparse
    // checkout) should still build; it just cannot claim a catalog identity.
    let Ok(entries) = fs::read_dir(&manifest_dir) else {
        println!(
            "cargo:warning=no Agent catalog at {}; the Engine build identity will not track manifests",
            manifest_dir.display()
        );
        println!("cargo:rustc-env=DIRI_AGENT_CATALOG_BUILD_ID=unknown");
        return;
    };

    let mut paths = entries
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
