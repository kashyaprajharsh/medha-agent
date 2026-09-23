use super::*;
use crate::tui_tea::Item;
use std::path::Path;

struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    model: Model,
    tx: UnboundedSender<TuiEvent>,
    _rx: tokio::sync::mpsc::UnboundedReceiver<TuiEvent>,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut model = Model::new(
        "m".into(),
        None,
        kernel::ReasoningConfig::default(),
        lockfile::UiConfig::default(),
        std::collections::HashMap::new(),
        std::sync::Arc::new(sandbox::WorkspaceSandbox::new_jailed(&workspace).unwrap()),
    );
    model.plugins = Some(
        Store::new(
            root.join("home/plugins"),
            workspace.join(".medha/plugins"),
            root.join("home/plugins.toml"),
            root.join("state/plugins.toml"),
            "0.1.8",
        )
        .with_hook_sources(extensions::HookSource::standard(
            &workspace,
            &root.join("home"),
            None,
        )),
    );
    model.plugin_markets = Some(extensions::sources::Marketplaces::new(
        root.join("home/markets.toml"),
    ));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    Fixture {
        _temp: temp,
        root,
        model,
        tx,
        _rx: rx,
    }
}

fn write_package(root: &Path, id: &str, permissions: &str) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("plugin.toml"),
        format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"Example\"\nversion = \"0.1.0\"\n\
             medha = \">=0.1.0, <0.2.0\"\n{permissions}\n\n[[components]]\nkind = \"action\"\n\
             id = \"review\"\ntitle = \"Review\"\ndescription = \"Review a change\"\n\
             prompt = \"Review the current diff.\"\n"
        ),
    )
    .unwrap();
}

fn labels(model: &Model) -> Vec<String> {
    model.picker.as_ref().unwrap().kind.labels()
}

fn select(model: &mut Model, row: usize) {
    model.picker.as_mut().unwrap().selected = row;
}

