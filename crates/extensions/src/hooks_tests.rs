use super::*;
use crate::test_support::{store, write_hook_package, write_hook_package_with};
use kernel::HookRunner as _;
use std::fs;

fn tool_request(tool: &str, command: &str) -> kernel::HookRequest {
    kernel::HookRequest::new(
        "session",
        HookPoint::PreTool,
        kernel::TrustLabel::System,
        serde_json::json!({"tool": tool, "args": {"command": command}}),
    )
}

fn runner_for(temp: &Path, id: &str, script: &str, extra: &str) -> ProcessHookRunner {
    let source = temp.join("source");
    write_hook_package_with(&source, id, script, 2_000, extra);
    let store = store(temp);
    store.install(&source).unwrap();
    store.enable(id, None, &Grant::default()).unwrap();
    ProcessHookRunner::load(store, temp).with_test_backend(Arc::new(sandbox::HostBackend))
}

#[test]
fn matchers_use_canonical_or_existing_tool_names_and_globs() {
    assert!(tool_matches("shell.exec", "shell.exec"));
    assert!(tool_matches("mcp__*", "mcp__github__search"));
    assert!(tool_matches("*exec", "shell.exec"));
    assert!(!tool_matches("shell.exec", "read"));
    assert!(tool_matches(
        crate::compat::canonical_tool_name("Bash"),
        "shell.exec"
    ));
}

