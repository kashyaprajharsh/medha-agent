use crate::{Tool, ToolError, ToolRegistry};
use async_trait::async_trait;
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use kernel::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Artifacts;
impl ArtifactStore for Artifacts {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Err("unused".into())
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Err("unused".into())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Err("unused".into())
    }
}

struct Endpoint {
    caps: ProviderCaps,
    images: AtomicBool,
    requests: Mutex<Vec<CompiledContext>>,
    turns: Mutex<VecDeque<Vec<Block>>>,
}

impl Endpoint {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            caps: ProviderCaps {
                images: ImageSupport::Supported,
                caching: false,
                max_ctx: Some(128_000),
                tool_calls: ToolCallStrategy::Native,
            },
            images: AtomicBool::new(true),
            requests: Mutex::new(vec![]),
            turns: Mutex::new(VecDeque::new()),
        })
    }
}

#[async_trait]
impl Provider for Endpoint {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }
    fn image_support(&self) -> ImageSupport {
        if self.images.load(Ordering::Relaxed) {
            ImageSupport::Supported
        } else {
            ImageSupport::Unsupported
        }
    }
    async fn stream(
        &self,
        context: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(context.clone());
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Block::Text("done".into())]);
        Ok(stream::iter(turn.into_iter().map(Ok)).boxed())
    }
}

#[derive(Default)]
struct Boundary(AtomicBool);
#[async_trait]
impl ContextEngine for Boundary {
    async fn compile(&self, messages: &[Message], _: Option<u32>) -> CompileResult {
        let compacted = self.0.swap(false, Ordering::Relaxed);
        CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted,
            summarized: compacted,
            before_tokens: 0,
            after_tokens: 0,
            overflow: false,
            summary: None,
        }
    }
}

struct Late;
#[async_trait]
impl Tool for Late {
    fn name(&self) -> &str {
        "aaa_late"
    }
    fn description(&self) -> &str {
        "late tool"
    }
    fn blast_radius(&self) -> BlastRadius {
        BlastRadius::Read
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: &Value) -> Result<Value, ToolError> {
        Ok(json!({}))
    }
}

fn registry(root: &std::path::Path, late: bool) -> Arc<ToolRegistry> {
    let sandbox = Arc::new(sandbox::WorkspaceSandbox::new_jailed(root).unwrap());
    let mut registry = ToolRegistry::with_workspace(sandbox, Arc::new(Artifacts));
    if late {
        registry.register(Arc::new(Late));
    }
    Arc::new(registry)
}

fn runtime(
    endpoint: Arc<Endpoint>,
    log: Arc<InMemoryLog>,
    registry: Arc<ToolRegistry>,
    boundary: Arc<Boundary>,
) -> Kernel<Endpoint, InMemoryLog> {
    Kernel::new(
        endpoint,
        log,
        registry,
        boundary,
        Arc::new(Artifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

#[tokio::test]
async fn real_read_definition_is_frozen_across_restart_until_the_compaction_request() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::new();
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let boundary = Arc::new(Boundary::default());
    let first = runtime(
        endpoint.clone(),
        log.clone(),
        registry(root.path(), false),
        boundary.clone(),
    );
    let (mut history, _) = first
        .run_session(
            &session,
            vec![Message::user("first")],
            Budget::default(),
            &NullSink,
            None,
        )
        .await
        .unwrap();
    drop(first);
    endpoint.images.store(false, Ordering::Relaxed);
    let resumed = runtime(
        endpoint.clone(),
        log.clone(),
        registry(root.path(), true),
        boundary.clone(),
    );
    history.push(Message::user("second"));
    let (mut history, _) = resumed
        .run_session(&session, history, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    {
        let requests = endpoint.requests.lock().unwrap();
        assert_eq!(
            serde_json::to_value(&requests[1].tools[..requests[0].tools.len()]).unwrap(),
            serde_json::to_value(&requests[0].tools).unwrap()
        );
        assert_eq!(requests[1].tools.last().unwrap().name, "aaa_late");
    }
    boundary.0.store(true, Ordering::Relaxed);
    history.push(Message::user("compact"));
    resumed
        .run_session(&session, history, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    let events = log.events(session.id).await;
    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let read = |index: usize| {
        requests[index]
            .tools
            .iter()
            .find(|spec| spec.name == "read")
            .unwrap()
            .description
            .clone()
    };
    assert_eq!(read(0), read(1));
    assert_ne!(read(1), read(2));
    let checkpoint = events
        .iter()
        .find(|event| event.kind == EventKind::Compaction)
        .unwrap();
    assert_eq!(
        checkpoint.payload["tools"]["specs"],
        serde_json::to_value(&requests[2].tools).unwrap()
    );
}

#[tokio::test]
async fn plan_keeps_request_prefix_and_registry_tools_while_refusing_an_actual_write() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "original").unwrap();
    let endpoint = Endpoint::new();
    let log = Arc::new(InMemoryLog::new());
    let registry = registry(root.path(), false);
    let kernel = runtime(
        endpoint.clone(),
        log.clone(),
        registry,
        Arc::new(Boundary::default()),
    );
    let mut session = Session::new();
    let (mut history, _) = kernel
        .run_session(
            &session,
            vec![Message::system("fixed"), Message::user("start")],
            Budget::default(),
            &NullSink,
            None,
        )
        .await
        .unwrap();
    session.autonomy = AutonomyLevel::Plan;
    history.push(Message::user("inspect"));
    endpoint.turns.lock().unwrap().push_back(vec![
        Block::ToolIntent(ToolIntent {
            id: "read".into(),
            tool: "read".into(),
            args: json!({"path":"note.txt"}),
        }),
        Block::ToolIntent(ToolIntent {
            id: "write".into(),
            tool: "edit".into(),
            args: json!({"path":"note.txt","old":"original","new":"changed"}),
        }),
    ]);
    let (mut history, _) = kernel
        .run_session(&session, history, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
        "original"
    );
    let events = log.events(session.id).await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == EventKind::PolicyDecision
                && event.payload["intent_id"] == "write"
                && event.payload["decision"] == "deny")
    );
    assert!(events.iter().any(|event| event.kind == EventKind::ToolObs
        && event.payload["intent_id"] == "read"
        && event.payload["status"] == "ok"));
    assert!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::UserMessage)
            .all(|event| !event.shown_text().0.contains("Plan mode"))
    );
    session.autonomy = AutonomyLevel::Normal;
    history.push(Message::user("continue"));
    kernel
        .run_session(&session, history, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for pair in requests.windows(2) {
        assert_eq!(
            serde_json::to_value(&pair[0].tools).unwrap(),
            serde_json::to_value(&pair[1].tools).unwrap()
        );
        assert_eq!(
            pair[1].ordered_messages()[..pair[0].ordered_messages().len()],
            pair[0].ordered_messages()
        );
    }
}
