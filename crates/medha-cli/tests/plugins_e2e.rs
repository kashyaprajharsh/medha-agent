use std::path::Path;
use std::process::{Command, Output};

fn medha(workspace: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_medha"))
        .args(args)
        .current_dir(workspace)
        .env("MEDHA_HOME", home)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn plugin_lifecycle_stays_out_of_model_tool_context() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let home = temp.path().join("home");
    let package = temp.path().join("package");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.toml"),
        r#"schema_version = 1
id = "dev.medha.review"
name = "Review helpers"
version = "0.1.0"
medha = ">=0.1.0, <0.2.0"

[[components]]
kind = "action"
id = "review"
title = "Review"
description = "Review the selected change"
prompt = "Review the selected change."
"#,
    )
    .unwrap();

    let output = medha(
        &workspace,
        &home,
        &["plugins", "install", package.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("(off)"));

    let output = medha(&workspace, &home, &["plugins", "actions"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "No enabled plugin actions.\n");

    let output = medha(
        &workspace,
        &home,
        &["plugins", "enable", "dev.medha.review"],
    );
    assert!(output.status.success(), "{}", stdout(&output));
    let output = medha(&workspace, &home, &["plugins", "actions"]);
    assert!(stdout(&output).contains("dev.medha.review/review"));

    let output = medha(
        &workspace,
        &home,
        &["plugins", "inspect", "dev.medha.review"],
    );
    assert!(output.status.success());
    assert!(stdout(&output).contains("operator action (not a model tool)"));
    assert!(stdout(&output).contains("model tools: none automatically exposed"));

    // Any package drift revokes activation until the new exact hash is enabled.
    std::fs::write(home.join("plugins/dev.medha.review/drift"), "changed").unwrap();
    let output = medha(&workspace, &home, &["plugins", "list"]);
    assert!(stdout(&output).contains("changed (disabled)"));
    let output = medha(&workspace, &home, &["plugins", "actions"]);
    assert_eq!(stdout(&output), "No enabled plugin actions.\n");

    let output = medha(
        &workspace,
        &home,
        &["plugins", "remove", "dev.medha.review"],
    );
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(!home.join("plugins/dev.medha.review").exists());
}

#[test]
fn requested_access_is_shown_and_must_be_granted_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let home = temp.path().join("home");
    let package = temp.path().join("package");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.toml"),
        r#"schema_version = 1
id = "dev.medha.net"
name = "Network helper"
version = "0.1.0"
medha = ">=0.1.0, <0.2.0"

[permissions]
network_hosts = ["api.example.com"]
read_paths = ["src"]

[[components]]
kind = "action"
id = "sync"
title = "Sync"
description = "Sync the tracker"
prompt = "Sync the tracker."
"#,
    )
    .unwrap();
    assert!(
        medha(
            &workspace,
            &home,
            &["plugins", "install", package.to_str().unwrap()]
        )
        .status
        .success()
    );

    let refused = medha(&workspace, &home, &["plugins", "enable", "dev.medha.net"]);
    assert!(!refused.status.success());
    assert!(stdout(&refused).contains("network: allowed for this plugin's processes"));
    assert!(stdout(&refused).contains("read in workspace: src"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--grant"));

    let granted = medha(
        &workspace,
        &home,
        &["plugins", "enable", "dev.medha.net", "--grant"],
    );
    assert!(granted.status.success(), "{}", stdout(&granted));
    let inspect = stdout(&medha(
        &workspace,
        &home,
        &["plugins", "inspect", "dev.medha.net"],
    ));
    assert!(inspect.contains("state: enabled"));
    assert!(inspect.contains("access granted: as requested"));
}
