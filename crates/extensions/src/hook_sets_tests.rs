use super::*;
use std::fs;

fn hooks(root: &Path, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = root.join("hooks");
    for (path, body) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn hook(package: &Package, index: usize) -> &ExtensionComponent {
    &package.manifest.components[index]
}

#[test]
fn a_script_in_an_event_folder_is_a_hook_without_any_config() {
    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(
        temp.path(),
        &[
            (
                "pre-tool/strip-em-dashes.py",
                "#!/usr/bin/env python3\n# medha: matcher=Bash timeout=10\nprint()\n",
            ),
            ("task-completion/check.sh", "echo ok\n"),
            ("pre-tool/README.md", "notes"),
        ],
    );
    let package = load(&dir, PROJECT_HOOKS_ID, "0.1.8").unwrap();
    assert_eq!(package.manifest.components.len(), 2);
    let ExtensionComponent::Hook {
        id,
        points,
        matcher,
        entrypoint,
        timeout_ms,
        protocol,
        workdir,
        ..
    } = hook(&package, 0)
    else {
        panic!("expected a hook");
    };
    assert_eq!(id, "pre-tool-strip-em-dashes");
    assert_eq!(points, &[HookPoint::PreTool]);
    assert_eq!(matcher, &["Bash"]);
    assert_eq!(*timeout_ms, 10_000);
    assert_eq!(*protocol, HookProtocol::ExitStatus);
    assert_eq!(*workdir, HookWorkdir::Workspace);
    assert!(
        entrypoint.shell,
        "a non-executable .py runs through python3"
    );
    assert!(entrypoint.program.starts_with("python3 "));
    assert_eq!(package.manifest.permissions.read_paths, ["."]);
    assert_eq!(package.manifest.permissions.write_paths, ["."]);
}

#[test]
fn headers_and_folder_names_are_checked_not_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(temp.path(), &[("file-change/x.sh", "echo")]);
    let error = load(&dir, PROJECT_HOOKS_ID, "0.1.8")
        .unwrap_err()
        .to_string();
    assert!(error.contains("not an event Medha runs"), "{error}");
    assert!(error.contains("pre-tool"), "{error}");

    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(
        temp.path(),
        &[("post-tool/notify.sh", "# medha: network=true\n")],
    );
    let error = load(&dir, PROJECT_HOOKS_ID, "0.1.8")
        .unwrap_err()
        .to_string();
    assert!(error.contains("list the hosts"), "{error}");

    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(
        temp.path(),
        &[(
            "post-tool/notify.sh",
            "# medha: network=hooks.slack.com failure=ignore\n",
        )],
    );
    let package = load(&dir, PROJECT_HOOKS_ID, "0.1.8").unwrap();
    assert_eq!(
        package.manifest.permissions.network_hosts,
        ["hooks.slack.com"]
    );

    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(temp.path(), &[("pre-tool/tool.bin", "\0")]);
    assert!(load(&dir, PROJECT_HOOKS_ID, "0.1.8").is_err());
}

#[test]
fn an_empty_hooks_folder_is_not_listed() {
    let temp = tempfile::tempdir().unwrap();
    let dir = hooks(temp.path(), &[("pre-tool/.keep", "")]);
    assert!(matches!(
        load(&dir, PROJECT_HOOKS_ID, "0.1.8"),
        Err(Error::NotFound(_))
    ));
}
