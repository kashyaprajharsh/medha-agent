use super::*;

fn fixture() -> (tempfile::TempDir, ProjectTrust) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    let trust = ProjectTrust::in_home(&root, &root.join("home"), &root.join("state"))
        .unwrap()
        .unwrap();
    (dir, trust)
}

#[test]
fn checkout_identity_does_not_inherit_across_nested_repositories() {
    let (dir, trust) = fixture();
    let child = dir.path().join("src");
    std::fs::create_dir(&child).unwrap();
    assert_eq!(root(&child, &trust.home).unwrap(), Some(trust.root.clone()));
    std::fs::write(child.join(".git"), "gitdir: ../vendor.git").unwrap();
    assert_eq!(
        root(&child, &trust.home).unwrap(),
        Some(child.canonicalize().unwrap())
    );
    trust.accept(&trust.review(&trust.root).unwrap()).unwrap();
    assert!(!trust.allows_content(&child));
    assert!(root(&trust.root, &trust.root).unwrap().is_none());
}

#[test]
fn settings_are_exact_and_revocation_keeps_the_old_record_untouched() {
    let (dir, trust) = fixture();
    let path = dir.path().join("medha.lock");
    std::fs::write(
        &path,
        "[policy]\nautonomy = 'yolo'\n[memory]\nwrite_approval = 'none'\n",
    )
    .unwrap();
    let shown = trust.review(&trust.root).unwrap();
    assert!(!trust.trusted());
    trust.accept(&shown).unwrap();
    let raw = MedhaLock::load(&path).unwrap().unwrap();
    let old_path = trust.record.with_file_name("lock_trust.toml");
    let mut old = lockfile::AcceptedLocks::default();
    old.accept(
        &trust.root.display().to_string(),
        &raw.legacy_risky_settings(),
    );
    old.save(&old_path).unwrap();
    let old_bytes = std::fs::read(&old_path).unwrap();
    trust.revoke().unwrap();
    assert_eq!(std::fs::read(old_path).unwrap(), old_bytes);
    trust.accept(&trust.review(&trust.root).unwrap()).unwrap();
    assert_eq!(
        trust.filter(&trust.root, &raw).memory.write_approval,
        "none"
    );
    std::fs::write(
        &path,
        "[policy]\nautonomy = 'normal'\n[memory]\nwrite_approval = 'none'\n",
    )
    .unwrap();
    let raw = MedhaLock::load(&path).unwrap().unwrap();
    let safe = trust.filter(&trust.root, &raw);
    assert_eq!(safe.policy.autonomy, "careful");
    assert_eq!(safe.memory.write_approval, "none");
    assert!(trust.accept(&shown).is_err());
    trust.revoke().unwrap();
    assert!(!trust.trusted());
    assert_eq!(
        trust.filter(&trust.root, &raw).memory.write_approval,
        "user-scope"
    );
}

#[test]
fn subfolders_share_the_record_without_inheriting_legacy_acceptance() {
    let (dir, trust) = fixture();
    let child = dir.path().join("src");
    std::fs::create_dir(&child).unwrap();
    let other = ProjectTrust::in_home(&child, &trust.home, &trust.state_home)
        .unwrap()
        .unwrap();
    assert_eq!(other.record, trust.record);
    let path = trust.root.join("medha.lock");
    std::fs::write(
        &path,
        "[policy]\nautonomy = 'yolo'\n[memory]\nwrite_approval = 'none'\n",
    )
    .unwrap();
    let raw = MedhaLock::load(&path).unwrap().unwrap();
    let mut old = lockfile::AcceptedLocks::default();
    old.accept(
        &trust.root.display().to_string(),
        &raw.legacy_risky_settings(),
    );
    let old_path = trust.record.with_file_name("lock_trust.toml");
    old.save(&old_path).unwrap();
    let bytes = std::fs::read(&old_path).unwrap();
    assert!(!trust.trusted());
    assert_eq!(trust.filter(&trust.root, &raw).policy.autonomy, "yolo");
    assert_eq!(
        trust.filter(&trust.root, &raw).memory.write_approval,
        "user-scope"
    );
    assert_eq!(other.filter(&child, &raw).policy.autonomy, "careful");
    trust.accept(&trust.review(&trust.root).unwrap()).unwrap();
    assert!(other.allows_content(&child));
    other.revoke().unwrap();
    assert!(!trust.trusted());
    assert_eq!(trust.filter(&trust.root, &raw).policy.autonomy, "careful");
    assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
}

#[test]
fn concurrent_acceptances_keep_the_checkout_trusted() {
    let (dir, trust) = fixture();
    let review = trust.review(dir.path()).unwrap();
    let accepting = trust.clone();
    let worker = std::thread::spawn(move || accepting.accept(&review).unwrap());
    trust.accept(&trust.review(dir.path()).unwrap()).unwrap();
    worker.join().unwrap();
    assert!(trust.trusted());
}

#[cfg(unix)]
#[test]
fn aliases_share_identity_but_copies_and_moves_do_not() {
    let (dir, trust) = fixture();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("link");
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    assert_eq!(root(&alias, &trust.home).unwrap(), Some(trust.root.clone()));
    trust.accept(&trust.review(&trust.root).unwrap()).unwrap();
    let copy = aliases.path().join("copy");
    std::fs::create_dir(&copy).unwrap();
    std::fs::create_dir(copy.join(".git")).unwrap();
    std::fs::copy(&trust.record, copy.join(RECORD)).unwrap();
    let copied = ProjectTrust {
        root: copy.canonicalize().unwrap(),
        record: copy.join(RECORD),
        home: trust.home.clone(),
        state_home: trust.state_home.clone(),
    };
    assert!(!copied.trusted());
    assert!(!trust.allows_content(&copy));
    let moved = aliases.path().join("moved");
    std::fs::rename(dir.path(), &moved).unwrap();
    let relocated = ProjectTrust::in_home(&moved, &trust.home, &trust.state_home)
        .unwrap()
        .unwrap();
    assert!(!relocated.trusted());
    assert_ne!(relocated.record, trust.record);
    std::fs::remove_dir_all(moved).unwrap();
}

