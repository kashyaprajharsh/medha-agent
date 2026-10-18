use super::*;
#[cfg(unix)]
use std::fs;
use std::process::Command;

#[cfg(unix)]
#[test]
fn cloned_attributes_cannot_execute_global_filters_or_checkout_hooks() {
    let scratch = test_support::scratch("plugin-fetch-filter");
    let root = scratch.canonicalize().unwrap();
    let home = root.join("home");
    let repo = root.join("repo");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env_clear()
            .envs(environment())
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    fs::write(repo.join("note"), "original bytes").unwrap();
    fs::write(repo.join(".gitattributes"), "note filter=planted\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "initial",
    ]);
    let marker = root.join("executed");
    fs::write(home.join(".gitconfig"),format!("[filter \"planted\"]\nsmudge = touch '{}'; cat\nrequired = true\n[credential]\nhelper = operator-helper\n",marker.display())).unwrap();
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(repo.join("note")).unwrap();
    let mut env = environment();
    env.retain(|(name, _)| name != "HOME");
    env.push(("HOME".into(), home.into()));
    let args = arguments_with_environment(
        &repo,
        &["checkout".into(), "HEAD".into(), "--".into(), "note".into()],
        env.clone(),
    )
    .unwrap();
    assert!(!args.iter().any(|arg| arg == "credential.helper="));
    assert!(args.iter().any(|arg| arg == "filter.planted.smudge="));
    let output = Command::new("git")
        .args(args)
        .current_dir(&repo)
        .env_clear()
        .envs(env)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(repo.join("note")).unwrap(),
        "original bytes"
    );
    assert!(!marker.exists());
}

#[test]
fn fetch_environment_excludes_agent_credentials_and_unknown_transports_are_disabled() {
    let env = environment();
    for key in [
        "MEDHA_API_KEY",
        "OPENAI_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_TEMPLATE_DIR",
    ] {
        assert!(!env.iter().any(|(name, _)| name == key));
    }
    let scratch = test_support::scratch("plugin-fetch-transport");
    let args = arguments(
        &scratch,
        &[
            "clone".into(),
            "--".into(),
            "ext::touch planted".into(),
            "checkout".into(),
        ],
    )
    .unwrap();
    assert!(args.iter().any(|arg| arg == "protocol.allow=never"));
    let output = Command::new("git")
        .args(args)
        .current_dir(&scratch)
        .env_clear()
        .envs(env)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!scratch.join("planted").exists());
}
