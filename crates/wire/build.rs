//! Both the CLI and desktop embed the same backend source identity. Package
//! version and Git HEAD are insufficient for local, uncommitted builds.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn sources(folder: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(folder).expect("read backend sources") {
        let path = entry.expect("source entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        if matches!(name.as_ref(), "target" | ".git" | "node_modules") {
            continue;
        }
        if path.is_dir() {
            sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            || name == "Cargo.toml"
        {
            files.push(path);
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let mut files = vec![root.join("Cargo.toml"), root.join("Cargo.lock")];
    for folder in [root.join("crates"), root.join("apps/desktop/src-tauri/src")] {
        println!("cargo:rerun-if-changed={}", folder.display());
        sources(&folder, &mut files);
    }
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        // Portable relative names, not the machine's checkout location.
        hash.update(
            file.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
        );
        hash.update([0]);
        hash.update(std::fs::read(&file).expect("read backend source"));
        hash.update([0]);
    }
    println!("cargo:rustc-env=MEDHA_BACKEND_BUILD={:x}", hash.finalize());
}
