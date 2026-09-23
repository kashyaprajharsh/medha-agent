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
async fn exit_status_hooks_run_unchanged_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let script = "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n\
                  *'git push --force'*) echo 'force push is not allowed' >&2; exit 2;;\n\
                  *'git commit'*) echo '{\"systemMessage\":\"checked the commit\"}';;\n\
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
    store.install(&source).unwrap();
    store
        .enable("dev.medha.slow", None, &Grant::default())
        .unwrap();
    let runner = ProcessHookRunner::load(store, temp.path())
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
}
