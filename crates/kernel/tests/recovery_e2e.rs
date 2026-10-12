//! A reply cut off at the output limit, or a model that cannot be used, is
//! recovered from where another answer is still possible.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::recovery::Rest;
use kernel::{
    AllowAll, AutoDeny, Block, Budget, CompileResult, CompiledContext, ContextEngine, EventKind,
    EventLog, Executor, InMemoryLog, InterruptQueue, Kernel, Message, NoVerify, Observation,
    PreparedModelRequest, Provider, ProviderCaps, ProviderError, Role, Session, StopReason,
    ToolCallStrategy, ToolIntent, ToolSpec,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// How a scripted reply ends.
#[derive(Clone, Copy, PartialEq)]
enum End {
    Whole,
    /// At the output limit.
    Cut,
    /// The stream goes silent.
    Stalled,
    /// A part of the reply arrives that cannot be read.
    Unreadable,
}
use End::{Cut, Stalled, Unreadable, Whole};

/// One reply: its blocks, then how it ends.
type Reply = (Vec<Block>, End);

struct Scripted {
    caps: ProviderCaps,
    replies: Mutex<VecDeque<Reply>>,
    /// What every later reply is, once the script runs out.
    then: Reply,
    asked: Mutex<Vec<PreparedModelRequest>>,
    /// The status the user's own model answers with, when it cannot be used.
    down: Option<u16>,
    stand_ins: usize,
    /// How many stand-ins have been switched to, and how often the own model was restored.
    standing_in: Mutex<usize>,
    restores: Mutex<usize>,
    /// The rate the person set for their own model, when they set one.
    own_rate: Mutex<Option<kernel::Pricing>>,
    /// How many times the person lets a turn wait for a server that is away.
    server_waits: Mutex<u32>,
    /// The own model's server takes a model's earlier thinking back.
    takes_thinking_back: Mutex<bool>,
    /// How long each model that was left was to be rested.
    rests: Mutex<Vec<Rest>>,
    /// After this many requests the own model answers again.
    down_for: Mutex<usize>,
    /// The request by which a sub-agent sharing the connection has switched it.
    moved_at: Mutex<usize>,
    /// The words the own model's server refuses with.
    refusal: Mutex<String>,
    /// How many keys are saved for the own model's server, and how many of
    /// them, counted from the first, its server turns away.
    keys: Mutex<usize>,
    bad_keys: Mutex<usize>,
    /// The key in use, counted from 1, and the answers that made it change.
    key: Mutex<usize>,
    swaps: Mutex<Vec<u16>>,
}

impl Scripted {
    fn new(replies: Vec<Reply>, then: Reply) -> Arc<Self> {
        Self::down(None, 0, replies, then)
    }

    fn down(down: Option<u16>, stand_ins: usize, replies: Vec<Reply>, then: Reply) -> Arc<Self> {
        Arc::new(Self {
            caps: ProviderCaps {
                images: kernel::ImageSupport::Unsupported,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
            replies: Mutex::new(replies.into()),
            then,
            asked: Mutex::new(Vec::new()),
            down,
            stand_ins,
            standing_in: Mutex::new(0),
            restores: Mutex::new(0),
            own_rate: Mutex::new(None),
            server_waits: Mutex::new(kernel::recovery::WAITS),
            takes_thinking_back: Mutex::new(false),
            rests: Mutex::new(Vec::new()),
            down_for: Mutex::new(usize::MAX),
            moved_at: Mutex::new(usize::MAX),
            refusal: Mutex::new("refused".into()),
            keys: Mutex::new(1),
            bad_keys: Mutex::new(usize::MAX),
            key: Mutex::new(1),
            swaps: Mutex::new(Vec::new()),
        })
    }

    /// The output allowance each request asked for.
    fn room_asked(&self) -> Vec<Option<u64>> {
        let asked = self.asked.lock().unwrap();
        asked
            .iter()
            .map(|request| request.body["max_tokens"].as_u64())
            .collect()
    }
}

#[async_trait]
impl Provider for Scripted {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }

    fn with_output_limit(
        &self,
        request: &PreparedModelRequest,
        max_output_tokens: u64,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        body["max_tokens"] = json!(max_output_tokens);
        Ok(Some(request.with_body(body)))
    }

    fn with_plain_schemas(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        body["plain_schemas"] = json!(true);
        Ok(Some(request.with_body(body)))
    }

    fn with_request_reasoning(
        &self,
        request: &PreparedModelRequest,
        config: &kernel::ReasoningConfig,
    ) -> Result<Option<PreparedModelRequest>, ProviderError> {
        let mut body = request.body.clone();
        body["thinking"] = json!(config.enabled != Some(false));
        Ok(Some(request.with_body(body)))
    }

    fn fall_back(&self, _failed: &str, rest: Rest) -> Option<(String, String)> {
        let mut standing_in = self.standing_in.lock().unwrap();
        (*standing_in < self.stand_ins).then(|| {
            *standing_in += 1;
            self.rests.lock().unwrap().push(rest);
            ("own-model".into(), format!("stand-in-{standing_in}"))
        })
    }

    fn another_key(&self) -> bool {
        *self.key.lock().unwrap() < *self.keys.lock().unwrap()
    }

    fn next_key(&self, status: u16, _: Option<std::time::Duration>) -> Option<(usize, usize)> {
        let (mut key, keys) = (self.key.lock().unwrap(), *self.keys.lock().unwrap());
        (*key < keys).then(|| {
            *key += 1;
            self.swaps.lock().unwrap().push(status);
            (*key, keys)
        })
    }

    fn context_identity(&self) -> String {
        match *self.standing_in.lock().unwrap() {
            0 => "own-model".into(),
            stand_in => format!("stand-in-{stand_in}"),
        }
    }

    fn standing_in(&self) -> Option<(String, String)> {
        let standing_in = *self.standing_in.lock().unwrap();
        (standing_in > 0).then(|| ("own-model".into(), format!("stand-in-{standing_in}")))
    }

    fn replays_reasoning(&self) -> bool {
        *self.takes_thinking_back.lock().unwrap()
    }

    fn stand_in_left(&self) -> bool {
        *self.standing_in.lock().unwrap() < self.stand_ins
    }

    /// The own model costs $1 per million tokens read; a stand-in costs $10.
    fn list_price(&self) -> Option<kernel::Pricing> {
        let input_per_mtok = match *self.standing_in.lock().unwrap() {
            0 => 1.0,
            _ => 10.0,
        };
        Some(kernel::Pricing {
            input_per_mtok,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            indicative: true,
        })
    }

    fn restore_primary(&self) -> Option<(String, String)> {
        *self.restores.lock().unwrap() += 1;
        *self.standing_in.lock().unwrap() = 0;
        None
    }

    async fn stream(
        &self,
        _: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        Err(ProviderError::Decode(
            "only prepared requests are scripted".into(),
        ))
    }

    async fn stream_prepared(
        &self,
        request: &PreparedModelRequest,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.asked.lock().unwrap().push(request.clone());
        // By this request another caller sharing the connection has moved it.
        if self.asked.lock().unwrap().len() == *self.moved_at.lock().unwrap() {
            *self.standing_in.lock().unwrap() = 1;
        }
        let still_down = self.asked.lock().unwrap().len() <= *self.down_for.lock().unwrap();
        let own = *self.standing_in.lock().unwrap() == 0;
        let bad_key = *self.key.lock().unwrap() <= *self.bad_keys.lock().unwrap();
        if let Some(status) = self.down.filter(|_| own && still_down && bad_key) {
            let refusal = self.refusal.lock().unwrap().clone();
            return Err(ProviderError::Status(status, refusal));
        }
        let next = self.replies.lock().unwrap().pop_front();
        let (blocks, end) = next.unwrap_or_else(|| self.then.clone());
        let mut items: Vec<_> = blocks.into_iter().map(Ok).collect();
        match end {
            Whole => {}
            Cut => items.push(Err(ProviderError::Truncated)),
            Stalled => items.push(Err(ProviderError::StreamStalled { waited_secs: 180 })),
            Unreadable => items.push(Err(ProviderError::Garbled("unreadable".into()))),
        }
        Ok(stream::iter(items).boxed())
    }
}

/// Sends the chat as it is, and keeps the thinking it was told goes back.
#[derive(Default)]
struct Passthrough(Mutex<String>);

#[async_trait]
impl ContextEngine for Passthrough {
    fn note_thinking_sent_back(&self, thinking: &str) {
        self.0.lock().unwrap().push_str(thinking);
    }

    async fn compile(&self, messages: &[Message], _: Option<u32>) -> CompileResult {
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

struct NoArtifacts;

impl kernel::ArtifactStore for NoArtifacts {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Ok("hash".into())
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Ok(0)
    }
}

#[derive(Default)]
struct Notes(Mutex<Vec<ToolIntent>>);

#[async_trait]
impl Executor for Notes {
    fn specs(&self) -> Vec<ToolSpec> {
        let spec = |name: &str| ToolSpec {
            name: name.into(),
            description: String::new(),
            schema: json!({ "type": "object" }),
            blast_radius: kernel::BlastRadius::Read,
            category: kernel::ToolCategory::Read,
            icon: String::new(),
        };
        vec![spec("note"), spec("update_plan")]
    }

    fn blast_radius(&self, _: &str) -> Option<kernel::BlastRadius> {
        Some(kernel::BlastRadius::Read)
    }

    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.0.lock().unwrap().push(intent.clone());
        Observation::ok(&intent.id, json!({ "noted": true }))
    }
}

/// The reply text a person would see, taken back when the kernel says so.
#[derive(Default)]
struct Screen(Mutex<String>);

impl kernel::StreamSink for Screen {
    fn text(&self, delta: &str) {
        self.0.lock().unwrap().push_str(delta);
    }
    fn restarted(&self) {
        self.0.lock().unwrap().clear();
    }
    fn supports_restart(&self) -> bool {
        true
    }
}

struct Ran {
    stop: Result<StopReason, String>,
    messages: Vec<Message>,
    log: Arc<InMemoryLog>,
    session: Session,
    executed: usize,
    shown: String,
    /// Thinking the size estimate was told goes back to the model.
    sized: String,
}

async fn run(provider: Arc<Scripted>) -> Ran {
    run_hearing(provider, None).await
}

/// Run one message, with the queue a person's later messages arrive on.
async fn run_hearing(provider: Arc<Scripted>, heard: Option<InterruptQueue>) -> Ran {
    let log = Arc::new(InMemoryLog::new());
    let notes = Arc::new(Notes::default());
    let own_rate = *provider.own_rate.lock().unwrap();
    let waits = *provider.server_waits.lock().unwrap();
    let context = Arc::new(Passthrough::default());
    let kernel = Kernel::new(
        provider,
        log.clone(),
        notes.clone(),
        context.clone(),
        Arc::new(NoArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
    .with_pricing(own_rate)
    .with_server_waits(waits);
    let session = Session::new();
    let asked = vec![Message::user("write it")];
    let screen = Screen::default();
    let turn = kernel.run_session(&session, asked, Budget::default(), &screen, heard);
    let (stop, messages) = match turn.await {
        Ok((messages, stop)) => (Ok(stop), messages),
        Err(error) => (Err(error.to_string()), Vec::new()),
    };
    let executed = notes.0.lock().unwrap().len();
    Ran {
        stop,
        messages,
        log,
        session,
        executed,
        shown: screen.0.into_inner().unwrap(),
        sized: context.0.lock().unwrap().clone(),
    }
}

fn text(said: &str) -> Block {
    Block::Text(said.into())
}

/// A long answer cut off at the limit is kept and continued from where it
/// stopped. The model is asked in a message that is never shown as the user's.
#[tokio::test]
async fn a_cut_off_answer_is_kept_and_continued() {
    let provider = Scripted::new(
        vec![(vec![text("The first half, ")], Cut)],
        (vec![text("and the second.")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));

    let said: Vec<_> = ran
        .messages
        .iter()
        .map(|m| (&m.role, m.content.as_str()))
        .collect();
    assert_eq!(said[1], (&Role::Assistant, "The first half, "));
    assert_eq!(said[2].0, &Role::User);
    assert!(
        said[2].1.contains("truncated by the output length limit"),
        "{said:?}"
    );
    assert_eq!(said[3], (&Role::Assistant, "and the second."));

    let second = provider.asked.lock().unwrap()[1].clone();
    let sent = second.context.ordered_messages();
    assert_eq!(
        sent.len(),
        3,
        "the fragment and the request to go on were both sent"
    );
    let events = ran.log.events(ran.session.id).await;
    let asked: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::UserMessage)
        .map(|event| event.is_from_person())
        .collect();
    assert_eq!(
        asked,
        [true, false],
        "only the first message is the person's"
    );
}

/// A model that is cut off every time is continued three times, not forever,
/// and the turn then ends with everything it wrote.
#[tokio::test]
async fn an_answer_that_keeps_being_cut_off_stops_after_three_continuations() {
    let provider = Scripted::new(Vec::new(), (vec![text("more ")], Cut));
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 4);
    let written = ran
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count();
    assert_eq!(written, 4, "every fragment is kept");
}

/// A tool call cut off before its arguments are complete is never run as it
/// is. The same call is asked for again with more room each time.
#[tokio::test]
async fn a_cut_off_tool_call_is_retried_with_more_room_and_never_run_incomplete() {
    let started = Block::ToolStarted {
        name: "note".into(),
        target: None,
    };
    let whole = Block::ToolIntent(ToolIntent {
        id: "call-1".into(),
        tool: "note".into(),
        args: json!({ "text": "all of it" }),
    });
    let provider = Scripted::new(
        vec![
            (vec![started.clone()], Cut),
            (vec![started.clone()], Cut),
            (vec![whole], Whole),
        ],
        (vec![text("done")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(ran.executed, 1, "the complete call ran once");
    // No allowance is configured here, so each retry builds on the one before.
    assert_eq!(
        provider.room_asked()[..3],
        [None, Some(8_192), Some(32_768)],
        "each retry asked for more room"
    );

    // Cut off every time: four retries, then the turn ends and nothing ran.
    let stuck = Scripted::new(Vec::new(), (vec![started], Cut));
    let ran = run(stuck.clone()).await;
    let error = ran.stop.unwrap_err();
    assert!(
        error.contains("the incomplete action was not run"),
        "{error}"
    );
    assert_eq!(stuck.asked.lock().unwrap().len(), 5);
    assert_eq!(ran.executed, 0);
}

/// Thinking that used the whole allowance leaves nothing to continue from, so
/// the same request is sent once more with thinking off. If that too writes
/// nothing, the person is told what to change.
#[tokio::test]
async fn a_reply_cut_off_before_any_text_is_asked_for_again_without_thinking() {
    let provider = Scripted::new(vec![(Vec::new(), Cut)], (vec![text("answer")], Whole));
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(ran.messages.last().unwrap().content, "answer");
    let thinking: Vec<_> = provider
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.body["thinking"].clone())
        .collect();
    assert_eq!(thinking, [json!(null), json!(false)]);

    let silent = Scripted::new(Vec::new(), (Vec::new(), Cut));
    let error = run(silent.clone()).await.stop.unwrap_err();
    assert!(error.contains("before writing anything"), "{error}");
    assert_eq!(
        silent.asked.lock().unwrap().len(),
        2,
        "once more, not forever"
    );
}

/// A model stuck repeating one line until its limit is not asked to continue:
/// that would only buy more of it. Nothing of the loop is kept.
#[tokio::test]
async fn a_reply_that_is_one_line_repeated_is_not_continued() {
    let looped = "I will now continue with the next step of the task.\n".repeat(40);
    let provider = Scripted::new(Vec::new(), (vec![text(&looped)], Cut));
    let ran = run(provider.clone()).await;
    let error = ran.stop.unwrap_err();
    assert!(error.contains("repeating the same text"), "{error}");
    assert_eq!(provider.asked.lock().unwrap().len(), 1);
    let events = ran.log.events(ran.session.id).await;
    let kept = events
        .iter()
        .any(|event| event.kind == EventKind::ModelText);
    assert!(!kept, "the loop was saved into the chat");
}

/// A model whose key is refused does not end the turn when the user listed
/// another: that one answers, and the next message tries their own model again.
#[tokio::test]
async fn an_unusable_model_hands_the_turn_to_the_next_one_listed() {
    let answered = (vec![text("answered")], Whole);
    let provider = Scripted::down(Some(401), 1, Vec::new(), answered.clone());
    for turn in 1..=2 {
        let ran = run(provider.clone()).await;
        assert_eq!(ran.stop, Ok(StopReason::Finished), "turn {turn}");
        assert_eq!(ran.messages.last().unwrap().content, "answered");
        assert_eq!(
            *provider.restores.lock().unwrap(),
            turn,
            "the own model is tried first"
        );
    }
    // Each turn: one refused request to the own model, then the stand-in's.
    assert_eq!(provider.asked.lock().unwrap().len(), 4);

    // Nobody listed: the provider's own words end the turn, as before.
    let alone = Scripted::down(Some(401), 0, Vec::new(), answered.clone());
    let error = run(alone).await.stop.unwrap_err();
    assert!(error.contains("401"), "{error}");

    // A request this server will not take may be one another server does.
    let refusing = Scripted::down(Some(400), 1, Vec::new(), answered);
    let ran = run(refusing.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(refusing.asked.lock().unwrap().len(), 2);

    // A fault in the reply itself is not handed on: another model would repeat it.
    let cut = Scripted::down(None, 1, Vec::new(), (Vec::new(), Cut));
    let error = run(cut.clone()).await.stop.unwrap_err();
    assert!(error.contains("before writing anything"), "{error}");
    assert_eq!(*cut.standing_in.lock().unwrap(), 0);
}

/// A server that fails every time is tried again five times, and only then
/// does the next model listed take the turn. Its answer is marked as a stand-in's.
#[tokio::test(start_paused = true)]
async fn a_server_that_keeps_failing_is_retried_before_another_model_answers() {
    let answered = (vec![text("answered")], Whole);
    let provider = Scripted::down(Some(500), 1, Vec::new(), answered.clone());
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(
        provider.asked.lock().unwrap().len(),
        7,
        "the first try, five retries, then the stand-in"
    );
    assert_eq!(*provider.rests.lock().unwrap(), [Rest::NextMessage]);

    // An overloaded server is tried twice more, not five times, when another can answer.
    let busy = Scripted::down(Some(503), 1, Vec::new(), answered.clone());
    assert_eq!(run(busy.clone()).await.stop, Ok(StopReason::Finished));
    assert_eq!(busy.asked.lock().unwrap().len(), 4);

    // A rate limit is not waited out at all, and that model is then rested.
    let limited = Scripted::down(Some(429), 1, Vec::new(), answered.clone());
    assert_eq!(run(limited.clone()).await.stop, Ok(StopReason::Finished));
    assert_eq!(limited.asked.lock().unwrap().len(), 2);
    assert_eq!(*limited.rests.lock().unwrap(), [Rest::Limited(None)]);

    // With nobody to hand over to, a rate limit is waited out as before.
    let alone = Scripted::down(Some(429), 0, Vec::new(), answered);
    let error = run(alone.clone()).await.stop.unwrap_err();
    assert!(error.contains("429"), "{error}");
    assert_eq!(alone.asked.lock().unwrap().len(), 6);
    let events = ran.log.events(ran.session.id).await;
    let reply = events
        .iter()
        .find(|event| event.kind == EventKind::ModelText)
        .expect("the answer was saved");
    assert_eq!(
        reply.payload["stand_in"],
        json!({ "model": "stand-in-1", "for": "own-model" })
    );
}

/// A stream that dies while the model writes a tool call never runs the call
/// half-written. With nothing else written it is reopened twice first; the
/// model is then told to do the work in smaller calls, sized to what it can write.
#[tokio::test(start_paused = true)]
async fn a_stream_dying_under_a_tool_call_ends_in_advice_to_split_the_work() {
    let started = Block::ToolStarted {
        name: "note".into(),
        target: None,
    };
    let whole = Block::ToolIntent(ToolIntent {
        id: "call-1".into(),
        tool: "note".into(),
        args: json!({ "text": "the first part" }),
    });
    let dying = (vec![started.clone()], Stalled);
    let provider = Scripted::new(
        vec![
            dying.clone(),
            dying.clone(),
            dying.clone(),
            (vec![whole.clone()], Whole),
        ],
        (vec![text("done")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(ran.executed, 1, "only the complete call ran");
    assert_eq!(provider.asked.lock().unwrap().len(), 5);
    let advice = ran.messages[1].content.as_str();
    assert_eq!(ran.messages[1].role, Role::User);
    assert!(
        advice.contains("previous tool call (note)") && advice.contains("smaller tool calls"),
        "{advice}"
    );
    assert!(advice.contains("under ~8000 tokens"), "{advice}");

    // Dying every time: three rounds of advice, then the turn ends with the reason.
    let stuck = Scripted::new(Vec::new(), dying);
    let error = run(stuck.clone()).await.stop.unwrap_err();
    assert!(error.contains("stopped reconnecting"), "{error}");
    assert_eq!(
        stuck.asked.lock().unwrap().len(),
        12,
        "four rounds of three"
    );

    // With text already written, that text is kept and the advice comes at once.
    let wrote_first = (vec![text("I will write it. "), started], Stalled);
    let provider = Scripted::new(
        vec![wrote_first, (vec![whole], Whole)],
        (vec![text("done")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(
        provider.asked.lock().unwrap().len(),
        3,
        "nothing was resent"
    );
    assert_eq!(ran.messages[1].content, "I will write it. ");
    assert!(ran.messages[2].content.contains("smaller tool calls"));
}

/// With more than one key saved for a server, a key the server turns away is
/// swapped for the next before any other model is tried. A rate limit is given
/// one more try on the same key first. Stand-ins come only when every key failed.
#[tokio::test(start_paused = true)]
async fn the_next_saved_key_is_tried_before_another_model() {
    let answered = (vec![text("answered")], Whole);
    let with_keys = |status: u16, bad: usize, stand_ins: usize| {
        let provider = Scripted::down(Some(status), stand_ins, Vec::new(), answered.clone());
        *provider.keys.lock().unwrap() = 2;
        *provider.bad_keys.lock().unwrap() = bad;
        provider
    };
    for (status, requests) in [(401, 2), (402, 2), (429, 3)] {
        let provider = with_keys(status, 1, 1);
        let ran = run(provider.clone()).await;
        assert_eq!(ran.stop, Ok(StopReason::Finished), "{status}");
        assert_eq!(provider.asked.lock().unwrap().len(), requests, "{status}");
        assert_eq!(*provider.swaps.lock().unwrap(), [status]);
        assert_eq!(*provider.standing_in.lock().unwrap(), 0, "no other model");
    }

    // A plan's usage limit will not lift on a second try: the next key at once.
    let provider = with_keys(429, 1, 1);
    *provider.refusal.lock().unwrap() = r#"{"type":"usage_limit_reached"}"#.into();
    assert_eq!(run(provider.clone()).await.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 2);

    // A limit passed on from further upstream is not this key's. No key is
    // rested for it, and the model listed to stand in answers.
    let provider = with_keys(429, 1, 1);
    *provider.refusal.lock().unwrap() =
        r#"{"error":{"message":"Provider returned error","metadata":{"provider_name":"x"}}}"#
            .into();
    assert_eq!(run(provider.clone()).await.stop, Ok(StopReason::Finished));
    assert!(
        provider.swaps.lock().unwrap().is_empty(),
        "a good key was rested"
    );
    assert_eq!(*provider.standing_in.lock().unwrap(), 1);

    // Both keys refused: only then does the model listed to stand in answer.
    let provider = with_keys(401, 2, 1);
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 3);
    assert_eq!(*provider.standing_in.lock().unwrap(), 1);
}

/// A server that refuses one thing in the request gets the request once more
/// without it: the reasoning setting it will not take, or the pattern rules in
/// tool descriptions it cannot use. A block under content rules is not sent
/// again, and is said in plain words.
#[tokio::test]
async fn a_request_refused_for_one_thing_is_sent_once_more_without_it() {
    let answered = (vec![text("answered")], Whole);
    let refusing = |words: &str, tries: usize| {
        let provider = Scripted::down(Some(400), 0, Vec::new(), answered.clone());
        *provider.refusal.lock().unwrap() = words.into();
        *provider.down_for.lock().unwrap() = tries;
        provider
    };
    let second_body = |provider: &Scripted, key: &str| {
        let asked = provider.asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "one refusal, one answer");
        asked[1].body[key].clone()
    };

    let mandatory = "Reasoning is mandatory for this endpoint and cannot be disabled";
    let provider = refusing(mandatory, 1);
    assert_eq!(run(provider.clone()).await.stop, Ok(StopReason::Finished));
    assert_eq!(second_body(&provider, "thinking"), json!(true));

    let provider = refusing("error parsing grammar: unknown escape at \\d", 1);
    assert_eq!(run(provider.clone()).await.stop, Ok(StopReason::Finished));
    assert_eq!(second_body(&provider, "plain_schemas"), json!(true));

    // Refused again without it: the server's words end the turn, after one retry only.
    let stuck = refusing(mandatory, usize::MAX);
    let error = run(stuck.clone()).await.stop.unwrap_err();
    assert!(error.contains("Reasoning is mandatory"), "{error}");
    assert_eq!(stuck.asked.lock().unwrap().len(), 2);

    let blocked = refusing("Your request was flagged by our safety system", usize::MAX);
    let error = run(blocked.clone()).await.stop.unwrap_err();
    assert!(
        error.contains("blocked this request under its content rules")
            && error.contains("flagged by our safety system"),
        "{error}"
    );
    assert_eq!(blocked.asked.lock().unwrap().len(), 1);
}

/// A sub-agent shares its parent's connection. When the sub-agent is the one
/// that switched it to a stand-in, the parent's later answers are still marked
/// as the stand-in's, though the parent never saw the failure itself.
#[tokio::test]
async fn an_answer_is_marked_when_another_caller_made_the_switch() {
    let call = Block::ToolIntent(ToolIntent {
        id: "call-1".into(),
        tool: "note".into(),
        args: json!({}),
    });
    let provider = Scripted::new(vec![(vec![call], Whole)], (vec![text("done")], Whole));
    *provider.moved_at.lock().unwrap() = 2;
    let ran = run(provider).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    let events = ran.log.events(ran.session.id).await;
    let reply = events
        .iter()
        .find(|event| event.kind == EventKind::ModelText)
        .expect("the answer was saved");
    assert_eq!(
        reply.payload["stand_in"],
        json!({ "model": "stand-in-1", "for": "own-model" })
    );
}

/// A call is priced at the rate of the model that answered it. A chat whose
/// model changed is not billed at the rate it opened with.
#[tokio::test]
async fn a_call_costs_what_the_model_that_answered_it_charges() {
    let read = kernel::Usage {
        prompt_tokens: 1_000_000,
        total_tokens: 1_000_000,
        ..kernel::Usage::default()
    };
    let answered = (vec![Block::Usage(read), text("answered")], Whole);
    let cost = |ran: Ran| async move {
        let events = ran.log.events(ran.session.id).await;
        let used = events
            .iter()
            .find(|event| event.kind == EventKind::ContextUsage)
            .expect("the call's usage was saved");
        used.payload["cost_usd"].as_f64()
    };
    let own = Scripted::new(Vec::new(), answered.clone());
    assert_eq!(cost(run(own).await).await, Some(1.0));
    let stood_in = Scripted::down(Some(401), 1, Vec::new(), answered.clone());
    assert_eq!(cost(run(stood_in).await).await, Some(10.0));

    // A rate the person set is their own model's rate. It is used for that
    // model, and a stand-in is not passed off at it.
    let five = kernel::Pricing {
        input_per_mtok: 5.0,
        output_per_mtok: 0.0,
        cached_input_per_mtok: None,
        cache_write_per_mtok: None,
        indicative: false,
    };
    let own = Scripted::new(Vec::new(), answered.clone());
    *own.own_rate.lock().unwrap() = Some(five);
    assert_eq!(cost(run(own).await).await, Some(5.0));
    let stood_in = Scripted::down(Some(401), 1, Vec::new(), answered);
    *stood_in.own_rate.lock().unwrap() = Some(five);
    assert_eq!(cost(run(stood_in).await).await, Some(10.0));
}

/// A message typed while Medha waits for a server that is away is not left
/// waiting: the wait ends, and the very next request carries the message.
#[tokio::test(start_paused = true)]
async fn a_message_typed_during_a_wait_ends_the_wait() {
    let provider = Scripted::down(Some(500), 0, Vec::new(), (vec![text("answered")], Whole));
    *provider.down_for.lock().unwrap() = 6;
    let (handle, queue) = InterruptQueue::pair();
    let running = tokio::spawn(run_hearing(provider.clone(), Some(queue)));
    // Six tries take 20 seconds at most, and the wait after them at least 15 more.
    tokio::time::sleep(std::time::Duration::from_secs(22)).await;
    assert_eq!(provider.asked.lock().unwrap().len(), 6, "still waiting");
    handle.steer("never mind the essay, just say hi");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let asked = provider.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 7, "the wait ended when the message arrived");
    let carried = serde_json::to_string(&asked[6].context.ordered_messages()).unwrap();
    assert!(carried.contains("just say hi"), "{carried}");
    assert_eq!(running.await.unwrap().stop, Ok(StopReason::Finished));
}

/// A reply with no words and no action is not passed off as an answer. It is
/// never saved, the same request is sent again, and the turn carries on when
/// the model does answer.
#[tokio::test(start_paused = true)]
async fn an_empty_reply_is_asked_for_again_and_never_saved() {
    let call = |tool: &str| {
        Block::ToolIntent(ToolIntent {
            id: "call-1".into(),
            tool: tool.into(),
            args: json!({}),
        })
    };
    let nothing = (Vec::new(), Whole);
    let provider = Scripted::new(vec![nothing.clone()], (vec![text("answer")], Whole));
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 2);
    let said: Vec<_> = ran.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(said, ["write it", "answer"], "no empty turn was kept");

    // After its tools ran: told once to carry on, with no wait.
    let provider = Scripted::new(
        vec![(vec![call("note")], Whole), nothing.clone()],
        (vec![text("done")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 3);
    let nudge = &ran.messages[ran.messages.len() - 2];
    assert!(
        nudge.content.contains("returned an empty response"),
        "{nudge:?}"
    );

    // Text, then only tidying up, then nothing: the text was the answer.
    let provider = Scripted::new(
        vec![(vec![text("All set."), call("update_plan")], Whole)],
        nothing.clone(),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 2, "not asked again");

    // Thinking but never answering: three more tries, then the person is told.
    // Its server is plainly answering, so it is not kept waiting between tries.
    let thinking = (vec![Block::Reasoning("hmm".into())], Whole);
    let stuck = Scripted::new(Vec::new(), thinking);
    let began = tokio::time::Instant::now();
    let error = run(stuck.clone()).await.stop.unwrap_err();
    assert!(error.contains("kept answering with nothing"), "{error}");
    assert_eq!(stuck.asked.lock().unwrap().len(), 4);
    assert!(began.elapsed().is_zero(), "{:?}", began.elapsed());

    // No output at all, twice running: a third try would be the same, so none is made.
    let silent = Scripted::new(Vec::new(), nothing);
    assert!(run(silent.clone()).await.stop.is_err());
    assert_eq!(silent.asked.lock().unwrap().len(), 2);
}

/// A server that is only away for now, with no other model to turn to, does not
/// end the turn after the first round of retries: it is waited for and tried
/// again, five times at most, and the turn goes on as soon as it answers.
#[tokio::test(start_paused = true)]
async fn a_server_that_is_away_is_waited_for_when_nobody_else_can_answer() {
    let answered = (vec![text("answered")], Whole);
    let back = Scripted::down(Some(500), 0, Vec::new(), answered.clone());
    *back.down_for.lock().unwrap() = 8;
    let ran = run(back.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(ran.messages.last().unwrap().content, "answered");
    assert_eq!(
        back.asked.lock().unwrap().len(),
        9,
        "six tries, a wait, then it answers on the third"
    );

    // Never back: six rounds of six tries, and the server's own words at the end.
    let gone = Scripted::down(Some(500), 0, Vec::new(), answered.clone());
    let error = run(gone.clone()).await.stop.unwrap_err();
    assert!(error.contains("500"), "{error}");
    assert_eq!(gone.asked.lock().unwrap().len(), 36);

    // A refused key is not a server that is away: no waiting.
    let refused = Scripted::down(Some(401), 0, Vec::new(), answered.clone());
    assert!(run(refused.clone()).await.stop.is_err());
    assert_eq!(refused.asked.lock().unwrap().len(), 1);

    // The person set the number of waits to none: one round of tries, then the error.
    let impatient = Scripted::down(Some(500), 0, Vec::new(), answered);
    *impatient.server_waits.lock().unwrap() = 0;
    assert!(run(impatient.clone()).await.stop.is_err());
    assert_eq!(impatient.asked.lock().unwrap().len(), 6);
}

/// A reply that arrives unreadable may come through whole on another try. It
/// is asked for three times in all, and is not a server to sit and wait for.
#[tokio::test(start_paused = true)]
async fn an_unreadable_reply_is_asked_for_three_times_in_all() {
    let unreadable = (Vec::new(), Unreadable);
    let provider = Scripted::new(
        vec![unreadable.clone(), unreadable.clone()],
        (vec![text("answered")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 3);

    let never = Scripted::new(Vec::new(), unreadable);
    let error = run(never.clone()).await.stop.unwrap_err();
    assert!(error.contains("unreadable"), "{error}");
    assert_eq!(
        never.asked.lock().unwrap().len(),
        3,
        "no fourth try, no waiting"
    );
}

/// A stream that dies in the middle of an answer does not cost the answer:
/// what was written is kept and saved, and the model is asked to go on from it.
#[tokio::test(start_paused = true)]
async fn an_answer_whose_stream_dies_is_kept_and_continued_not_sent_for_again() {
    let provider = Scripted::new(
        vec![(vec![text("Half of the answer, ")], Stalled)],
        (vec![text("and the rest.")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(provider.asked.lock().unwrap().len(), 2, "one request each");
    let said: Vec<_> = ran
        .messages
        .iter()
        .map(|m| (&m.role, m.content.as_str()))
        .collect();
    assert_eq!(said[1], (&Role::Assistant, "Half of the answer, "));
    assert!(said[2].1.contains("cut off by a network error"), "{said:?}");
    assert_eq!(said[3], (&Role::Assistant, "and the rest."));
    let events = ran.log.events(ran.session.id).await;
    let saved = events
        .iter()
        .filter(|event| event.kind == EventKind::ModelText)
        .count();
    assert_eq!(saved, 2, "both parts are in the saved chat");

    // A connection that dies every time: three continuations, and all four parts kept.
    let more = |said| (vec![text(said)], Stalled);
    let stuck = Scripted::new(
        vec![
            more("The first part, "),
            more("then a second, "),
            more("then a third, "),
        ],
        more("and a fourth, "),
    );
    let ran = run(stuck.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(stuck.asked.lock().unwrap().len(), 4);
    let all = "The first part, then a second, then a third, and a fourth, ";
    assert_eq!(ran.shown, all);

    // Nothing written yet: the stream is reopened as before, twice, then given up.
    let silent = Scripted::new(Vec::new(), (Vec::new(), Stalled));
    let error = run(silent.clone()).await.stop.unwrap_err();
    assert!(error.contains("stopped reconnecting"), "{error}");
    assert_eq!(silent.asked.lock().unwrap().len(), 3);
}

/// Thinking counts toward the next request only where it will be in it.
/// Counted elsewhere, a chat is summarised early for nothing; left out here,
/// a request can be sent too big.
#[tokio::test]
async fn thinking_is_sized_only_where_it_goes_back_to_the_model() {
    let reply = || {
        (
            vec![Block::Reasoning("weighing it".into()), text("4")],
            Whole,
        )
    };
    let takes = Scripted::new(Vec::new(), reply());
    *takes.takes_thinking_back.lock().unwrap() = true;
    assert_eq!(run(takes).await.sized, "weighing it");

    let strict = Scripted::new(Vec::new(), reply());
    assert_eq!(run(strict).await.sized, "");
}

/// A model asked to go on often starts by writing the end of its last piece
/// again. The person never sees it twice, it is not saved twice, and it does
/// not go back to the model twice.
#[tokio::test(start_paused = true)]
async fn a_continued_reply_that_starts_by_repeating_itself_is_joined_once() {
    let first = "The parser keeps a stack of open blocks. When a block closes it is folded \
        into its parent, and the parent's span";
    let again = "When a block closes it is folded into its parent, and the parent's span";
    let texts = |messages: &[kernel::ModelMessage]| -> Vec<String> {
        let assistants = messages.iter().filter(|m| m.role == Role::Assistant);
        let text = |m: &kernel::ModelMessage| -> String {
            let parts = m.parts.iter().filter_map(|part| match part {
                kernel::ContentPart::Text(part) => Some(part.text.as_str()),
                _ => None,
            });
            parts.collect()
        };
        assistants.map(text).collect()
    };

    // The repeat arrives in pieces, as a stream delivers it, then the new text.
    let provider = Scripted::new(
        vec![
            (vec![text(first)], Stalled),
            (
                vec![
                    text(&again[..20]),
                    text(&again[20..]),
                    text(" is widened"),
                    text("."),
                ],
                Cut,
            ),
        ],
        (vec![text(" Errors are collected.")], Whole),
    );
    let ran = run(provider.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    let whole = format!("{first} is widened. Errors are collected.");
    assert_eq!(ran.shown, whole, "shown once, with nothing lost");
    let saved = kernel::project_ordered_messages(&ran.log.events(ran.session.id).await);
    assert_eq!(texts(&saved).concat(), whole, "saved once");
    let third = provider.asked.lock().unwrap()[2].clone();
    let sent = texts(&third.context.ordered_messages());
    assert_eq!(sent, [first, " is widened."], "sent back to the model once");

    // A reply that ends normally right after its repeat is joined the same way.
    let ended = Scripted::new(
        vec![(vec![text(first)], Stalled)],
        (vec![text(again), text(" is widened.")], Whole),
    );
    let ran = run(ended.clone()).await;
    assert_eq!(ran.shown, format!("{first} is widened."));
    let saved = kernel::project_ordered_messages(&ran.log.events(ran.session.id).await);
    assert_eq!(texts(&saved), [first, " is widened."]);

    // Nothing but the repeat: the reply was already whole, so the turn ends
    // there. It is not an empty reply, and it is not asked for again.
    let only = Scripted::new(
        vec![(vec![text(first)], Stalled)],
        (vec![text(again)], Whole),
    );
    let ran = run(only.clone()).await;
    assert_eq!(ran.stop, Ok(StopReason::Finished));
    assert_eq!(ran.shown, first);
    assert_eq!(only.asked.lock().unwrap().len(), 2);

    // A continuation that simply goes on is shown and saved exactly as written.
    let plain = Scripted::new(
        vec![(vec![text(first)], Stalled)],
        (vec![text(" is "), text("widened.")], Whole),
    );
    let ran = run(plain.clone()).await;
    assert_eq!(ran.shown, format!("{first} is widened."));
}
