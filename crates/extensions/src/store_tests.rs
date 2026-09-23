use super::*;
use crate::test_support::{store, write_action_package};
use std::fs;

fn none() -> Grant {
    Grant::default()
}

fn write_package_requesting(root: &Path, id: &str, permissions: &str) {
    write_action_package(root, id);
    let manifest = root.join(crate::MANIFEST_FILE);
    let text = fs::read_to_string(&manifest).unwrap();
    let text = text.replacen(
        "\n[[components]]",
        &format!("\n[permissions]\n{permissions}\n\n[[components]]"),
        1,
    );
    fs::write(manifest, text).unwrap();
}

#[test]
fn discovered_packages_are_disabled_until_their_exact_hash_is_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("project/example");
    write_action_package(&package, "dev.medha.example");
    let store = store(temp.path());
    let first = store.discover().unwrap();
    assert_eq!(first.plugins[0].activation, Activation::Disabled);

    store
        .enable("dev.medha.example", Some(Scope::Project), &none())
        .unwrap();
    assert_eq!(
        store.discover().unwrap().plugins[0].activation,
        Activation::Enabled
    );

    fs::write(package.join("extra.txt"), "changed").unwrap();
    let discovery = store.discover().unwrap();
    assert_eq!(discovery.plugins[0].activation, Activation::Changed);
    assert!(discovery.notices()[0].contains("changed after it was enabled"));
}

#[test]
fn install_copies_safely_but_does_not_enable() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.installable");
    let store = store(temp.path());
    let installed = store.install(&source).unwrap();
    assert_eq!(installed.manifest.id, "dev.medha.installable");
    let found = store.discover().unwrap();
    assert_eq!(found.plugins.len(), 1);
    assert_eq!(found.plugins[0].scope, Scope::User);
    assert_eq!(found.plugins[0].activation, Activation::Disabled);
    assert!(store.actions().unwrap().is_empty());
    store
        .enable("dev.medha.installable", None, &none())
        .unwrap();
    assert_eq!(
        store.actions().unwrap()[0].id,
        "dev.medha.installable/review"
    );
}

#[test]
fn a_repository_package_can_never_disable_or_replace_a_user_package() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.guard");
    let store = store(temp.path());
    store.install(&source).unwrap();
    store.enable("dev.medha.guard", None, &none()).unwrap();

    write_action_package(&temp.path().join("project/evil"), "dev.medha.guard");
    let discovery = store.discover().unwrap();
    let state = |scope| {
        discovery
            .plugins
            .iter()
            .find(|plugin| plugin.scope == scope)
            .unwrap()
            .activation
    };
    assert_eq!(state(Scope::User), Activation::Enabled);
    assert_eq!(state(Scope::Project), Activation::Shadowed);
    assert!(
        discovery
            .notices()
            .iter()
            .any(|notice| notice.contains("ignored"))
    );
    assert_eq!(store.actions().unwrap().len(), 1);
    assert!(
        store
            .enable("dev.medha.guard", Some(Scope::Project), &none())
            .is_err()
    );
    store.disable("dev.medha.guard", None).unwrap();
    store.enable("dev.medha.guard", None, &none()).unwrap();
}

