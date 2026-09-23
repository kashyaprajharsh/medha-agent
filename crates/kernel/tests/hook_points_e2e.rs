use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, BlastRadius, Block, Budget, CompileResult, CompiledContext, ContextEngine,
    EventLog, Executor, HookAudit, HookBatch, HookContext, HookDecision, HookDirective, HookPoint,
    HookRequest, HookRunner, HookStatus, InMemoryLog, Kernel, Message, NoVerify, Observation,
    Provider, ProviderCaps, ProviderError, Session, StopReason, ToolCallStrategy, ToolIntent,
    ToolSpec, TrustLabel,
};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

struct ScriptedProvider {
    caps: ProviderCaps,
    turns: Mutex<VecDeque<Vec<Block>>>,
    requests: Mutex<Vec<CompiledContext>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<Block>>) -> Self {
        Self {
            caps: ProviderCaps {
                images: kernel::ImageSupport::Unsupported,
                caching: false,
                max_ctx: Some(32_000),
                tool_calls: ToolCallStrategy::Native,
            },
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(ctx.clone());
        let blocks = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Block::Text("done".into())]);
        Ok(stream::iter(blocks.into_iter().map(Ok)).boxed())
    }
}

struct Passthrough;

#[async_trait]
impl ContextEngine for Passthrough {
    async fn compile(&self, messages: &[Message], _max_ctx: Option<u32>) -> CompileResult {
        CompileResult {
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

struct ReadTools;

#[async_trait]
impl Executor for ReadTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn blast_radius(&self, _tool: &str) -> Option<BlastRadius> {
        Some(BlastRadius::Read)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        if intent.tool == "fail" {
            Observation::error(&intent.id, "setup has not run")
        } else {
            Observation::ok(intent.id.clone(), json!({"ok": true}))
        }
    }
}

struct MemArtifacts;

impl kernel::ArtifactStore for MemArtifacts {
    fn put(&self, _bytes: &[u8]) -> Result<String, String> {
        Ok("sha256:test".into())
    }

    fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    fn size(&self, _hash: &str) -> Result<usize, String> {
        Ok(0)
    }
}

enum Reply {
    Deny(&'static str),
    Context(&'static str),
}

#[derive(Default)]
struct PointHooks {
    replies: HashMap<HookPoint, Reply>,
    requests: Mutex<Vec<HookRequest>>,
}

impl PointHooks {
    fn with(point: HookPoint, reply: Reply) -> Self {
        Self {
            replies: HashMap::from([(point, reply)]),
            requests: Mutex::default(),
        }
    }

    fn count(&self, point: HookPoint) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.point == point)
            .count()
    }
}

#[async_trait]
impl HookRunner for PointHooks {
    async fn invoke(
        &self,
        request: &HookRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> HookBatch {
        self.requests.lock().unwrap().push(request.clone());
        let mut batch = HookBatch::default();
        let decision = match self.replies.get(&request.point) {
            Some(Reply::Deny(reason)) => {
                batch.directive = HookDirective::Deny((*reason).into());
                HookDecision::Deny
            }
            Some(Reply::Context(text)) => {
                batch.contexts.push(HookContext {
                    plugin_id: "dev.test".into(),
                    component_id: "guard".into(),
                    text: (*text).into(),
                });
                HookDecision::AddContext
            }
            None => HookDecision::Continue,
        };
        batch.audits.push(HookAudit {
            event_id: request.event_id.clone(),
            plugin_id: "dev.test".into(),
            component_id: "guard".into(),
            point: request.point,
            status: HookStatus::Completed,
            decision: Some(decision),
            reason: None,
            duration_ms: 1,
        });
        batch
    }
}

#[derive(Default)]
struct Notices(Mutex<Vec<String>>);

impl kernel::StreamSink for Notices {
    fn notice(&self, text: &str) {
        self.0.lock().unwrap().push(text.into());
    }
}

fn build(
    provider: Arc<ScriptedProvider>,
    log: Arc<InMemoryLog>,
    hooks: Arc<PointHooks>,
) -> Kernel<ScriptedProvider, InMemoryLog> {
    Kernel::new(
        provider,
        log,
        Arc::new(ReadTools),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
    .with_hooks(hooks)
}

async fn run(
    kernel: &Kernel<ScriptedProvider, InMemoryLog>,
    session: &Session,
    sink: &Notices,
) -> StopReason {
    kernel
        .run_session(
            session,
            vec![Message::user("go")],
            Budget::default(),
            sink,
            None,
        )
        .await
        .unwrap()
        .1
}

fn tool_call(tool: &str) -> Vec<Block> {
    vec![Block::ToolIntent(ToolIntent {
        id: "call-1".into(),
        tool: tool.into(),
        args: json!({}),
    })]
}

#[tokio::test]
async fn a_blocked_prompt_never_reaches_the_model() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let hooks = Arc::new(PointHooks::with(
        HookPoint::PromptSubmit,
        Reply::Deny("prompts may not mention production"),
    ));
    let sink = Notices::default();
    let kernel = build(provider.clone(), Arc::new(InMemoryLog::new()), hooks);
    let stop = run(&kernel, &Session::new(), &sink).await;
    assert_eq!(stop, StopReason::Blocked);
    assert_eq!(provider.calls(), 0);
    assert!(sink.0.lock().unwrap()[0].contains("prompts may not mention production"));
}

#[tokio::test]
async fn added_context_is_logged_as_tool_trust_before_the_model_reads_it() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let log = Arc::new(InMemoryLog::new());
    let hooks = Arc::new(PointHooks::with(
        HookPoint::PromptSubmit,
        Reply::Context("this repository uses pnpm"),
    ));
    let session = Session::new();
    let kernel = build(provider.clone(), log.clone(), hooks);
    run(&kernel, &session, &Notices::default()).await;

    let request = provider.requests.lock().unwrap()[0].clone();
    let injected = request
        .messages
        .iter()
        .find(|message| message.content.contains("this repository uses pnpm"))
        .expect("hook context reaches the model");
    assert_eq!(injected.trust, Some(TrustLabel::Tool));
    assert!(
        injected
            .content
            .starts_with("[prompt_submit hook dev.test/guard]")
    );
    let logged = log.events(session.id).await.into_iter().any(|event| {
        event
            .payload
            .to_string()
            .contains("this repository uses pnpm")
    });
    assert!(
        logged,
        "model-visible hook context must be in the event log"
    );
}

#[tokio::test]
async fn task_completion_can_resume_work_only_a_bounded_number_of_times() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let hooks = Arc::new(PointHooks::with(
        HookPoint::TaskCompletion,
        Reply::Context("tests are still failing"),
    ));
    let kernel = build(
        provider.clone(),
        Arc::new(InMemoryLog::new()),
        hooks.clone(),
    );
    let stop = run(&kernel, &Session::new(), &Notices::default()).await;
    assert_eq!(stop, StopReason::Finished);
    assert_eq!(
        provider.calls(),
        4,
        "one answer plus three hook continuations"
    );
    assert_eq!(hooks.count(HookPoint::TaskCompletion), 4);
}

#[tokio::test]
async fn tool_failure_context_is_attached_to_the_failed_result() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_call("fail")]));
    let hooks = Arc::new(PointHooks::with(
        HookPoint::ToolFailure,
        Reply::Context("run `make setup` first"),
    ));
    let kernel = build(
        provider.clone(),
        Arc::new(InMemoryLog::new()),
        hooks.clone(),
    );
    run(&kernel, &Session::new(), &Notices::default()).await;
    assert_eq!(hooks.count(HookPoint::ToolFailure), 1);
    let second = provider.requests.lock().unwrap()[1].clone();
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.content.contains("run `make setup` first")
                && message.content.contains("dev.test/guard")),
        "the model reads the hook note with the tool error"
    );

    let provider = Arc::new(ScriptedProvider::new(vec![tool_call("read")]));
    let hooks = Arc::new(PointHooks::default());
    let kernel = build(provider, Arc::new(InMemoryLog::new()), hooks.clone());
    run(&kernel, &Session::new(), &Notices::default()).await;
    assert_eq!(hooks.count(HookPoint::ToolFailure), 0);
    assert_eq!(hooks.count(HookPoint::PostTool), 1);
}

#[tokio::test]
async fn session_start_runs_once_per_session_and_not_for_sub_agents() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let hooks = Arc::new(PointHooks::default());
    let kernel = build(provider, Arc::new(InMemoryLog::new()), hooks.clone());
    let session = Session::new();
    run(&kernel, &session, &Notices::default()).await;
    run(&kernel, &session, &Notices::default()).await;
    assert_eq!(hooks.count(HookPoint::SessionStart), 1);
    assert_eq!(hooks.count(HookPoint::PromptSubmit), 2);

    let child = kernel.derive(
        Arc::new(ReadTools),
        Arc::new(Passthrough),
        Arc::new(AutoDeny),
    );
    run(&child, &Session::new(), &Notices::default()).await;
    assert_eq!(hooks.count(HookPoint::SessionStart), 1);
    assert_eq!(hooks.count(HookPoint::PromptSubmit), 2);
}
