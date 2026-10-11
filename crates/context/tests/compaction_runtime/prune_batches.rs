use super::*;

struct BatchTool;
#[async_trait]
impl Executor for BatchTool {
    fn specs(&self) -> Vec<ToolSpec> {
        LargeTool(AtomicUsize::new(0)).specs()
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        Observation::ok(
            &intent.id,
            json!({"content":format!("{} {}", intent.id, "x".repeat(1_000))}),
        )
    }
}

fn resumed(endpoint: Arc<Endpoint>, log: Arc<InMemoryLog>) -> Kernel<Endpoint, InMemoryLog> {
    Kernel::new(
        endpoint,
        log,
        Arc::new(BatchTool),
        Arc::new(PipelineEngine::with_counter(
            CompactionPolicy::default(),
            Arc::new(HeuristicCounter),
        )),
        Arc::new(NoArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
}

fn checkpoints(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::Compaction)
        .collect()
}

#[tokio::test]
async fn growing_chat_keeps_one_batch_across_restarts_until_its_runway_regrows() {
    let endpoint = Endpoint::new(20_000, vec![]);
    let log = Arc::new(InMemoryLog::new());
    let session = Session::new();
    let mut messages = vec![Message::system("fixed"), Message::user("start")];
    for index in 0..8 {
        let id = format!("old-{index}");
        messages.push(Message::assistant_calls(
            "",
            vec![ToolIntent {
                id: id.clone(),
                tool: "test.read".into(),
                args: json!({}),
            }],
        ));
        messages.push(Message::tool_result(
            id,
            format!("{index} {}", "a".repeat(6_000)),
        ));
    }
    messages.push(Message::user("continue"));
    let (mut history, _) = resumed(endpoint.clone(), log.clone())
        .run_session(&session, messages, Budget::default(), &NullSink, None)
        .await
        .unwrap();
    let events = log.events(session.id).await;
    let initial = checkpoints(&events);
    assert_eq!(initial.len(), 1);
    assert!(
        initial[0].payload["summary"].is_null(),
        "start with pruning, not summarization"
    );
    assert_eq!(
        initial[0].payload["policy_version"], 3,
        "a new prune must not look like an old checkpoint after restart"
    );
    let rearm = initial[0].payload["prune_rearm_tokens"].as_u64().unwrap();
    assert!(rearm > 0);
    let mut above_trigger_without_rewrite = false;
    let mut stable_turns = 0;
    let mut second_batch = false;
    for index in 0..80 {
        endpoint.turns.lock().unwrap().extend([
            vec![Block::ToolIntent(ToolIntent {
                id: format!("new-{index}"),
                tool: "test.read".into(),
                args: json!({}),
            })],
            vec![Block::Text("done".into())],
        ]);
        history.push(Message::user(format!("next {index}")));
        let (next, stop) = resumed(endpoint.clone(), log.clone())
            .run_session(&session, history, Budget::default(), &NullSink, None)
            .await
            .unwrap();
        assert_eq!(stop, StopReason::Finished);
        history = next;
        let events = log.events(session.id).await;
        let batches = checkpoints(&events);
        if batches.len() == 2 {
            assert!(
                batches[1].payload["summary"].is_null(),
                "second checkpoint on turn {index}, rearm={rearm}, before={}, after={}",
                batches[1].payload["before_tokens"],
                batches[1].payload["after_tokens"]
            );
            second_batch = true;
            break;
        }
        assert_eq!(batches.len(), 1, "a restart must restore the prune guard");
        stable_turns += 1;
        let requests = endpoint.sent.lock().unwrap();
        above_trigger_without_rewrite |= request_tokens(requests.last().unwrap()) >= 12_000;
        assert_eq!(batches[0].payload["prune_rearm_tokens"], rearm);
    }
    assert!(
        stable_turns > 10 && above_trigger_without_rewrite && second_batch,
        "must cross the prune trigger without per-turn rewrites, then commit another batch after regrowth"
    );
}
