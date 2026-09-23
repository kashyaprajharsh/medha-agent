use crate::{Error, Scope, io_error};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct StateFile {
    pub(crate) version: u32,
    pub(crate) plugins: BTreeMap<String, Pin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    pub(crate) scope: Scope,
    pub(crate) content_hash: String,
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) grant: Grant,
    /// Medha copied this package into its managed store; only such packages
    /// may be deleted by `remove`.
    #[serde(default)]
    pub(crate) installed: bool,
    /// Where an installed package was fetched from, for update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<Origin>,
    /// The approval that `rollback` restores with the kept previous files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous: Option<Box<Pin>>,
}

impl Pin {
    pub(crate) fn new(scope: Scope, content_hash: String) -> Self {
        Self {
            scope,
            content_hash,
            enabled: false,
            grant: Grant::default(),
            installed: false,
            origin: None,
            previous: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub source: crate::sources::GitSource,
    pub commit: String,
    /// The marketplace entry's manifest fields, reapplied on every update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

/// Access the operator approved for one exact package hash. Every field maps to
/// an enforcer: network to the sandbox network switch, paths to sandbox roots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grant {
    pub network: bool,
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
}

impl Grant {
    /// Secret handles have no broker yet, so a request for one cannot be granted.
    pub fn for_request(
        requested: &medha_extension_api::RequestedPermissions,
    ) -> Result<Self, Error> {
        if !requested.secrets.is_empty() {
            return Err(Error::Ungrantable(format!(
                "it requests secret handles ({}), which this Medha version cannot broker",
                requested.secrets.join(", ")
            )));
        }
        for path in requested.read_paths.iter().chain(&requested.write_paths) {
            workspace_relative(path)?;
        }
        Ok(Self {
            network: !requested.network_hosts.is_empty(),
            read_paths: requested.read_paths.clone(),
            write_paths: requested.write_paths.clone(),
        })
    }

    pub fn is_empty(&self) -> bool {
        !self.network && self.read_paths.is_empty() && self.write_paths.is_empty()
    }

    pub fn read_roots(&self, workspace: &Path) -> Vec<std::path::PathBuf> {
        resolve_roots(workspace, &self.read_paths)
    }

    pub fn write_roots(&self, workspace: &Path) -> Vec<std::path::PathBuf> {
        resolve_roots(workspace, &self.write_paths)
    }
}

fn resolve_roots(workspace: &Path, paths: &[String]) -> Vec<std::path::PathBuf> {
    paths
        .iter()
        .filter_map(|path| workspace_relative(path).ok())
        .map(|relative| workspace.join(relative))
        .collect()
}

/// Requested paths are relative to the workspace and may not leave it.
fn workspace_relative(path: &str) -> Result<std::path::PathBuf, Error> {
    let candidate = Path::new(path);
    let mut relative = std::path::PathBuf::new();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(part) => relative.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(Error::Ungrantable(format!(
                    "path '{path}' must stay inside the workspace"
                )));
            }
        }
    }
    if path.contains('\\') {
        return Err(Error::Ungrantable(format!(
            "path '{path}' must use '/' separators"
        )));
    }
    Ok(relative)
}

pub(crate) fn read_state_file(path: &Path) -> Result<StateFile, Error> {
    parse_state_file(path).map_err(|reason| {
        Error::State(format!(
            "{} {reason}; move it aside to reset plugin approvals",
            path.display()
        ))
    })
}

/// The reason is one line and omits the path, so callers can phrase it.
pub(crate) fn parse_state_file(path: &Path) -> Result<StateFile, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StateFile {
                version: STATE_VERSION,
                plugins: BTreeMap::new(),
            });
        }
        Err(error) => return Err(format!("cannot be read ({error})")),
    };
    let state: StateFile = toml::from_str(&text)
        .map_err(|error| format!("is not valid plugin state ({})", error.message()))?;
    if state.version != STATE_VERSION {
        return Err(format!(
            "uses unsupported plugin state version {}",
            state.version
        ));
    }
    Ok(state)
}

pub(crate) fn write_state_file(path: &Path, state: &StateFile) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::State("plugin state path has no parent".into()))?;
    let temporary = parent.join(format!(
        ".plugins-{}-{}.tmp",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let body = toml::to_string_pretty(state)
        .map_err(|error| Error::State(format!("serializing plugin state: {error}")))?;
    let mut file = File::create(&temporary)
        .map_err(|error| io_error("creating plugin state", &temporary, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("securing plugin state", &temporary, error))?;
    }
    file.write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("writing plugin state", &temporary, error))?;
    drop(file);
    atomic_replace(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> Result<(), Error> {
    fs::rename(source, target).map_err(|error| io_error("publishing plugin state", target, error))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> Result<(), Error> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (source_wide, target_wide) = (wide(source), wide(target));
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io_error(
            "publishing plugin state",
            target,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}
