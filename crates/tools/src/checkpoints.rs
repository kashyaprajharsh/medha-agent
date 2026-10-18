//! Checkpoints use Git only for the tracked/nonignored inventory. Backup and
//! conditional restore use the same sandbox capabilities as structured writes.

use super::*;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

fn inventory(root: &Path, targets: &[String]) -> Result<Vec<PathBuf>, String> {
    if !root
        .ancestors()
        .any(|folder| std::fs::symlink_metadata(folder.join(".git")).is_ok())
    {
        return folder_inventory(root, targets);
    }
    let mut args = vec![
        "ls-files".into(),
        "-z".into(),
        "--cached".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "--".into(),
    ];
    args.extend_from_slice(targets);
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
        return Err(
            "Cannot enumerate tracked and nonignored files for a complete checkpoint".into(),
        );
    }
    let mut paths = std::collections::BTreeSet::new();
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let path = Path::new(
            std::str::from_utf8(bytes)
                .map_err(|_| "Checkpoint filenames must be UTF-8 on this platform")?,
        );
        if path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("Checkpoint path escaped its workspace".into());
        }
        paths.insert(root.join(path));
    }
    Ok(paths.into_iter().collect())
}

fn folder_inventory(root: &Path, targets: &[String]) -> Result<Vec<PathBuf>, String> {
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
        if targets.is_empty()
            || targets
                .iter()
                .any(|target| entry.path().starts_with(root.join(target)))
        {
            paths.push(entry.into_path());
        }
        if paths.len() > 100_000 {
            return Err("Checkpoint inventory exceeds 100,000 files; command was not run".into());
        }
    }
    Ok(paths)
}

fn regular(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        let meta = std::fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            return Err("Checkpoint cannot safely back up a symlink; command was not run".into());
        }
    }
    if !path.is_file() {
        return Err("Checkpoint source is not a regular file; command was not run".into());
    }
    Ok(())
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

fn remove(sandbox: &WorkspaceSandbox, directory: &Path, manifest: &Value) -> Result<(), String> {
    for row in manifest["files"].as_array().into_iter().flatten() {
        if let Some(id) = row["snapshot"].as_str() {
            sandbox
                .discard_snapshot(id)
                .map_err(|error| error.to_string())?;
        }
    }
    std::fs::remove_dir_all(directory).map_err(|error| error.to_string())
}

