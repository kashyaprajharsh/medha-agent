use crate::{MANIFEST_FILE, Store};
use std::fs;
use std::path::Path;

pub(crate) fn write_action_package(root: &Path, id: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        format!(
            r#"schema_version = 1
id = "{id}"
name = "Example"
version = "0.1.0"
medha = ">=0.1.0, <0.2.0"

[[components]]
kind = "action"
id = "review"
title = "Review"
description = "Review a change"
prompt = "Review the selected change."
"#
        ),
    )
    .unwrap();
}

#[cfg(unix)]
pub(crate) fn write_hook_package(root: &Path, id: &str, script: &str, timeout_ms: u64) {
    write_hook_package_with(root, id, script, timeout_ms, "");
}

#[cfg(unix)]
pub(crate) fn write_hook_package_with(
    root: &Path,
    id: &str,
    script: &str,
    timeout_ms: u64,
    extra: &str,
) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        format!(
            r#"schema_version = 1
id = "{id}"
name = "Hook example"
version = "0.1.0"
medha = ">=0.1.0, <0.2.0"

[[components]]
kind = "hook"
id = "guard"
points = ["pre_tool"]
failure = "fail_closed"
timeout_ms = {timeout_ms}
entrypoint = {{ program = "hook.sh", args = [] }}
{extra}
"#
        ),
    )
    .unwrap();
    let entrypoint = root.join("hook.sh");
    fs::write(&entrypoint, script).unwrap();
    fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
}

pub(crate) fn store(root: &Path) -> Store {
    Store::new(
        root.join("user"),
        root.join("project"),
        root.join("home/plugins.toml"),
        root.join("state/plugins.toml"),
        "0.1.8",
    )
}