fn notices(model: &Model) -> String {
    model
        .items
        .iter()
        .filter_map(|item| match &item.item {
            Item::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn install(f: &mut Fixture, id: &str, permissions: &str) {
    let package = f.root.join(format!("package-{id}"));
    write_package(&package, id, permissions);
    run_command(
        &mut f.model,
        &format!("install {}", package.display()),
        &f.tx,
    );
}

#[test]
fn a_plugin_that_asks_for_nothing_is_on_right_after_install() {
    let mut f = fixture();
    run_command(&mut f.model, "", &f.tx);
    let rows = labels(&f.model);
    assert!(rows[0].starts_with("➕ Install: owner/repo"));
    assert!(rows[1].starts_with("🔎 Discover"));
    assert!(rows[2].starts_with("🩺"));

    install(&mut f, "dev.me.kit", "");
    assert!(notices(&f.model).contains("installed dev.me.kit 0.1.0"));
    assert!(labels(&f.model)[LIST_HEAD].starts_with("● dev.me.kit"));
    assert_eq!(f.model.picker.as_ref().unwrap().selected, LIST_HEAD);
    assert!(notices(&f.model).contains("enabled dev.me.kit — active now"));

    handle_key(&mut f.model, KeyCode::Char(' '), &f.tx);
    assert!(labels(&f.model)[LIST_HEAD].starts_with("○ dev.me.kit"));
    assert_eq!(
        f.model.picker.as_ref().unwrap().selected,
        LIST_HEAD,
        "the cursor stays on the toggled plugin"
    );
}

#[test]
fn requested_access_needs_an_explicit_confirmation() {
    let mut f = fixture();
    install(
        &mut f,
        "dev.me.net",
        "[permissions]\nnetwork_hosts = [\"api.example.com\"]",
    );
    assert!(notices(&f.model).contains("network: allowed for this plugin's processes"));
    assert_eq!(
        labels(&f.model),
        ["Keep disabled", "Allow this access and enable"]
    );
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(labels(&f.model)[LIST_HEAD].starts_with("○ dev.me.net"));

    handle_key(&mut f.model, KeyCode::Char(' '), &f.tx);
    select(&mut f.model, 1);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(labels(&f.model)[LIST_HEAD].starts_with("● dev.me.net"));
}

#[test]
fn details_run_an_action_by_placing_it_in_the_composer() {
    let mut f = fixture();
    install(&mut f, "dev.me.kit", "");
    select(&mut f.model, LIST_HEAD);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(notices(&f.model).contains("action     review"));
    let rows = labels(&f.model);
    assert!(
        !rows.iter().any(|row| row.contains("Update")),
        "a folder install has nothing to update from"
    );
    let run = rows
        .iter()
        .position(|row| row == "▶ Run action: Review")
        .unwrap();
    select(&mut f.model, run);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert_eq!(f.model.input, "Review the current diff.");
    assert!(f.model.picker.is_none());
}

#[test]
fn repository_shadowing_shows_up_as_a_warning() {
    let mut f = fixture();
    install(&mut f, "dev.me.kit", "");
    write_package(
        &f.root.join("workspace/.medha/plugins/copy"),
        "dev.me.kit",
        "",
    );
    run_command(&mut f.model, "", &f.tx);
    let rows = labels(&f.model);
    let warning = rows.len() - 2;
    assert!(rows[warning].starts_with("⚠ 1 warning"));
    select(&mut f.model, warning);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(labels(&f.model)[0].contains("ignored because a user plugin has the same id"));
    handle_key(&mut f.model, KeyCode::Esc, &f.tx);
    assert!(labels(&f.model)[0].starts_with("➕"));
}

#[test]
fn a_new_hook_folder_asks_once_and_applies_without_a_restart() {
    let mut f = fixture();
    let script = f.root.join("workspace/.medha/hooks/pre-tool/strip.py");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::write(&script, "# medha: matcher=Bash\nprint()\n").unwrap();
    let reloads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    f.model.apply_plugins = Some({
        let reloads = std::sync::Arc::clone(&reloads);
        std::sync::Arc::new(move || {
            reloads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Vec::new()
        })
    });

    review_hooks(&mut f.model);
    assert!(notices(&f.model).contains("project.hooks: hooks were found"));
    assert!(notices(&f.model).contains("pre-tool-strip — pre_tool · Bash"));
    assert_eq!(
        labels(&f.model),
        ["Keep off (don't ask again)", "Allow and turn on"]
    );
    select(&mut f.model, 1);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(notices(&f.model).contains("enabled project.hooks — active now"));
    assert_eq!(reloads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(f.model.plugins_changed, "the skill list is rebuilt next");
    assert!(f.model.picker.is_none(), "nothing else to review");

    std::fs::write(&script, "# medha: matcher=Bash\nprint('edited')\n").unwrap();
    review_hooks(&mut f.model);
    assert!(notices(&f.model).contains("changed since you allowed them"));
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(notices(&f.model).contains("stays off"));
    review_hooks(&mut f.model);
    assert!(
        f.model.picker.is_none(),
        "a recorded choice is not asked again"
    );
}

#[test]
fn discover_offers_adding_a_marketplace_and_doctor_reports() {
    let mut f = fixture();
    run_command(&mut f.model, "discover", &f.tx);
    assert!(notices(&f.model).contains("no marketplace plugins yet"));
    assert_eq!(
        labels(&f.model),
        [
            "🔍 Type to search 0 plugins — from ",
            "➕ Add a marketplace (owner/repo)…",
            "⟳ Refresh marketplaces",
            "← Back"
        ]
    );
    select(&mut f.model, 1);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert_eq!(f.model.input, "/plugins marketplace add ");

    run_command(&mut f.model, "doctor", &f.tx);
    assert!(notices(&f.model).contains("plugin check:"));
    assert!(notices(&f.model).contains("sandbox"));
}

#[test]
fn enabled_plugin_actions_become_slash_commands() {
    let mut f = fixture();
    install(&mut f, "dev.me.kit", "");
    install(&mut f, "dev.other.kit", "");
    let names: Vec<&str> = f
        .model
        .plugin_commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["/kit:review", "/kit:review"],
        "a clash gets a prefix"
    );
    let _ = f
        .model
        .plugins
        .as_ref()
        .unwrap()
        .disable("dev.other.kit", None);
    refresh_commands(&mut f.model);
    let only = &f.model.plugin_commands;
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].name, "/review");
    assert_eq!(
        commands::expand(only, "/review the auth change").as_deref(),
        Some("Review the current diff.\n\nthe auth change")
    );
    assert_eq!(commands::expand(only, "/reviewer"), None);
}

#[test]
fn typing_in_discover_narrows_the_catalog() {
    let mut f = fixture();
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    std::fs::write(
        f.root.join("home/markets.toml"),
        "[[marketplaces]]\nname = \"m\"\ncommit = \"c\"\nsource = { url = \"u\" }\n\
         plugins = [\n  { name = \"ponytail\", description = \"Laziest solution\", source = { url = \"u\" } },\n  \
         { name = \"amd-skills\", description = \"GPU kernels\", source = { url = \"u\" } },\n]\n",
    )
    .unwrap();
    run_command(&mut f.model, "discover", &f.tx);
    let rows = labels(&f.model);
    assert_eq!(rows[0], "🔍 Type to search 2 plugins — from m 2");
    assert_eq!(rows.len(), 6);
    let typed = |f: &mut Fixture, text: &str| {
        for key in text.chars() {
            handle_key(&mut f.model, KeyCode::Char(key), &f.tx);
        }
    };
    typed(&mut f, "gpu");
    let rows = labels(&f.model);
    assert_eq!(rows[0], "🔍 gpu▏  1 match(es)");
    assert_eq!(rows.len(), 5);
    assert!(rows[1].starts_with("○ amd-skills  — GPU kernels  [m]"));
    assert_eq!(f.model.picker.as_ref().unwrap().selected, 1);
    handle_key(&mut f.model, KeyCode::Esc, &f.tx);
    assert_eq!(labels(&f.model).len(), 6, "Esc clears the search first");
    typed(&mut f, "ponytial");
    assert!(
        labels(&f.model)[1].starts_with("○ ponytail"),
        "one typo still finds it"
    );
    handle_key(&mut f.model, KeyCode::Esc, &f.tx);
    typed(&mut f, "amd");
    handle_key(&mut f.model, KeyCode::Backspace, &f.tx);
    assert!(
        labels(&f.model)[1].starts_with("○ amd-skills"),
        "names rank first"
    );
    handle_key(&mut f.model, KeyCode::Esc, &f.tx);
    handle_key(&mut f.model, KeyCode::Esc, &f.tx);
    assert!(
        labels(&f.model)[0].starts_with("➕ Install"),
        "then goes back"
    );
}

#[test]
fn the_hooks_wizard_writes_a_script_and_asks_to_turn_it_on() {
    let mut f = fixture();
    hooks_command(&mut f.model, "");
    assert!(labels(&f.model)[0].starts_with("pre-tool — before a tool runs"));
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert_eq!(labels(&f.model)[1], "Shell commands");
    select(&mut f.model, 1);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert_eq!(f.model.input, "/hooks add pre-tool shell.exec ");

    hooks_command(
        &mut f.model,
        "add pre-tool shell.exec echo no force pushes >&2; exit 2",
    );
    let written = f
        .root
        .join("workspace/.medha/hooks/pre-tool/echo-no-force.sh");
    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.starts_with("#!/bin/sh\n# medha: matcher=shell.exec\necho no force"));
    assert_eq!(
        labels(&f.model),
        ["Keep off (don't ask again)", "Allow and turn on"]
    );

    let script = f.root.join("strip.py");
    std::fs::write(&script, "#!/usr/bin/env python3\nprint()\n").unwrap();
    hooks::add(
        &f.root.join("workspace"),
        "post-tool",
        "edit",
        script.to_str().unwrap(),
    )
    .unwrap();
    let copied =
        std::fs::read_to_string(f.root.join("workspace/.medha/hooks/post-tool/strip.py")).unwrap();
    assert_eq!(
        copied,
        "#!/usr/bin/env python3\n# medha: matcher=edit\nprint()\n"
    );
    assert!(hooks::add(&f.root.join("workspace"), "file-change", "*", "true").is_err());
}

#[test]
fn remove_asks_first_and_bad_commands_explain_usage() {
    let mut f = fixture();
    install(&mut f, "dev.me.kit", "");
    run_command(&mut f.model, "remove dev.me.kit", &f.tx);
    assert_eq!(labels(&f.model), ["Keep plugin", "Remove plugin"]);
    select(&mut f.model, 1);
    handle_key(&mut f.model, KeyCode::Enter, &f.tx);
    assert!(notices(&f.model).contains("removed dev.me.kit"));
    assert_eq!(
        labels(&f.model).len(),
        3,
        "install, discover, and doctor remain"
    );

    run_command(&mut f.model, "frobnicate", &f.tx);
    assert!(notices(&f.model).contains("usage: /plugins"));
}
