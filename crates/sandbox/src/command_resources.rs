use crate::{exec::ExecError, git_controls::Protected};
use std::{path::PathBuf, sync::Arc};

#[derive(Default)]
pub struct CommandResources {
    protected_content: Vec<crate::protected_commands::Snapshot>,
    pub(crate) _protected: Vec<Arc<Protected>>,
    #[cfg(windows)]
    snapshot: Vec<crate::git_snapshot::Snapshot>,
}

impl CommandResources {
    pub(crate) fn with_protected_content(
        mut self,
        request: &crate::exec::ExecRequest,
    ) -> Result<Self, ExecError> {
        let command = format!("{} {}", request.program, request.args.join(" "));
        self.protected_content
            .extend(crate::protected_commands::Snapshot::prepare(
                &request.cwd,
                &command,
            )?);
        Ok(self)
    }

    pub(crate) fn git(roots: &[PathBuf]) -> Result<Self, ExecError> {
        Ok(Self {
            protected_content: Vec::new(),
            _protected: crate::git_controls::prepare(roots)?,
            #[cfg(windows)]
            snapshot: Vec::new(),
        })
    }

    #[cfg(windows)]
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
        #[cfg(windows)]
        let mut notices: Vec<_> = self
            .snapshot
            .into_iter()
            .filter_map(crate::git_snapshot::Snapshot::restore)
            .collect();
        #[cfg(not(windows))]
        let mut notices = Vec::new();
        notices.extend(
            self.protected_content
                .into_iter()
                .filter_map(crate::protected_commands::Snapshot::finish),
        );
        (!notices.is_empty()).then(|| notices.join("\n"))
    }
}
