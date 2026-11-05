use crate::exec::ExecError;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, PoisonError, Weak},
};

// A folder's entry stays until its cleanup has finished, so a new command waits instead of reusing half-removed files.
static ACTIVE: Mutex<BTreeMap<PathBuf, Weak<Protected>>> = Mutex::new(BTreeMap::new());
static CLEANED: Condvar = Condvar::new();

pub(crate) struct Protected {
    pub _entry: PathBuf,
    pub readonly: Vec<PathBuf>,
    created: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Drop for Protected {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        remove_created(&self._entry, &self.created);
        if active
            .get(&self._entry)
            .is_some_and(|known| known.strong_count() == 0)
        {
            active.remove(&self._entry);
        }
        CLEANED.notify_all();
    }
}

/// Files set up for a command that failed before it could be handed out; the caller holds the lock.
struct Pending<'a> {
    entry: &'a Path,
    created: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        remove_created(self.entry, &self.created);
    }
}

fn remove_created(entry: &Path, created: &[(PathBuf, Option<Vec<u8>>)]) {
    let created_entry = created
        .first()
        .is_some_and(|(path, contents)| path == entry && contents.is_none());
    for (path, contents) in created.iter().rev() {
        match contents {
            Some(contents)
                if std::fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
                    && std::fs::read(path).is_ok_and(|current| current == *contents) =>
            {
                let _ = std::fs::remove_file(path);
            }
            None => {
                let _ = std::fs::remove_dir(path);
            }
            _ => {}
        }
    }
    if created_entry
        && std::fs::symlink_metadata(entry)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
    {
        let _ = std::fs::remove_dir_all(entry);
    }
}

pub(crate) fn prepare(roots: &[PathBuf]) -> Result<Vec<Arc<Protected>>, ExecError> {
    targets(roots)?
        .iter()
        .map(|root| prepare_one(root))
        .collect()
}

fn targets(roots: &[PathBuf]) -> Result<Vec<PathBuf>, ExecError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut targets = Vec::new();
    for root in roots.iter().filter(|root| root.is_dir()) {
        let folder = root.canonicalize().map_err(unavailable)?;
        targets.push(
            git_safety::project_root(&folder, &home)
                .map_err(unavailable)?
                .unwrap_or(folder),
        );
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

#[cfg(target_os = "linux")]
pub(crate) fn held(roots: &[PathBuf]) -> Result<Vec<Arc<Protected>>, ExecError> {
    let roots = targets(roots)?;
    let active = ACTIVE.lock().unwrap();
    let found: Vec<Result<Arc<Protected>, ExecError>> = roots.iter().map(|root| {
        let root = root.canonicalize().map_err(unavailable)?;
        active.get(&root.join(".git")).and_then(Weak::upgrade).ok_or_else(|| ExecError::Unavailable("Git protection must be retained for the command's lifetime; prepare command resources before building".into()))
    }).collect();
    // Anything found is released after the lock, since its cleanup takes the lock.
    drop(active);
    found.into_iter().collect()
}

fn prepare_one(root: &Path) -> Result<Arc<Protected>, ExecError> {
    let root = root.canonicalize().map_err(unavailable)?;
    let entry = root.join(".git");
    let mut active = ACTIVE.lock().unwrap();
    while let Some(known) = active.get(&entry) {
        let Some(known) = known.upgrade() else {
            active = CLEANED.wait(active).unwrap();
            continue;
        };
        let checked = validate_aliases(&known.readonly);
        drop(active);
        return checked.map(|()| known);
    }
    if active.len() >= 128 {
        return Err(ExecError::Unavailable(
            "Git protection exceeds 128 active folders".into(),
        ));
    }
    let mut protected = Pending {
        entry: &entry,
        created: vec![],
    };
    let mut readonly = vec![];
    if !entry.try_exists().map_err(unavailable)? {
        std::fs::create_dir(&entry).map_err(unavailable)?;
        protected.created.push((entry.clone(), None));
    }
    let metadata = std::fs::symlink_metadata(&entry).map_err(unavailable)?;
    if metadata.file_type().is_symlink() {
        return Err(ExecError::Unavailable(
            "Git protection refuses a linked .git entry".into(),
        ));
    }
    if metadata.is_file() {
        readonly.push(entry.clone());
    } else if metadata.is_dir() {
        for (name, contents) in [
            ("config", Some(b"".as_slice())),
            ("commondir", Some(b".\n".as_slice())),
            ("hooks", None),
        ] {
            let path = entry.join(name);
            if !path.try_exists().map_err(unavailable)? {
                match contents {
                    Some(bytes) => {
                        use std::io::Write;
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                            .map_err(unavailable)?;
                        file.write_all(bytes).map_err(unavailable)?;
                    }
                    None => std::fs::create_dir(&path).map_err(unavailable)?,
                }
                protected
                    .created
                    .push((path.clone(), contents.map(Vec::from)));
            }
            let meta = std::fs::symlink_metadata(&path).map_err(unavailable)?;
            if meta.file_type().is_symlink() || (name == "hooks") != meta.is_dir() {
                return Err(ExecError::Unavailable(format!(
                    "Git protection cannot mount {} safely",
                    path.display()
                )));
            }
            readonly.push(path);
        }
    } else {
        return Err(ExecError::Unavailable(
            "Git protection needs a regular .git entry".into(),
        ));
    }
    validate_aliases(&readonly)?;
    let created = std::mem::take(&mut protected.created);
    drop(protected);
    let protected = Arc::new(Protected {
        _entry: entry.clone(),
        readonly,
        created,
    });
    active.insert(entry, Arc::downgrade(&protected));
    Ok(protected)
}

pub(crate) fn validate_aliases(paths: &[PathBuf]) -> Result<(), ExecError> {
    let mut pending = paths.to_vec();
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > 4096 {
            return Err(ExecError::Unavailable(
                "Git control protection exceeds 4096 entries".into(),
            ));
        }
        let meta = std::fs::symlink_metadata(&path).map_err(unavailable)?;
        #[cfg(unix)]
        let aliased = {
            use std::os::unix::fs::MetadataExt;
            meta.is_file() && meta.nlink() != 1
        };
        #[cfg(not(unix))]
        let aliased = false;
        if meta.file_type().is_symlink() || aliased {
            return Err(ExecError::Unavailable(format!(
                "Git protection refuses linked control {}",
                path.display()
            )));
        }
        if meta.is_dir() {
            for child in std::fs::read_dir(&path).map_err(unavailable)? {
                if pending.len().saturating_add(visited) >= 4096 {
                    return Err(ExecError::Unavailable(
                        "Git control protection exceeds 4096 entries".into(),
                    ));
                }
                pending.push(child.map_err(unavailable)?.path());
            }
        }
    }
    Ok(())
}

fn unavailable(error: std::io::Error) -> ExecError {
    ExecError::Unavailable(format!(
        "cannot prepare Git control-file protection: {error}"
    ))
}