#[tokio::test]
async fn a_non_matching_tool_never_starts_the_hook() {
    let temp = tempfile::tempdir().unwrap();
    let runner = runner_for(
        temp.path(),
        "dev.medha.only-shell",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"decision\":\"deny\",\"reason\":\"no\"}'\n",
        "matcher = [\"Bash\"]",
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let read = runner.invoke(&tool_request("read", ""), &cancel).await;
    assert!(read.audits.is_empty(), "no process for an unmatched tool");
    assert_eq!(read.directive, kernel::HookDirective::Continue);
    let shell = runner
        .invoke(&tool_request("shell.exec", "ls"), &cancel)
        .await;
    assert!(matches!(shell.directive, kernel::HookDirective::Deny(_)));
}

#[tokio::test]
async fn causation_depth_limit_skips_hook_before_spawning_it() {
    let temp = tempfile::tempdir().unwrap();
    let runner = runner_for(
        temp.path(),
        "dev.medha.depth",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"decision\":\"deny\"}'\n",
        "",
    );
    let mut request = tool_request("read", "");
    request.depth = MAX_CAUSATION_DEPTH;
    request.causation_id = Some("origin".into());
    let batch = runner
        .invoke(&request, &tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(batch.directive, kernel::HookDirective::Continue);
    assert_eq!(batch.audits.len(), 1);
    assert_eq!(batch.audits[0].status, kernel::HookStatus::Skipped);
}

#[tokio::test]
async fn bundled_example_hook_returns_typed_context() {
    let temp = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-plugin");
    let store = store(temp.path());
    store.install(&source).unwrap();
    store
        .enable("dev.medha.hello", None, &Grant::default())
        .unwrap();
    let runner = ProcessHookRunner::load(store, temp.path())
        .with_test_backend(Arc::new(sandbox::HostBackend));
    let request = kernel::HookRequest::new(
        "session",
        HookPoint::SessionStart,
        kernel::TrustLabel::System,
        serde_json::json!({"source":"startup"}),
    );
    let batch = runner
        .invoke(&request, &tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(batch.audits.len(), 1, "{:#?}", batch.notices);
    assert_eq!(batch.audits[0].decision, Some(HookDecision::AddContext));
    assert_eq!(batch.contexts.len(), 1);
}

#[tokio::test]
async fn exit_status_hooks_run_unchanged_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let script = "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n\
                  *'git push --force'*) echo 'force push is not allowed' >&2; exit 2;;\n\
                  *'git commit'*) echo '{\"systemMessage\":\"checked the commit\"}';;\n\
                  *missing-runtime*) echo 'node: not found' >&2; exit 127;;\n\
                  esac\n";
    let runner = runner_for(
        temp.path(),
        "dev.medha.compat",
        script,
        "protocol = \"exit_status\"\nmatcher = [\"Bash\"]",
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let blocked = runner
        .invoke(&tool_request("shell.exec", "git push --force"), &cancel)
        .await;
    assert_eq!(
        blocked.directive,
        kernel::HookDirective::Deny("force push is not allowed".into())
    );
    let noted = runner
        .invoke(&tool_request("shell.exec", "git commit -m x"), &cancel)
        .await;
    assert_eq!(noted.directive, kernel::HookDirective::Continue);
    assert_eq!(
        noted.notices,
        ["dev.medha.compat/guard: checked the commit"]
    );
    let failed = runner
        .invoke(&tool_request("shell.exec", "missing-runtime"), &cancel)
        .await;
    let hooks = runner.snapshot();
    let hook = &hooks[0];
    let health = runner
        .store
        .component_health(&hook.plugin_id, &hook.content_hash);
    assert_eq!(health[0].last_failure, failed.audits[0].reason);
    assert!(
        health[0]
            .last_failure
            .as_ref()
            .unwrap()
            .contains("node: not found")
    );
    let silent = runner
        .invoke(&tool_request("shell.exec", "ls"), &cancel)
        .await;
    assert_eq!(silent.directive, kernel::HookDirective::Continue);
    assert_eq!(silent.audits[0].status, kernel::HookStatus::Completed);
}

#[tokio::test]
async fn supervised_hook_runs_typed_stdio_and_drift_revokes_it() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_hook_package(
        &source,
        "dev.medha.guard",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"decision\":\"continue\"}'\n",
        10_000,
    );
    let store = store(temp.path());
    store.install(&source).unwrap();
    store
        .enable("dev.medha.guard", None, &Grant::default())
        .unwrap();
    let runner = ProcessHookRunner::load(store.clone(), temp.path())
        .with_test_backend(Arc::new(sandbox::HostBackend));
    let request = kernel::HookRequest::new(
        "session",
        HookPoint::PreTool,
        kernel::TrustLabel::System,
        serde_json::json!({"tool": "read"}),
    );
    let first = runner
        .invoke(&request, &tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(first.directive, kernel::HookDirective::Continue);
    assert_eq!(first.audits.len(), 1);
    assert_eq!(first.audits[0].status, kernel::HookStatus::Completed);

    let installed = temp.path().join("user/dev.medha.guard/hook.sh");
    fs::write(
        &installed,
        "#!/bin/sh\nprintf '%s\\n' '{\"decision\":\"continue\"}'\n# drift\n",
    )
    .unwrap();
    let second = runner
        .invoke(&request, &tokio_util::sync::CancellationToken::new())
        .await;
    assert!(matches!(second.directive, kernel::HookDirective::Deny(_)));
    assert_eq!(second.audits[0].status, kernel::HookStatus::Failed);
}

#[tokio::test]
async fn fail_closed_hook_timeout_stops_the_process() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    write_hook_package(
        &source,
        "dev.medha.slow",
        "#!/bin/sh\ncat >/dev/null\nsleep 5\nprintf '%s\\n' '{\"decision\":\"continue\"}'\n",
        30,
    );
    let store = store(temp.path());
    let package = store.install(&source).unwrap();
    store
        .enable("dev.medha.slow", None, &Grant::default())
        .unwrap();
    let runner = ProcessHookRunner::load(store.clone(), temp.path())
        .with_test_backend(Arc::new(sandbox::HostBackend));
    let request = kernel::HookRequest::new(
        "session",
        HookPoint::PreTool,
        kernel::TrustLabel::System,
        serde_json::json!({}),
    );
    let started = std::time::Instant::now();
    let batch = runner
        .invoke(&request, &tokio_util::sync::CancellationToken::new())
        .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(matches!(batch.directive, kernel::HookDirective::Deny(_)));
    assert_eq!(batch.audits[0].status, kernel::HookStatus::TimedOut);
    let health = store.component_health("dev.medha.slow", &package.content_hash);
    assert_eq!(health[0].state, crate::ComponentState::Failed);
    assert_eq!(
        health[0].last_failure.as_deref(),
        Some("hook process exceeded its deadline")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn installed_hook_can_read_its_package_and_write_only_granted_roots() {
    const CHILD: &str = "MEDHA_PLUGIN_SANDBOX_TEST";
    if let Some(base) = std::env::var_os(CHILD) {
        let base = PathBuf::from(base);
        let home = base.join("home/.medha");
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("credentials.toml"), "fixture secret").unwrap();
        let source = base.join("source");
        write_hook_package_with(
            &source,
            "dev.medha.scoped",
            r#"#!/bin/sh
set -eu
cat >/dev/null
printf saved > "$MEDHA_PLUGIN_DATA/state"
test "$(cat "$MEDHA_PLUGIN_DATA/state")" = saved
state=$(dirname "$(dirname "$MEDHA_PLUGIN_DATA")")
ln -s "$state/credentials.toml" "$MEDHA_PLUGIN_DATA/secret-link"
for file in "$state/credentials.toml" "$MEDHA_PLUGIN_DATA/secret-link"; do
    if cat "$file" 2>/dev/null; then exit 11; fi
done
for file in "$MEDHA_PLUGIN_ROOT/changed" "$MEDHA_PROJECT_DIR/changed"; do
    if touch "$file" 2>/dev/null; then exit 12; fi
done
printf '{"decision":"continue"}'
"#,
            5_000,
            "workdir = \"workspace\"\n[permissions]\nread_paths = [\".\"]",
        );
        let store = Store::new(
            home.join("plugins"),
            workspace.join(".medha/plugins"),
            home.join("plugins.toml"),
            base.join("project-state.toml"),
            "0.1.8",
        );
        let package = store.install(&source).unwrap();
        store
            .enable(
                "dev.medha.scoped",
                None,
                &Grant {
                    read_paths: vec![".".into()],
                    ..Grant::default()
                },
            )
            .unwrap();
        let runner = ProcessHookRunner::load(store.clone(), &workspace);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let batch = runtime.block_on(runner.invoke(
            &tool_request("read", ""),
            &tokio_util::sync::CancellationToken::new(),
        ));
        assert_eq!(
            batch.audits[0].status,
            kernel::HookStatus::Completed,
            "{:#?}",
            batch.audits
        );
        assert_eq!(
            store.component_health("dev.medha.scoped", &package.content_hash)[0].state,
            crate::ComponentState::Ready
        );
        return;
    }
    if !sandbox::native_sandbox_supported() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hooks::tests::installed_hook_can_read_its_package_and_write_only_granted_roots",
            "--nocapture",
        ])
        .env(CHILD, temp.path())
        .env("HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