fn clean_orphans(sandbox: &WorkspaceSandbox) -> Result<(), String> {
    if sandbox.command_snapshots_dir() == sandbox.snapshots_dir() {
        return Ok(());
    }
    let mut referenced = std::collections::HashSet::new();
    let manifests = sandbox.command_snapshots_dir().join("commands");
    if manifests.is_dir() {
        for entry in std::fs::read_dir(manifests).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let manifest = entry.path().join("manifest.json");
            if !manifest.exists() {
                std::fs::remove_dir_all(entry.path()).map_err(|error| error.to_string())?;
                continue;
            }
            let value: Value = serde_json::from_slice(
                &std::fs::read(manifest).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            for row in value["files"].as_array().into_iter().flatten() {
                if let Some(id) = row["snapshot"].as_str() {
                    referenced.insert(id.to_owned());
                }
            }
        }
    }
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

fn retain(
    sandbox: &WorkspaceSandbox,
    incoming: usize,
    limits: &kernel::mutations::Settings,
) -> Result<(), String> {
    let directory = sandbox.command_snapshots_dir().join("commands");
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let mut manifests = Vec::new();
    for entry in std::fs::read_dir(&directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let value: Value = serde_json::from_slice(
            &std::fs::read(entry.path().join("manifest.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        manifests.push((entry.file_name(), entry.path(), value));
    }
    manifests.sort_by(|a, b| a.0.cmp(&b.0));
    let mut total: usize = manifests
        .iter()
        .map(|(_, _, value)| {
            value["bytes"]
                .as_u64()
                .unwrap_or(limits.checkpoint_bytes as u64) as usize
        })
        .fold(0, usize::saturating_add);
    let mut count = manifests.len();
    for (_, path, value) in manifests {
        if count < limits.checkpoint_count
            && total.saturating_add(incoming) <= limits.checkpoint_bytes
        {
            break;
        }
        total = total.saturating_sub(value["bytes"].as_u64().unwrap_or(0) as usize);
        count -= 1;
        remove(sandbox, &path, &value)?;
    }
    Ok(())
}

fn blocked(guard: &kernel::mutations::Guard, command: &str, reason: String) -> Value {
    let id = guard
        .refuse(command.as_bytes(), "shell checkpoint", &reason, command)
        .ok();
    json!({"error":reason,"changed":false,"command":command,"mutation":{"id":id}})
}

pub(crate) async fn prepare(sandbox: &Arc<WorkspaceSandbox>, args: &Value) -> Result<Value, Value> {
    let Some((guard, _)) = kernel::egress::mutation_context() else {
        return Ok(Value::Null);
    };
    let command = args["command"].as_str().unwrap_or_default();
    let limits = guard.settings();
    let result = async {
        let targets = policy::checkpoint_targets(command).map_err(str::to_owned)?;
        let outside = |target: &str| {
            let path = Path::new(target);
            (path.is_absolute() && !path.starts_with(sandbox.root()))
                || path.components().any(|part| matches!(part, Component::ParentDir))
        };
        let outside_target = targets.as_ref().is_some_and(|targets| {
            targets.iter().any(|target| outside(target))
        });
        let outside_operand = targets.is_some()
            && policy::plain_commands(command).is_some_and(|commands| {
                commands.iter().flatten().any(|word| outside(word))
            });
        if outside_target || outside_operand {
            return Err("Destructive command was not run: its outside-workspace target has no complete checkpoint".into());
        }
        let external = sandbox.exec_backend_label() == "ssh"
            || args["outside_sandbox"] == true
            || args["write_paths"].as_array().is_some_and(|paths| !paths.is_empty());
        if targets.is_some() && external {
            return Err("Destructive command was not run: a complete checkpoint of its actual external or SSH target is unavailable".into());
        }
        if external {
            return Ok(Value::Null);
        }
        if let Some(targets) = &targets {
            let plan = ShellPlan::parse(args, sandbox).map_err(|error| error.to_string())?;
            if plan.workdir != sandbox.root() {
                return Err("Destructive command was not run: checkpointing this alternate working directory is unavailable".into());
            }
            let root = sandbox.root().to_owned();
            let targets = targets.clone();
            let eligible = tokio::task::spawn_blocking(move || inventory(&root, &targets))
                .await.map_err(|error| error.to_string())??;
            // Ignored-only deletion has no snapshot allocation, comparison or notice.
            if eligible.is_empty() {
                return Ok(Value::Null);
            }
        }
        if let Some(value) = guard.checkpoint(sandbox.root())
            && value["checkpoint"].as_str().is_some_and(|id| {
                sandbox.command_snapshots_dir().join("commands").join(id).join("manifest.json").is_file()
            })
        {
            return Ok(value);
        }
        if targets.is_none() {
            return Ok(Value::Null);
        }
        let root = sandbox.root().to_owned();
        let paths = tokio::task::spawn_blocking(move || inventory(&root, &[]))
            .await.map_err(|error| error.to_string())??;
        std::fs::create_dir_all(sandbox.command_snapshots_dir()).map_err(|error| error.to_string())?;
        let lockfile = std::fs::OpenOptions::new().create(true).truncate(false)
            .read(true).write(true).open(sandbox.command_snapshots_dir().join(".checkpoint.lock"))
            .map_err(|error| error.to_string())?;
        let mut process_lock = fd_lock::RwLock::new(lockfile);
        let _exclusive = process_lock.try_write()
            .map_err(|_| "Another process is making a checkpoint; command was not run".to_owned())?;
        clean_orphans(sandbox)?;
        let mut rows = Vec::new();
        let mut bytes = 0;
        let captured = async {
            // Sizes set each file's share of the byte allowance, so copies can run together.
            let mut planned = Vec::new();
            let mut sized = 0usize;
            for path in paths {
                if path.starts_with(sandbox.snapshots_dir()) || path.starts_with(sandbox.command_snapshots_dir()) {
                    continue;
                }
                let meta = match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        rows.push(json!({"path":path,"snapshot":null,"before":"missing","expected":"unknown"}));
                        continue;
                    }
                    meta => meta.map_err(|error| error.to_string())?,
                };
                regular(sandbox.root(), &path)?;
                let remaining = limits.checkpoint_bytes.checked_sub(sized)
                    .ok_or("checkpoint exceeds its byte allowance")?;
                sized = sized.saturating_add(meta.len() as usize);
                planned.push((path, remaining));
            }
            let copies: Vec<_> = futures::stream::iter(planned)
                .map(|(path, remaining)| {
                    let sandbox = Arc::clone(sandbox);
                    tokio::spawn(async move {
                        let copied = match path.to_str() {
                            Some(name) => sandbox.checkpoint_file(name, remaining).await.map_err(|error| error.to_string()),
                            None => Err("Unsupported checkpoint path".to_owned()),
                        };
                        (path, copied)
                    })
                })
                .buffered(8)
                .collect()
                .await;
            let mut failure = None;
            for copy in copies {
                let (path, copied) = match copy {
                    Ok(copy) => copy,
                    Err(error) => {
                        failure.get_or_insert(error.to_string());
                        continue;
                    }
                };
                match copied {
                    Ok((id, digest, size)) => {
                        bytes += size;
                        rows.push(json!({"path":path,"snapshot":id,"before":digest,"expected":"unknown"}));
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
            if bytes > limits.checkpoint_bytes {
                return Err("checkpoint exceeds its byte allowance".into());
            }
            sandbox.seal_command_snapshots().map_err(|error| error.to_string())?;
            retain(sandbox, bytes, &limits)?;
            let id = ulid::Ulid::new().to_string();
            let directory = sandbox.command_snapshots_dir().join("commands").join(&id);
            let value = json!({"id":id,"bytes":bytes,"files":rows,"command":command});
            if let Err(error) = save(&directory, &value) {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
            let prepared = json!({"checkpoint":id,"_checkpoint_files":rows});
            guard.remember_checkpoint(sandbox.root(), prepared.clone());
            Ok(prepared)
        }.await;
        if captured.is_err() {
            for row in rows {
                if let Some(id) = row["snapshot"].as_str() {
                    let _ = sandbox.discard_snapshot(id);
                }
            }
        }
        captured
    }.await;
    result.map_err(|reason| blocked(&guard, command, reason))
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

pub(crate) async fn finish(sandbox: &WorkspaceSandbox, observation: &mut Observation) {
    let Some((guard, _)) = kernel::egress::mutation_context() else {
        return;
    };
    let Some(prepared) = guard.checkpoint(sandbox.root()) else {
        return;
    };
    let result = (|| -> Result<Value, String> {
        let mut rows = BTreeMap::<String, Value>::new();
        for row in prepared["_checkpoint_files"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(path) = row["path"].as_str() {
                rows.insert(path.to_owned(), row.clone());
            }
        }
        for path in inventory(sandbox.root(), &[])? {
            if path.starts_with(sandbox.snapshots_dir())
                || path.starts_with(sandbox.command_snapshots_dir())
            {
                continue;
            }
            if matches!(std::fs::symlink_metadata(&path),Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            regular(sandbox.root(), &path)?;
            let key = path
                .to_str()
                .ok_or("Unsupported checkpoint path")?
                .to_owned();
            rows.entry(key)
                .or_insert_with(|| json!({"path":path,"snapshot":null,"before":"missing"}));
        }
        let before_rows: Vec<_> = rows.values().cloned().collect();
        guard.remember_checkpoint(
            sandbox.root(),
            json!({"checkpoint":prepared["checkpoint"],"_checkpoint_files":before_rows}),
        );
        for (path, row) in &mut rows {
            // A changed link or unsupported target is never followed for undo.
            let path = Path::new(path);
            row["expected"] = json!(if path.exists() {
                regular(sandbox.root(), path)
                    .and_then(|_| digest(path))
                    .unwrap_or_else(|_| "unknown".into())
            } else {
                "missing".into()
            });
        }
        Ok(json!(rows.into_values().collect::<Vec<_>>()))
    })();
    observation.payload["checkpoint"] = prepared["checkpoint"].clone();
    observation.payload["_checkpoint_files"] = result.unwrap_or_else(|error| {
        observation.payload["checkpoint_warning"] = json!(format!(
            "Backup retained; automatic restore cannot prove the final revision: {error}"
        ));
        prepared["_checkpoint_files"].clone()
    });
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
