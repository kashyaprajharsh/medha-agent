use super::*;
use futures::stream::{self, BoxStream, StreamExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct AccessGate {
    decisions: Mutex<VecDeque<kernel::NetworkDecision>>,
    cards: Mutex<Vec<String>>,
    standard: AtomicUsize,
}

#[async_trait]
impl kernel::HumanGate for AccessGate {
    async fn confirm(&self, _: &str, _: Option<&str>, _: bool) -> kernel::Approval {
        self.standard.fetch_add(1, Ordering::SeqCst);
        kernel::Approval::Once
    }
    async fn confirm_access(&self, detail: Option<&str>, _: bool) -> kernel::NetworkDecision {
        self.cards.lock().unwrap().push(detail.unwrap().to_string());
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(kernel::NetworkDecision::Deny)
    }
}

struct NativeFixture {
    requests: Arc<Mutex<Vec<sandbox::ExecRequest>>>,
    network: sandbox::NetworkGrant,
}
#[async_trait]
impl sandbox::ExecBackend for NativeFixture {
    fn label(&self) -> &str {
        "native"
    }
    fn denies_network(&self, _: &sandbox::ExecRequest) -> bool {
        !self.network.granted() && !kernel::network_once_active()
    }
    fn build_command(
        &self,
        req: &sandbox::ExecRequest,
    ) -> Result<tokio::process::Command, sandbox::ExecError> {
        self.requests.lock().unwrap().push(req.clone());
        sandbox::exec::HostBackend.build_command(req)
    }
}

struct Fixture {
    base: PathBuf,
    outside: PathBuf,
    registry: Arc<ToolRegistry>,
    gate: Arc<AccessGate>,
    requests: Arc<Mutex<Vec<sandbox::ExecRequest>>>,
    roots: sandbox::ApprovedRoots,
    network: sandbox::NetworkGrant,
}
impl Fixture {
    fn new(decisions: Vec<kernel::NetworkDecision>) -> Self {
        let base = std::env::temp_dir().join(format!("medha-shell-access-{}", ulid::Ulid::new()));
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let outside = outside.canonicalize().unwrap();
        let gate = Arc::new(AccessGate {
            decisions: Mutex::new(decisions.into()),
            cards: Mutex::new(Vec::new()),
            standard: AtomicUsize::new(0),
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        let roots = sandbox::ApprovedRoots::default();
        let network = sandbox::NetworkGrant::default();
        let sbx = WorkspaceSandbox::new_with_roots(
            &workspace,
            base.join("trust.lock"),
            base.join("audit.log"),
            Some(gate.clone()),
            roots.clone(),
        )
        .unwrap()
        .with_exec_backend(Arc::new(NativeFixture {
            requests: requests.clone(),
            network: network.clone(),
        }))
        .with_network_grant(network.clone())
        .unwrap();
        let registry = Arc::new(ToolRegistry::with_workspace(
            Arc::new(sbx),
            Arc::new(Artifacts),
        ));
        Self {
            base,
            outside,
            registry,
            gate,
            requests,
            roots,
            network,
        }
    }
    fn args(&self) -> Value {
        json!({"command": "printf installed > installed.txt", "network": true, "workdir": self.outside, "write_paths": [self.outside]})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).unwrap();
    }
}

struct Artifacts;
impl kernel::ArtifactStore for Artifacts {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Ok("unused".into())
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Ok(0)
    }
}
struct Provider {
    first: AtomicBool,
    args: Value,
    caps: kernel::ProviderCaps,
}
#[async_trait]
impl kernel::Provider for Provider {
    fn capabilities(&self) -> &kernel::ProviderCaps {
        &self.caps
    }
    async fn stream(
        &self,
        _: &kernel::CompiledContext,
    ) -> Result<
        BoxStream<'static, Result<kernel::Block, kernel::ProviderError>>,
        kernel::ProviderError,
    > {
        let block = if self.first.swap(false, Ordering::SeqCst) {
            kernel::Block::ToolIntent(ToolIntent {
                id: "install".into(),
                tool: "shell.exec".into(),
                args: self.args.clone(),
            })
        } else {
            kernel::Block::Text("done".into())
        };
        Ok(stream::iter([Ok(block)]).boxed())
    }
}
struct Context;
#[async_trait]
impl kernel::ContextEngine for Context {
    async fn compile(&self, messages: &[kernel::Message], _: Option<u32>) -> kernel::CompileResult {
        kernel::CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted: false,
            summarized: false,
            before_tokens: 0,
            after_tokens: 0,
            overflow: false,
            summary: None,
        }
    }
}
struct Review;
impl kernel::Policy for Review {
    fn authorize(
        &self,
        _: kernel::AutonomyLevel,
        _: &ToolIntent,
        _: Option<BlastRadius>,
    ) -> kernel::Decision {
        kernel::Decision::Human
    }
}
async fn run(fixture: &Fixture, args: Value, plan: bool) -> Vec<kernel::Event> {
    use kernel::EventLog;
    let log = Arc::new(kernel::InMemoryLog::new());
    let provider = Provider {
        first: AtomicBool::new(true),
        args,
        caps: kernel::ProviderCaps {
            images: kernel::ImageSupport::Unsupported,
            caching: false,
            max_ctx: None,
            tool_calls: kernel::ToolCallStrategy::Native,
        },
    };
    let kernel = kernel::Kernel::new(
        Arc::new(provider),
        log.clone(),
        Arc::new(orchestrator::NarrowedExecutor::new(
            fixture.registry.clone(),
            None,
        )),
        Arc::new(Context),
        Arc::new(Artifacts),
        Arc::new(Review),
        fixture.gate.clone(),
        Arc::new(kernel::NoVerify),
    );
    let mut session = kernel::Session::new();
    if plan {
        session.autonomy = kernel::AutonomyLevel::Plan;
    }
    kernel
        .run_session(
            &session,
            vec![kernel::Message::user("install in the requested directory")],
            kernel::Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    log.events(session.id).await
}

#[tokio::test]
async fn network_and_folder_are_approved_together_before_the_only_execution() {
    let f = Fixture::new(vec![kernel::NetworkDecision::Once]);
    run(&f, f.args(), false).await;
    assert_eq!(
        std::fs::read_to_string(f.outside.join("installed.txt")).unwrap(),
        "installed"
    );
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert_eq!(
        f.gate.standard.load(Ordering::SeqCst),
        0,
        "command review must use the combined card"
    );
    {
        let cards = f.gate.cards.lock().unwrap();
        assert_eq!(cards.len(), 1);
        assert!(cards[0].contains("Network access"));
        assert!(cards[0].contains("Read/write"));
        assert!(cards[0].contains(&format!("Working directory: {}", f.outside.display())));
        assert!(!cards[0].contains("- Read "), "write already includes read");
    }
    assert!(!f.network.granted());
    assert!(f.roots.write_roots().is_empty());
    assert!(!kernel::network_once_active());
    run(&f, f.args(), false).await;
    assert_eq!(
        f.requests.lock().unwrap().len(),
        1,
        "second command must not inherit once access"
    );
    assert_eq!(f.gate.cards.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn denying_access_and_plan_mode_do_not_start_any_process() {
    let f = Fixture::new(vec![kernel::NetworkDecision::Deny]);
    run(&f, f.args(), false).await;
    assert!(!f.outside.join("installed.txt").exists());
    assert!(f.requests.lock().unwrap().is_empty());
    assert_eq!(f.gate.cards.lock().unwrap().len(), 1);
    run(&f, f.args(), true).await;
    assert_eq!(
        f.gate.cards.lock().unwrap().len(),
        1,
        "plan mode cannot ask to bypass its restriction"
    );
    assert!(f.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn session_grants_are_reused_without_persisting_them() {
    let f = Fixture::new(vec![kernel::NetworkDecision::Session]);
    run(&f, f.args(), false).await;
    run(&f, f.args(), false).await;
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    assert_eq!(f.gate.cards.lock().unwrap().len(), 1);
    assert!(f.network.granted());
    assert_eq!(f.roots.write_roots(), vec![f.outside.clone()]);
    assert!(!f.base.join("trust.lock").exists());
}

#[tokio::test]
async fn persistent_grants_reload_into_a_new_workspace_sandbox() {
    let f = Fixture::new(vec![kernel::NetworkDecision::Persistent]);
    run(&f, f.args(), false).await;
    let roots = sandbox::ApprovedRoots::default();
    let network = sandbox::NetworkGrant::default();
    let _reloaded = WorkspaceSandbox::new_with_roots(
        f.base.join("workspace"),
        f.base.join("trust.lock"),
        f.base.join("audit.log"),
        None,
        roots.clone(),
    )
    .unwrap()
    .with_network_grant(network.clone())
    .unwrap();
    assert!(network.granted());
    assert_eq!(roots.write_roots(), vec![f.outside.clone()]);
}

#[tokio::test]
async fn unexpected_network_failure_does_not_replay_partial_side_effects() {
    let f = Fixture::new(Vec::new());
    run(&f, json!({"command": "printf x >> count; echo 'getaddrinfo ENOTFOUND registry.example' >&2; exit 1", "network": false}), false).await;
    assert_eq!(
        std::fs::read_to_string(f.base.join("workspace/count")).unwrap(),
        "x"
    );
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert!(f.gate.cards.lock().unwrap().is_empty());
}

#[tokio::test]
async fn redirected_filesystem_failure_is_reported_without_replaying() {
    let f = Fixture::new(Vec::new());
    let command = format!(
        "printf '%s: Permission denied\\n' '{}' ; true",
        f.outside.display()
    );
    let events = run(&f, json!({"command": command, "network": false}), false).await;
    let observation = events
        .iter()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .unwrap();
    assert_eq!(
        observation.payload["payload"]["denied_paths"][0],
        f.outside.display().to_string()
    );
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert!(f.gate.cards.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_directory_is_rejected_before_any_approval_or_execution() {
    let f = Fixture::new(vec![kernel::NetworkDecision::Once]);
    let mut args = f.args();
    args["read_paths"] = json!(["relative/path"]);
    run(&f, args, false).await;
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(f.gate.cards.lock().unwrap().is_empty());
    assert_eq!(f.gate.standard.load(Ordering::SeqCst), 0);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn native_workdir_read_grant_does_not_expand_workspace_write_access() {
    if !sandbox::native_sandbox_supported() {
        return;
    }
    let mut f = Fixture::new(vec![
        kernel::NetworkDecision::Once,
        kernel::NetworkDecision::Once,
    ]);
    let backend = sandbox::select_backend(
        &sandbox::SandboxConfig::default(),
        Vec::new(),
        f.roots.clone(),
        f.network.clone(),
    );
    assert_eq!(backend.label(), "native");
    let workspace = f.base.join("workspace");
    let sbx = WorkspaceSandbox::new_with_roots(
        &workspace,
        f.base.join("trust.lock"),
        f.base.join("audit.log"),
        Some(f.gate.clone()),
        f.roots.clone(),
    )
    .unwrap()
    .with_exec_backend(backend)
    .with_network_grant(f.network.clone())
    .unwrap();
    f.registry = Arc::new(ToolRegistry::with_workspace(
        Arc::new(sbx),
        Arc::new(Artifacts),
    ));
    std::fs::write(f.outside.join("readable.txt"), "readable").unwrap();
    let events = run(&f, json!({"command": "cat readable.txt; printf bad > blocked.txt", "network": false, "workdir": f.outside}), false).await;
    let observation = events
        .iter()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .unwrap();
    assert_eq!(observation.payload["payload"]["stdout"], "readable");
    assert!(
        !f.outside.join("blocked.txt").exists(),
        "read-only cwd must not become a writable workspace"
    );
    run(&f, json!({"command": "printf approved > allowed.txt", "network": false, "workdir": f.outside, "write_paths": [f.outside]}), false).await;
    assert_eq!(
        std::fs::read_to_string(f.outside.join("allowed.txt")).unwrap(),
        "approved"
    );
    assert!(f.roots.write_roots().is_empty());
    assert_eq!(f.gate.cards.lock().unwrap().len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn a_directory_symlink_changed_after_approval_cannot_retarget_once_access() {
    let f = Fixture::new(Vec::new());
    let alias = f.base.join("alias");
    std::os::unix::fs::symlink(&f.outside, &alias).unwrap();
    let intent = ToolIntent {
        id: "symlink".into(),
        tool: "shell.exec".into(),
        args: json!({"command": "touch should-not-exist", "network": false, "workdir": alias, "write_paths": [alias]}),
    };
    let access = f.registry.missing_access(&intent).unwrap();
    let other = f.base.join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();
    let result = kernel::execution_access_scope(access, f.registry.execute(&intent)).await;
    assert_eq!(result.status, kernel::ObsStatus::Error);
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!other.join("should-not-exist").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires native macOS sandbox, npm, and public registry access"]
async fn approved_native_npm_install_works_in_an_outside_directory() {
    assert!(sandbox::native_sandbox_supported());
    let mut f = Fixture::new(vec![kernel::NetworkDecision::Once]);
    let backend = sandbox::select_backend(
        &sandbox::SandboxConfig {
            net: sandbox::NetPolicy::Deny,
            ..Default::default()
        },
        Vec::new(),
        f.roots.clone(),
        f.network.clone(),
    );
    assert_eq!(backend.label(), "native");
    let sbx = WorkspaceSandbox::new_with_roots(
        f.base.join("workspace"),
        f.base.join("trust.lock"),
        f.base.join("audit.log"),
        Some(f.gate.clone()),
        f.roots.clone(),
    )
    .unwrap()
    .with_exec_backend(backend)
    .with_network_grant(f.network.clone())
    .unwrap();
    f.registry = Arc::new(ToolRegistry::with_workspace(
        Arc::new(sbx),
        Arc::new(Artifacts),
    ));
    let mut args = f.args();
    args["command"] = json!(
        "npm install pptxgenjs --ignore-scripts --no-audit --no-fund --package-lock=false --fetch-retries=0 && node -e \"require('pptxgenjs'); console.log('package import OK')\""
    );
    args["timeout_s"] = json!(90);
    let events = run(&f, args, false).await;
    let observation = events
        .iter()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .unwrap();
    assert_eq!(
        observation.payload["payload"]["exit_code"], 0,
        "{}",
        observation.payload
    );
    assert!(
        observation.payload["payload"]["stdout"]
            .as_str()
            .unwrap()
            .contains("package import OK")
    );
    assert!(
        f.outside
            .join("node_modules/pptxgenjs/package.json")
            .exists()
    );
    assert_eq!(f.gate.cards.lock().unwrap().len(), 1);
    assert_eq!(f.gate.standard.load(Ordering::SeqCst), 0);
    assert!(!f.network.granted());
    assert!(f.roots.write_roots().is_empty());
}

#[tokio::test]
async fn relative_folder_denials_offer_access_guidance_without_claiming_absence() {
    let f = Fixture::new(Vec::new());
    let events = run(
        &f,
        json!({"command": "echo 'ls: .: Operation not permitted' >&2; true", "network": false}),
        false,
    )
    .await;
    let observation = events
        .iter()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .unwrap();
    let hint = observation.payload["payload"]["filesystem_hint"]
        .as_str()
        .unwrap();
    assert!(hint.contains("workdir"));
    assert!(hint.contains("does not mean a file or program is missing"));
    assert_eq!(f.requests.lock().unwrap().len(), 1);
}
