use super::*;

struct SmallRead(AtomicUsize);
#[async_trait]
impl Executor for SmallRead {
    fn specs(&self) -> Vec<ToolSpec> {
        LargeTool(AtomicUsize::new(0)).specs()
    }
    fn blast_radius(&self, _: &str) -> Option<BlastRadius> {
        Some(BlastRadius::Read)
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.0.fetch_add(1, Ordering::SeqCst);
        Observation::ok(&intent.id, json!({"content":"read file material"}))
    }
}

#[derive(Default)]
struct BudgetSink {
    notices: Mutex<Vec<String>>,
    costs: Mutex<Vec<f64>>,
}
impl StreamSink for BudgetSink {
    fn notice(&self, text: &str) {
        self.notices.lock().unwrap().push(text.into());
    }
    fn cost(&self, cost: f64, _: bool) {
        self.costs.lock().unwrap().push(cost);
    }
}

#[tokio::test]
async fn capped_adaptive_tasks_work_without_a_server_counter_and_fund_one_tool_free_finish() {
    let mut endpoint = Endpoint::new(
        32_000,
        vec![
            vec![
                Block::ToolIntent(ToolIntent {
                    id: "read-one".into(),
                    tool: "test.read".into(),
                    args: json!({"path":"note"}),
                }),
                Block::Usage(Usage {
                    prompt_tokens: 300,
                    completion_tokens: 4000,
                    total_tokens: 4300,
                    ..Default::default()
                }),
            ],
            vec![
                Block::Text("Read the note. No files were changed. Further work remains.".into()),
                Block::Usage(Usage {
                    prompt_tokens: 300,
                    completion_tokens: 20,
                    total_tokens: 320,
                    ..Default::default()
                }),
            ],
        ],
    );
    Arc::get_mut(&mut endpoint).unwrap().counter_available = false;
    let log = Arc::new(InMemoryLog::new());
    let tool = Arc::new(SmallRead(AtomicUsize::new(0)));
    let kernel = Kernel::new(
        endpoint.clone(),
        log.clone(),
        tool.clone(),
        Arc::new(PipelineEngine::default()),
        Arc::new(NoArtifacts),
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    )
    .with_pricing(Some(Pricing {
        input_per_mtok: 1.0,
        output_per_mtok: 1.0,
        cached_input_per_mtok: None,
        cache_write_per_mtok: None,
        indicative: false,
    }));
    let session = Session::new();
    let sink = BudgetSink::default();
    let (messages, stop) = kernel
        .run_session(
            &session,
            vec![
                Message::system("Answer plainly."),
                Message::user("Read the note and complete the task."),
            ],
            Budget {
                max_cost_usd: Some(0.006),
                ..Default::default()
            },
            &sink,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(stop, StopReason::Budget(BudgetStop::Cost)));
    assert_eq!(tool.0.load(Ordering::SeqCst), 1);
    let sent = endpoint.sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        2,
        "one working request and at most one finishing request"
    );
    assert!(!sent[0].context.tools.is_empty());
    assert!(sent[1].context.tools.is_empty());
    assert!(sent[1].body["max_tokens"].as_u64().unwrap() <= 512);
    assert!(
        messages
            .last()
            .unwrap()
            .content
            .contains("Further work remains")
    );
    assert!(sink.costs.lock().unwrap().iter().all(|cost| *cost <= 0.006));
    assert!(
        log.events(session.id)
            .await
            .iter()
            .any(|event| event.kind == EventKind::ModelText
                && event.payload.to_string().contains("Further work remains"))
    );
}
