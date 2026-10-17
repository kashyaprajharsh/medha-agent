use crate::{exec::ExecError, git_controls::Protected};
use std::{path::PathBuf, sync::Arc};

#[derive(Default)]
pub struct CommandResources {
    pub(crate) _protected: Vec<Arc<Protected>>,
    #[cfg(any(windows, target_os = "linux"))]
    snapshot: Vec<crate::git_snapshot::Snapshot>,
}

impl CommandResources {
    pub(crate) fn git(roots: &[PathBuf]) -> Result<Self, ExecError> {
        Ok(Self {
            _protected: crate::git_controls::prepare(roots)?,
            #[cfg(any(windows, target_os = "linux"))]
            snapshot: Vec::new(),
        })
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn snapshot(roots: &[PathBuf]) -> Result<Self, ExecError> {
        let mut roots: Vec<_> = roots
            .iter()
            .filter(|root| root.is_dir())
            .map(std::fs::canonicalize)
            .collect::<Result<_, _>>()
            .map_err(|error| ExecError::Unavailable(error.to_string()))?;
        roots.sort();
        roots.dedup();
        Ok(Self {
            snapshot: roots
                .iter()
                .map(|root| crate::git_snapshot::Snapshot::new(root))
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }

    pub(crate) fn finish(self) -> Option<String> {
        #[cfg(any(windows, target_os = "linux"))]
        {
            let notices: Vec<_> = self
                .snapshot
                .into_iter()
                .filter_map(crate::git_snapshot::Snapshot::restore)
                .collect();
            return (!notices.is_empty()).then(|| notices.join("\n"));
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        None
    }
}
