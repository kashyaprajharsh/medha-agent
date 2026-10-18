use super::*;

#[cfg(unix)]
fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .envs(environment())
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn repository_programs_do_not_run_and_filtered_staging_fails_without_corruption() {
    let scratch = test_support::scratch("medha-git-policy");
    let root = scratch.canonicalize().unwrap();
    assert!(git(&root, &["init", "-q"]).status.success());
    std::fs::write(root.join("a"), "original content\n").unwrap();
    let marker = root.join("executed");
    let program = format!("printf injected > '{}'", marker.display());
    for key in [
        "core.fsmonitor",
        "diff.external",
        "diff.bad.command",
        "diff.bad.textconv",
        "filter.bad.clean",
        "filter.bad.process",
    ] {
        assert!(git(&root, &["config", key, &program]).status.success());
    }
    std::fs::write(root.join(".gitattributes"), "* diff=bad filter=bad\n").unwrap();
    let args = arguments(&root, &["status".into(), "--short".into()]).unwrap();
    let output = Command::new("git")
        .args(args)
        .current_dir(&root)
        .env_clear()
        .envs(environment())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = arguments(&root, &["add".into(), "--".into(), "a".into()]).unwrap();
    let output = Command::new("git")
        .args(args)
        .current_dir(&root)
        .env_clear()
        .envs(environment())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a repository filter was silently skipped during staging"
    );
    assert!(!marker.exists());
    assert!(git(&root, &["ls-files"]).stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("a")).unwrap(),
        "original content\n"
    );
    for sub in ["diff", "show", "log", "blame"] {
        let args = arguments(&root, &[sub.into()]).unwrap();
        assert!(args.contains(&"--no-textconv".into()) && args.contains(&"--no-ext-diff".into()));
    }
}

#[test]
fn child_environment_contains_no_ambient_credentials_or_git_injection() {
    let keys: Vec<_> = environment().into_iter().map(|(key, _)| key).collect();
    for forbidden in [
        "MEDHA_API_KEY",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "GIT_CONFIG_COUNT",
        "GIT_SSH_COMMAND",
        "GIT_EXTERNAL_DIFF",
    ] {
        assert!(!keys.contains(&OsString::from(forbidden)));
    }
}

#[cfg(unix)]
#[test]
fn an_owned_global_filter_survives_the_commands_private_home() {
    let scratch = test_support::scratch("medha-git-owned-filter");
    let home = scratch.join("home");
    let root = scratch.join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    assert!(git(&root, &["init", "-q"]).status.success());
    std::fs::write(
        home.join(".gitconfig"),
        "[filter \"owned\"]\nclean = tr a-z A-Z\nrequired = true\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitattributes"), "note filter=owned\n").unwrap();
    std::fs::write(root.join("note"), "legitimate bytes\n").unwrap();
    let mut env = environment();
    env.retain(|(name, _)| name != "HOME");
    env.push(("HOME".into(), home.into()));
    let args = arguments_with_environment(&root, &["add".into(), "--".into(), "note".into()], env)
        .unwrap();
    assert!(
        args.iter()
            .any(|arg| arg == "filter.owned.clean=tr a-z A-Z")
    );
    let output = Command::new("git")
        .args(args)
        .current_dir(&root)
        .env_clear()
        .envs(environment())
        .env("HOME", scratch.join("private-home"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&root, &["show", ":note"]).stdout, b"LEGITIMATE BYTES\n");
    assert_eq!(
        std::fs::read_to_string(root.join("note")).unwrap(),
        "legitimate bytes\n"
    );
}

#[cfg(unix)]
#[test]
fn a_config_others_can_write_is_never_trusted_and_group_write_needs_a_private_group() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let scratch = test_support::scratch("medha-git-config-mode");
    let root = scratch.join("repo");
    let config = scratch.join("home").join(".gitconfig");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    assert!(git(&root, &["init", "-q"]).status.success());
    std::fs::write(&config, "[filter \"owned\"]\nclean = cat\n").unwrap();
    let origin = format!("file:{}", config.display());
    let trusted = |mode| {
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(mode)).unwrap();
        trusted_origin(b"global", origin.as_bytes(), &root)
    };
    assert!(trusted(0o644));
    assert!(!trusted(0o666), "a config any user can write was trusted");
    let gid = std::fs::metadata(&config).unwrap().gid();
    assert_eq!(trusted(0o664), private_group(gid));
    if unsafe { libc::getegid() } != 0 {
        assert!(!private_group(0), "the root group is shared, never private");
    }
}

#[cfg(unix)]
#[test]
fn a_global_include_of_repository_content_is_not_a_trusted_filter() {
    let scratch = test_support::scratch("medha-git-filter-origin");
    let home = scratch.join("home");
    let root = scratch.join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    assert!(git(&root, &["init", "-q"]).status.success());
    let source = root.join("included-config");
    std::fs::write(
        &source,
        "[filter \"planted\"]\nclean = touch executed; cat\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        format!("[include]\npath = {}\n", source.display()),
    )
    .unwrap();
    let mut env = environment();
    env.retain(|(name, _)| name != "HOME");
    env.push(("HOME".into(), home.into()));
    let args = arguments_with_environment(&root, &["add".into(), "note".into()], env).unwrap();
    assert!(args.iter().any(|arg| arg == "filter.planted.clean="));
    assert!(args.iter().any(|arg| arg == "filter.planted.required=true"));
    assert!(!args.iter().any(|arg| arg.contains("touch executed")));
}

#[test]
#[ignore = "release-only measurement on the real checkout"]
fn release_git_configuration_cost() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "run with --release"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let start = Instant::now();
    for _ in 0..30 {
        std::hint::black_box(arguments(root, &["status".into(), "--short".into()]).unwrap());
    }
    println!(
        "release Git configuration: calls=30, mean_us={}",
        start.elapsed().as_micros() / 30
    );
}