#[test]
fn non_git_folders_have_exact_scope_and_home_never_becomes_a_project() {
    let files = tempfile::tempdir().unwrap();
    let parent = files.path().canonicalize().unwrap();
    let child = parent.join("child");
    std::fs::create_dir(&child).unwrap();
    let home = parent.join("home");
    std::fs::create_dir_all(home.join(".git")).unwrap();
    assert_eq!(root(&parent, &home).unwrap(), Some(parent));
    assert_eq!(root(&child, &home).unwrap(), Some(child));
    assert_eq!(root(&home, &home).unwrap(), None);
}

#[test]
fn a_non_git_workspace_can_use_its_assets_without_trusting_a_subfolder_chat() {
    let files = tempfile::tempdir().unwrap();
    let workspace = files.path().canonicalize().unwrap();
    let home = workspace.join("home");
    let trust = ProjectTrust::in_home(&workspace, &home, &workspace.join("state"))
        .unwrap()
        .unwrap();
    let package = workspace.join(".medha/skills/task");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        "---\nname: task\ndescription: Help with a task\n---\nUse these steps.",
    )
    .unwrap();
    let asset_trust = trust.clone();
    let store = tools::SkillStore::new(workspace.join(".medha/skills"), None).with_project_source(
        std::sync::Arc::new(move |folder, asset| {
            asset_trust
                .allows_asset(folder, asset)
                .then(|| asset_trust.cache_dir())
        }),
    );
    let known = ["skill".to_owned()].into_iter().collect();
    assert!(store.load("task", &known).is_err());
    trust.accept(&trust.review(&workspace).unwrap()).unwrap();
    assert_eq!(
        store.load("task", &known).unwrap()["procedure"],
        "Use these steps."
    );
    let subfolder = workspace.join("src");
    std::fs::create_dir(&subfolder).unwrap();
    assert!(trust.allows_asset(&workspace, &subfolder));
    assert!(
        !trust.allows_content(&subfolder),
        "a separate non-Git chat still needs its own exact-folder trust"
    );
    assert!(
        crate::project_session::lsp_config_for_trust(
            &trust,
            &workspace,
            &subfolder,
            &lsp::Config::default()
        )
        .is_some()
    );
    std::fs::write(package.join(".git"), "gitdir: ../../../vendor.git").unwrap();
    assert!(
        store.load("task", &known).is_err(),
        "a nested checkout cannot inherit the workspace's permission"
    );
}

#[test]
fn an_action_shown_for_one_folder_cannot_accept_another_even_with_identical_settings() {
    let (files, first) = fixture();
    let child = first.root.join("src");
    std::fs::create_dir(&child).unwrap();
    let nested = first.root.join("vendor");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    let second = ProjectTrust::in_home(&nested, &first.home, &first.state_home)
        .unwrap()
        .unwrap();
    let shown = first.review(files.path()).unwrap();
    assert_eq!(
        shown.fingerprint,
        second.review(&nested).unwrap().fingerprint
    );
    assert!(
        second
            .accept_for(
                &nested,
                &shown.root,
                &shown.folder,
                shown.fingerprint.clone()
            )
            .is_err()
    );
    assert!(
        first
            .accept_for(
                &child,
                &shown.root,
                &shown.folder,
                shown.fingerprint.clone()
            )
            .is_err()
    );
    assert!(!second.trusted() && !first.trusted());
    first
        .accept_for(files.path(), &shown.root, &shown.folder, shown.fingerprint)
        .unwrap();
    assert!(first.trusted() && !second.trusted());
}

#[test]
fn legacy_fallback_keeps_its_original_grants_without_accepting_new_privileges() {
    struct Quiet;
    impl crate::Notices for Quiet {
        fn say(&self, _: &str) {}
    }
    let (files, trust) = fixture();
    let state = trust.record.parent().unwrap();
    let path = files.path().join("medha.lock");
    std::fs::write(&path, "[policy]\nautonomy = 'yolo'\napprove = []\n[verify]\ncommand = '  cargo test  '\n[memory]\nwrite_approval = 'none'\n[pricing]\ninput_per_mtok = 0\noutput_per_mtok = 2\n").unwrap();
    let lock = MedhaLock::load(&path).unwrap().unwrap();
    let mut legacy = lockfile::AcceptedLocks::default();
    legacy.accept(
        &files.path().display().to_string(),
        &lock.legacy_risky_settings(),
    );
    legacy.save(&state.join("lock_trust.toml")).unwrap();
    let before = std::fs::read(state.join("lock_trust.toml")).unwrap();
    let safe = crate::workspace::apply_lock_trust(lock.clone(), &path, files.path(), state, &Quiet);
    assert_eq!(safe.policy.autonomy, "yolo");
    assert!(safe.policy.approve.is_empty());
    assert_eq!(safe.verify.command.as_deref(), Some("  cargo test  "));
    assert_eq!(safe.memory.write_approval, "user-scope");
    assert_eq!(safe.pricing.input_per_mtok, None);
    assert_eq!(safe.pricing.output_per_mtok, None);
    let elsewhere =
        crate::workspace::apply_lock_trust(lock, &path, &files.path().join("src"), state, &Quiet);
    assert_eq!(elsewhere.policy.autonomy, "careful");
    assert_eq!(
        std::fs::read(state.join("lock_trust.toml")).unwrap(),
        before
    );
    assert!(!trust.trusted());
}
