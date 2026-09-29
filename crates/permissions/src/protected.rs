//! Credential stores (browser profiles, keychains, password managers) no route or grant opens.

use std::path::{Component, Path, PathBuf};

/// Beneath the home directory on every platform.
const HOME_RELATIVE: &[&str] = &[
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    // Whole vendor folders, so every channel (Beta, Dev, Canary) is covered.
    "Library/Application Support/Google",
    "Library/Application Support/Chromium",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Microsoft Edge Beta",
    "Library/Application Support/Microsoft Edge Dev",
    "Library/Application Support/Arc",
    "Library/Application Support/Vivaldi",
    "Library/Application Support/com.operasoftware.Opera",
    "Library/Application Support/Firefox",
    "Library/Application Support/1Password",
    "Library/Application Support/Bitwarden",
    "Library/Group Containers/2BUA8C4S2C.com.1password",
    ".mozilla",
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/google-chrome-unstable",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".config/opera",
    ".config/Bitwarden",
    ".local/share/keyrings",
];

/// Beneath the Windows local and roaming application-data folders.
const APP_DATA_RELATIVE: &[&str] = &[
    "Google",
    "Chromium/User Data",
    "BraveSoftware",
    "Microsoft/Edge",
    "Vivaldi/User Data",
    "Opera Software",
    "Mozilla/Firefox",
    "1Password",
    "Bitwarden",
];

/// Absolute protected roots for the current user.
pub fn protected_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = dirs::home_dir()
        .into_iter()
        .flat_map(|home| {
            HOME_RELATIVE
                .iter()
                .map(move |relative| home.join(relative))
        })
        .collect();
    if cfg!(windows) {
        for base in [dirs::data_local_dir(), dirs::data_dir()]
            .into_iter()
            .flatten()
        {
            paths.extend(APP_DATA_RELATIVE.iter().map(|relative| base.join(relative)));
        }
    }
    paths
}

/// Lexical, disk-free match against cached roots; pass the path that will actually be opened.
pub fn is_protected(path: &Path) -> bool {
    static ROOTS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        protected_paths()
            .iter()
            .flat_map(|root| [Some(lexical(root)), root.canonicalize().ok()])
            .flatten()
            .map(|root| fold(&root))
            .collect()
    });
    let path = fold(&lexical(path));
    roots.iter().any(|root| path.starts_with(root))
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn fold(path: &Path) -> PathBuf {
    if cfg!(any(target_os = "macos", windows)) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}
