//! Before a shell command that may write, the workspace is backed up so undo can restore it.
//! Git gives the tracked and nonignored inventory; copies use the sandbox's capabilities.
//! A file that cannot be copied is named in a note and never stops the command.

use super::*;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// How long a command waits for another process's backup before going without one.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

fn inventory(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root
        .ancestors()
        .any(|folder| std::fs::symlink_metadata(folder.join(".git")).is_ok())
    {
        return folder_inventory(root);
    }
    let args = [
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]
    .map(String::from);
    let mut args = git_safety::raw_arguments(root, &args)?;
    args.insert(0, "--literal-pathspecs".into());
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .envs(git_safety::environment());
    let output = sandbox::exec::run_command_bounded_sync(
        command,
        std::time::Duration::from_secs(10),
        16 * 1024 * 1024,
        4096,
    )
    .map_err(|e| e.to_string())?;
    if output.status != Some(0) || output.timed_out || output.stdout_truncated {
        return Err("the workspace's files could not be listed".into());
    }
    let mut paths = std::collections::BTreeSet::new();
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let path = Path::new(
            std::str::from_utf8(bytes).map_err(|_| "a file name is not UTF-8 on this platform")?,
        );
        if path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("a listed file escaped the workspace".into());
        }
        paths.insert(root.join(path));
    }
    Ok(paths.into_iter().collect())
}

fn folder_inventory(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .build()
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        paths.push(entry.into_path());
        if paths.len() > 100_000 {
            return Err("the workspace has more than 100,000 files".into());
        }
    }
    Ok(paths)
}

/// A regular file reached through real folders only; a link is never followed for undo.
fn regular(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        if std::fs::symlink_metadata(&current).map_or(true, |meta| meta.file_type().is_symlink()) {
            return false;
        }
    }
    path.is_file()
}

/// Size, change times and file id: equal values mean the bytes were not rewritten.
fn stamp(meta: &std::fs::Metadata) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        json!([
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.ino()
        ])
    }
    #[cfg(not(unix))]
    {
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_nanos().to_string());
        json!([meta.len(), modified])
    }
}

