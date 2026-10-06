//! Credential and coordination stores no route or grant opens.

use std::path::{Component, Path, PathBuf};

/// Keys, tokens and shell history beneath the home directory; one list for files and commands.
pub const CREDENTIAL_RELATIVE: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".docker",
    ".kube",
    ".config/gcloud",
    ".config/gh",
    ".config/pip",
    ".config/pnpm",
    ".git-credentials",
    ".gitconfig",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".yarnrc",
    ".yarnrc.yml",
    ".gem/credentials",
    ".gradle/gradle.properties",
    ".m2/settings.xml",
    ".nuget/NuGet.Config",
    ".bash_history",
    ".zsh_history",
    ".python_history",
    ".node_repl_history",
    ".local/share/fish/fish_history",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".medha/credentials.toml",
    ".medha/credentials.lock",
    // The token admits clients, and unlinking the singleton lock would allow
    // two backends to own the same state. Protect the whole control directory.
    ".medha/serve",
];

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
    "Medha/instruction-locks",
];

/// The state folder `MEDHA_HOME` names, when it is set: as closed as `~/.medha` is.
pub fn named_state_dir() -> Option<PathBuf> {
    let named = std::env::var_os("MEDHA_HOME").filter(|value| !value.is_empty())?;
    std::path::absolute(PathBuf::from(named)).ok()
}

/// The credential files of a state folder, wherever that folder is.
pub fn state_secrets(state: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    CREDENTIAL_RELATIVE
        .iter()
        .filter_map(|relative| relative.strip_prefix(".medha/"))
        .map(move |relative| state.join(relative))
}

/// Absolute protected roots for the current user.
pub fn protected_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = dirs::home_dir()
        .into_iter()
        .flat_map(|home| {
            HOME_RELATIVE
                .iter()
                .chain(CREDENTIAL_RELATIVE)
                .map(move |relative| home.join(relative))
        })
        .collect();
    if let Some(state) = named_state_dir() {
        paths.extend(state_secrets(&state));
    }
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
    roots.iter().any(|root| path.starts_with(root)) || is_state_control(&path)
}

/// Medha's own grants and leases. An agent that could unlink a held lease
/// could let another process lock a different inode for the same conversation.
fn is_state_control(path: &Path) -> bool {
    let grants = path
        .file_name()
        .is_some_and(|name| name == "trust.lock" || name == "lock_trust.toml");
    let leases = path
        .components()
        .any(|part| part.as_os_str() == "session-leases");
    (grants || leases)
        && (path.components().any(|part| part.as_os_str() == ".medha")
            || named_state_dir().is_some_and(|state| {
                [Some(lexical(&state)), state.canonicalize().ok()]
                    .into_iter()
                    .flatten()
                    .any(|state| path.starts_with(fold(&state)))
            }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordination_files_are_closed_without_closing_workspace_data() {
        let home = dirs::home_dir().unwrap();
        assert!(is_protected(&home.join(".medha/serve/lock")));
        assert!(is_protected(&home.join(".medha/serve/address")));
        assert!(is_protected(
            &home.join(".medha/projects/folder/session-leases/chat.lock")
        ));
        assert!(is_protected(
            &home.join(".medha/projects/folder/session-leases")
        ));
        assert!(!is_protected(
            &home.join(".medha/projects/folder/events.db")
        ));
        assert!(!is_protected(
            &home.join("project/session-leases/chat.lock")
        ));
    }
}
