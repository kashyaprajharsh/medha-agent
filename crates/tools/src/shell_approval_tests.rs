use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct PathGate(AtomicUsize);
#[async_trait]
impl kernel::HumanGate for PathGate {
    async fn confirm(&self, action: &str, _: Option<&str>, _: bool) -> kernel::Approval {
        assert!(action.starts_with("Read access"), "{action}");
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            kernel::Approval::Once
        } else {
            kernel::Approval::Deny
        }
    }
}

/// Exercise the actual owned-process runner without requiring a native jail
/// on CI: this backend reports a denial until the request carries its grant.
struct PathBackend {
    root: std::path::PathBuf,
}
#[async_trait]
impl sandbox::ExecBackend for PathBackend {
    fn label(&self) -> &str {
        "native"
    }
    fn build_command(
        &self,
        req: &sandbox::ExecRequest,
    ) -> Result<tokio::process::Command, sandbox::ExecError> {
        let allowed = req.read_roots.contains(&self.root);
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args([
            "-c",
            if allowed {
                "printf approved"
            } else {
                "printf '%s: Permission denied\\n' \"$1\" >&2; exit 1"
            },
            "fixture",
        ]);
        cmd.arg(self.root.join("data.txt"));
        Ok(cmd)
    }
}

#[tokio::test]
async fn shell_exec_retries_approved_paths_without_leaking_once_grants() {
    let base = std::env::temp_dir().join(format!("medha-shell-approval-{}", ulid::Ulid::new()));
    let workspace = base.join("workspace");
    let outside = base.join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("data.txt"), "fixture").unwrap();
    let gate = Arc::new(PathGate(AtomicUsize::new(0)));
    let roots = sandbox::ApprovedRoots::default();
    let sbx = Arc::new(
        WorkspaceSandbox::new_with_roots(
            &workspace,
            base.join("trust.lock"),
            base.join("audit.log"),
            Some(gate.clone()),
            roots,
        )
        .unwrap()
        .with_exec_backend(Arc::new(PathBackend {
            root: outside.canonicalize().unwrap(),
        })),
    );
    let tasks = Arc::new(TaskTable::default());
    let tool = ShellExec {
        sbx,
        tasks: tasks.clone(),
    };
    let args = json!({"command": format!("cat '{}'", outside.join("data.txt").display())});
    let first = tool.execute(&args).await.unwrap();
    assert_eq!(
        first["exit_code"], 0,
        "approved command did not retry: {first}"
    );
    assert_eq!(first["stdout"], "approved");
    assert_eq!(gate.0.load(Ordering::SeqCst), 1);
    assert!(
        tasks.info().is_empty(),
        "finished tasks must release admission"
    );
    let second = tool.execute(&args).await.unwrap();
    assert_ne!(
        second["exit_code"], 0,
        "once approval leaked into another command"
    );
    assert_eq!(gate.0.load(Ordering::SeqCst), 2);
    assert!(tasks.info().is_empty());
    std::fs::remove_dir_all(base).unwrap();
}