fn save(directory: &Path, value: &Value) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let temporary = directory.join(format!(".{}", ulid::Ulid::new()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        file_ops::replace(&temporary, &directory.join("manifest.json")).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Every manifest, oldest first by its ULID name.
fn manifests(sandbox: &WorkspaceSandbox) -> Result<Vec<(PathBuf, Value)>, String> {
    let directory = sandbox.command_snapshots_dir().join("commands");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let manifest = entry.path().join("manifest.json");
        let value = std::fs::read(&manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        match value {
            Some(value) => found.push((entry.path(), value)),
            // A manifest never written belongs to a backup that did not finish.
            None => std::fs::remove_dir_all(entry.path()).map_err(|e| e.to_string())?,
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Deletes copies no kept manifest names; copies are shared between backups of unchanged files.
fn clean_orphans(sandbox: &WorkspaceSandbox) -> Result<(), String> {
    if sandbox.command_snapshots_dir() == sandbox.snapshots_dir() {
        return Ok(());
    }
    let referenced: HashSet<String> = manifests(sandbox)?
        .iter()
        .flat_map(|(_, value)| value["files"].as_array().cloned().unwrap_or_default())
        .filter_map(|row| row["snapshot"].as_str().map(str::to_owned))
        .collect();
    for entry in
        std::fs::read_dir(sandbox.command_snapshots_dir()).map_err(|error| error.to_string())?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .filter(|name| ulid::Ulid::from_string(name).is_ok())
        else {
            continue;
        };
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_file()
            && !referenced.contains(&format!("command:{id}"))
        {
            std::fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Drops the oldest manifests past the count or byte allowance; their unshared copies go with them.
fn retain(
    sandbox: &WorkspaceSandbox,
    incoming: usize,
    limits: &kernel::mutations::Settings,
) -> Result<(), String> {
    let kept = manifests(sandbox)?;
    let mut total: usize = kept
        .iter()
        .map(|(_, value)| value["bytes"].as_u64().unwrap_or(0) as usize)
        .fold(0, usize::saturating_add);
    let mut count = kept.len();
    for (path, value) in kept {
        if count < limits.checkpoint_count
            && total.saturating_add(incoming) <= limits.checkpoint_bytes
        {
            break;
        }
        total = total.saturating_sub(value["bytes"].as_u64().unwrap_or(0) as usize);
        count -= 1;
        std::fs::remove_dir_all(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn lock(sandbox: &WorkspaceSandbox) -> Result<fd_lock::RwLock<std::fs::File>, String> {
    let lockfile = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(sandbox.command_snapshots_dir().join(".checkpoint.lock"))
        .map_err(|error| error.to_string())?;
    Ok(fd_lock::RwLock::new(lockfile))
}

/// The note a command carries when undo cannot cover all of what it changes.
fn note(text: String) -> Value {
    json!({ "_checkpoint_note": text })
}

pub(crate) async fn prepare(sandbox: &Arc<WorkspaceSandbox>, args: &Value) -> Result<Value, Value> {
    let Some((guard, _)) = kernel::egress::mutation_context() else {
        return Ok(Value::Null);
    };
    let command = args["command"].as_str().unwrap_or_default();
    if shell_reader::read(command).reads_only() {
        return Ok(Value::Null);
    }
    if sandbox.exec_backend_label() == "ssh" {
        return Ok(note(
            "No backup was made: the command runs on the SSH host, so undo cannot restore what it changes.".into(),
        ));
    }
    let outside = args["outside_sandbox"] == true
        || args["write_paths"]
            .as_array()
            .is_some_and(|paths| !paths.is_empty());
    let mut prepared = match guard.checkpoint(sandbox.root()) {
        Some(value)
            if value["_checkpoint_manifest"]
                .as_str()
                .is_some_and(|path| Path::new(path).is_file()) =>
        {
            json!({ "checkpoint": value["checkpoint"], "_checkpoint_manifest": value["_checkpoint_manifest"] })
        }
        _ => match capture(sandbox, command, &guard.settings()).await {
            Ok((full, skipped)) => {
                guard.remember_checkpoint(sandbox.root(), full.clone());
                let mut prepared = json!({ "checkpoint": full["checkpoint"], "_checkpoint_manifest": full["_checkpoint_manifest"] });
                if !skipped.is_empty() {
                    prepared["_checkpoint_note"] = json!(format!(
                        "Not backed up, so undo cannot restore them: {}.",
                        skipped.join(", ")
                    ));
                }
                prepared
            }
            Err(reason) => note(format!(
                "No backup was made before this command ({reason}), so undo cannot restore what it changes."
            )),
        },
    };
    if outside {
        let joined = [
            prepared["_checkpoint_note"].as_str().unwrap_or_default(),
            "Folders outside the workspace are not backed up; undo restores workspace files only.",
        ]
        .join(" ");
        prepared["_checkpoint_note"] = json!(joined.trim());
    }
    Ok(prepared)
}

/// Copies every file that changed since the last backup and reuses the rest.
async fn capture(
    sandbox: &Arc<WorkspaceSandbox>,
    command: &str,
    limits: &kernel::mutations::Settings,
) -> Result<(Value, Vec<String>), String> {
    let root = sandbox.root().to_owned();
    let paths = tokio::task::spawn_blocking(move || inventory(&root))
        .await
        .map_err(|error| error.to_string())??;
    std::fs::create_dir_all(sandbox.command_snapshots_dir()).map_err(|error| error.to_string())?;
    let mut process_lock = lock(sandbox)?;
    // Another process's backup is waited for, not a reason to refuse.
    let started = std::time::Instant::now();
    let _exclusive = loop {
        match process_lock.try_write() {
            Ok(held) => break held,
            Err(_) if started.elapsed() < LOCK_WAIT => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(_) => {
                return Err(
                    "another backup of this workspace did not finish within 30 seconds".into(),
                );
            }
        }
    };
    clean_orphans(sandbox)?;
    let previous: BTreeMap<String, Value> = manifests(sandbox)?
        .pop()
        .and_then(|(_, value)| value["files"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| Some((row["path"].as_str()?.to_owned(), row)))
        .collect();
    let (mut rows, planned, mut skipped) = plan(sandbox, paths, &previous, limits);
    let copied = copy(sandbox, planned).await;
    let mut bytes = 0;
    for (path, stamp, result) in copied {
        match result {
            Ok((id, digest, size)) => {
                bytes += size;
                rows.push(json!({"path":path,"snapshot":id,"before":digest,"expected":"unknown","stat":stamp}));
            }
            Err(_) => skipped.push(display(sandbox, &path)),
        }
    }
    sandbox
        .seal_command_snapshots()
        .map_err(|error| error.to_string())?;
    retain(sandbox, bytes, limits)?;
    let id = ulid::Ulid::new().to_string();
    let directory = sandbox.command_snapshots_dir().join("commands").join(&id);
    let manifest = directory.join("manifest.json");
    if let Err(error) = save(
        &directory,
        &json!({"id":id,"bytes":bytes,"files":rows,"command":command}),
    ) {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error);
    }
    clean_orphans(sandbox)?;
    let full = json!({"checkpoint":id,"_checkpoint_manifest":manifest,"_checkpoint_files":rows});
    Ok((full, skipped))
}

type Planned = Vec<(PathBuf, Value, usize)>;

/// Rows reused from the last backup, files to copy with their share of the allowance, and files left out.
fn plan(
    sandbox: &WorkspaceSandbox,
    paths: Vec<PathBuf>,
    previous: &BTreeMap<String, Value>,
    limits: &kernel::mutations::Settings,
) -> (Vec<Value>, Planned, Vec<String>) {
    let (mut rows, mut planned, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    let mut sized = 0usize;
    for path in paths {
        if path.starts_with(sandbox.snapshots_dir())
            || path.starts_with(sandbox.command_snapshots_dir())
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !regular(sandbox.root(), &path) {
            skipped.push(display(sandbox, &path));
            continue;
        }
        let stamp = stamp(&meta);
        let key = path.to_string_lossy().into_owned();
        if let Some(row) = previous.get(&key)
            && row["stat"] == stamp
            && row["snapshot"]
                .as_str()
                .is_some_and(|id| sandbox.snapshot_exists(id))
        {
            rows.push(row.clone());
            continue;
        }
        let len = meta.len() as usize;
        match limits.checkpoint_bytes.checked_sub(sized) {
            Some(remaining) if len <= remaining => {
                sized += len;
                planned.push((path, stamp, remaining));
            }
            _ => skipped.push(display(sandbox, &path)),
        }
    }
    (rows, planned, skipped)
}

/// Copies run eight at a time; a file that fails is left out, not the backup.
async fn copy(
    sandbox: &Arc<WorkspaceSandbox>,
    planned: Planned,
) -> Vec<(PathBuf, Value, Result<(String, String, usize), String>)> {
    futures::stream::iter(planned)
        .map(|(path, stamp, remaining)| {
            let sandbox = Arc::clone(sandbox);
            async move {
                let copied = match path.to_str() {
                    Some(name) => sandbox
                        .checkpoint_file(name, remaining)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("unsupported path".to_owned()),
                };
                (path, stamp, copied)
            }
        })
        .buffered(8)
        .collect()
        .await
}

fn display(sandbox: &WorkspaceSandbox, path: &Path) -> String {
    path.strip_prefix(sandbox.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn digest(path: &Path) -> Result<String, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("missing".into()),
        Err(error) => return Err(error.to_string()),
    };
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Records what the command changed relative to the backup; untouched files are not re-read.
pub(crate) async fn finish(sandbox: &WorkspaceSandbox, observation: &mut Observation) {
    let Some((guard, _)) = kernel::egress::mutation_context() else {
        return;
    };
    let Some(prepared) = guard.checkpoint(sandbox.root()) else {
        return;
    };
    let mut rows: BTreeMap<String, Value> = prepared["_checkpoint_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["path"].as_str()?.to_owned(), row.clone())))
        .collect();
    match changed(sandbox, &mut rows) {
        Ok(changed) => observation.payload["_checkpoint_files"] = json!(changed),
        Err(error) => {
            observation.payload["_checkpoint_note"] = json!(format!(
                "The backup is kept, but undo cannot confirm what this command changed: {error}"
            ));
        }
    }
    observation.payload["checkpoint"] = prepared["checkpoint"].clone();
    let mut remembered = prepared;
    remembered["_checkpoint_files"] = json!(rows.into_values().collect::<Vec<_>>());
    guard.remember_checkpoint(sandbox.root(), remembered);
}

/// Rows whose current bytes differ from the backup; files new since it are added as missing before.
fn changed(
    sandbox: &WorkspaceSandbox,
    rows: &mut BTreeMap<String, Value>,
) -> Result<Vec<Value>, String> {
    let mut changed = Vec::new();
    let mut present = HashSet::new();
    for path in inventory(sandbox.root())? {
        if path.starts_with(sandbox.snapshots_dir())
            || path.starts_with(sandbox.command_snapshots_dir())
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !regular(sandbox.root(), &path) {
            continue;
        }
        let key = path.to_string_lossy().into_owned();
        present.insert(key.clone());
        let row = rows
            .entry(key)
            .or_insert_with(|| json!({"path":path,"snapshot":null,"before":"missing","stat":null}));
        if row["stat"] == stamp(&meta) {
            continue;
        }
        let expected = digest(&path)?;
        if row["before"] == expected.as_str() {
            // Touched but not changed: later commands need not read it again.
            row["stat"] = stamp(&meta);
        } else {
            let mut row = row.clone();
            row["expected"] = json!(expected);
            changed.push(row);
        }
    }
    for (key, row) in rows.iter() {
        if !present.contains(key) && row["before"] != "missing" {
            let mut row = row.clone();
            row["expected"] = json!("missing");
            changed.push(row);
        }
    }
    Ok(changed)
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