#[test]
fn two_packages_in_one_scope_collide_and_neither_is_selected() {
    let temp = tempfile::tempdir().unwrap();
    write_action_package(&temp.path().join("project/one"), "dev.medha.collision");
    write_action_package(&temp.path().join("project/two"), "dev.medha.collision");
    let store = store(temp.path());
    let found = store.discover().unwrap();
    assert!(
        found
            .plugins
            .iter()
            .all(|plugin| plugin.activation == Activation::Collision)
    );
    assert!(matches!(
        store.enable("dev.medha.collision", None, &none()),
        Err(Error::Collision(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlinks_are_rejected_before_installation() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.symlink");
    fs::write(temp.path().join("outside"), "secret").unwrap();
    symlink(temp.path().join("outside"), source.join("escape")).unwrap();
    assert!(matches!(
        store(temp.path()).install(&source),
        Err(Error::UnsafePackage(_))
    ));
}

#[test]
fn removal_is_limited_to_packages_medha_installed() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.removable");
    let store = store(temp.path());
    store.install(&source).unwrap();
    store.enable("dev.medha.removable", None, &none()).unwrap();
    store.remove("dev.medha.removable").unwrap();
    assert!(store.discover().unwrap().plugins.is_empty());
    assert!(source.exists());

    let manual = temp.path().join("user/dev.medha.manual");
    write_action_package(&manual, "dev.medha.manual");
    fs::write(manual.join("notes.txt"), "mine").unwrap();
    assert!(matches!(
        store.remove("dev.medha.manual"),
        Err(Error::NotManaged(_))
    ));
    assert!(manual.join("notes.txt").exists());

    write_action_package(&temp.path().join("project/repo"), "dev.medha.repo");
    let error = store.remove("dev.medha.repo").unwrap_err().to_string();
    assert!(error.contains("part of this repository"), "{error}");
}

#[test]
fn user_pins_hold_across_workspaces_and_reinstall_starts_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.shared");
    let first = store(temp.path());
    first.install(&source).unwrap();
    first.enable("dev.medha.shared", None, &none()).unwrap();

    let other_workspace = Store::new(
        temp.path().join("user"),
        temp.path().join("other/project"),
        temp.path().join("home/plugins.toml"),
        temp.path().join("other/state/plugins.toml"),
        "0.1.8",
    );
    assert_eq!(
        other_workspace.discover().unwrap().plugins[0].activation,
        Activation::Enabled
    );
    other_workspace.remove("dev.medha.shared").unwrap();
    other_workspace.install(&source).unwrap();
    assert_eq!(
        first.discover().unwrap().plugins[0].activation,
        Activation::Disabled
    );
}

#[test]
fn corrupt_state_disables_its_scope_without_being_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    write_action_package(&temp.path().join("project/example"), "dev.medha.example");
    let state = temp.path().join("state/plugins.toml");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(&state, "not = [valid").unwrap();
    let store = store(temp.path());

    let discovery = store.discover().unwrap();
    assert_eq!(discovery.plugins[0].activation, Activation::Disabled);
    assert!(discovery.notices()[0].contains("stay off until the file is repaired"));
    assert!(matches!(
        store.enable("dev.medha.example", None, &none()),
        Err(Error::State(_))
    ));
    assert_eq!(fs::read_to_string(&state).unwrap(), "not = [valid");
}

#[test]
fn enabling_requires_approving_exactly_the_requested_access() {
    let temp = tempfile::tempdir().unwrap();
    write_package_requesting(
        &temp.path().join("project/net"),
        "dev.medha.net",
        "network_hosts = [\"api.example.com\"]\nread_paths = [\"src\"]",
    );
    let store = store(temp.path());
    assert!(matches!(
        store.enable("dev.medha.net", None, &none()),
        Err(Error::GrantRequired(_))
    ));
    let requested = store
        .inspect("dev.medha.net", None)
        .unwrap()
        .requested_grant()
        .unwrap();
    assert!(requested.network);
    assert_eq!(requested.read_paths, ["src"]);
    store.enable("dev.medha.net", None, &requested).unwrap();
    let plugin = store.inspect("dev.medha.net", None).unwrap();
    assert_eq!(plugin.activation, Activation::Enabled);
    assert_eq!(plugin.grant, Some(requested));
}

#[test]
fn a_hooks_file_is_listed_disabled_and_managed_like_a_plugin() {
    let temp = tempfile::tempdir().unwrap();
    let hooks = temp.path().join("workspace/.medha/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(
        hooks.join(crate::HOOKS_FILE),
        "[permissions]\nread_paths = [\".\"]\n\n[[hooks]]\nid = \"lint\"\n\
         points = [\"post_tool\"]\nmatcher = [\"edit\"]\n\
         entrypoint = { program = \"npm run lint --silent\", shell = true }\n",
    )
    .unwrap();
    let store = store(temp.path()).with_hook_sources(crate::HookSource::standard(
        &temp.path().join("workspace"),
        &temp.path().join("home"),
        None,
    ));
    let plugin = store.inspect(crate::PROJECT_HOOKS_ID, None).unwrap();
    assert_eq!(plugin.activation, Activation::Disabled);
    assert_eq!(plugin.scope, Scope::Project);
    let grant = plugin.requested_grant().unwrap();
    store.enable(crate::PROJECT_HOOKS_ID, None, &grant).unwrap();
    assert_eq!(
        store
            .inspect(crate::PROJECT_HOOKS_ID, None)
            .unwrap()
            .activation,
        Activation::Enabled
    );
    let error = store
        .remove(crate::PROJECT_HOOKS_ID)
        .unwrap_err()
        .to_string();
    assert!(error.contains("delete"), "{error}");

    fs::write(hooks.join("extra.sh"), "echo").unwrap();
    assert_eq!(
        store
            .inspect(crate::PROJECT_HOOKS_ID, None)
            .unwrap()
            .activation,
        Activation::Changed
    );
}

#[test]
fn secrets_and_paths_outside_the_workspace_cannot_be_granted() {
    let temp = tempfile::tempdir().unwrap();
    write_package_requesting(
        &temp.path().join("project/secret"),
        "dev.medha.secret",
        "secrets = [\"github-token\"]",
    );
    write_package_requesting(
        &temp.path().join("project/escape"),
        "dev.medha.escape",
        "write_paths = [\"../outside\"]",
    );
    let store = store(temp.path());
    for id in ["dev.medha.secret", "dev.medha.escape"] {
        let plugin = store.inspect(id, None).unwrap();
        assert!(matches!(
            plugin.requested_grant(),
            Err(Error::Ungrantable(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn file_modes_are_part_of_the_approval_and_install_normalizes_them() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_action_package(&source, "dev.medha.modes");
    fs::write(source.join("tool"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(source.join("tool"), fs::Permissions::from_mode(0o4777)).unwrap();
    let store = store(temp.path());
    store.install(&source).unwrap();
    let installed = temp.path().join("user/dev.medha.modes");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&installed.join("tool")), 0o755);
    assert_eq!(mode(&installed.join(crate::MANIFEST_FILE)), 0o644);

    store.enable("dev.medha.modes", None, &none()).unwrap();
    fs::set_permissions(
        installed.join(crate::MANIFEST_FILE),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        store.discover().unwrap().plugins[0].activation,
        Activation::Changed
    );
}
