use super::*;

struct Effectful(AtomicUsize);
#[async_trait]
impl Executor for Effectful {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "shell.exec".into(),
            description: String::new(),
            schema: json!({}),
            blast_radius: BlastRadius::IrreversibleLocal,
            category: kernel::ToolCategory::Other,
            icon: String::new(),
        }]
    }
    fn blast_radius(&self, _: &str) -> Option<BlastRadius> {
        Some(BlastRadius::IrreversibleLocal)
    }
    async fn execute(&self, intent: &ToolIntent) -> Observation {
        self.0.fetch_add(1, Ordering::SeqCst);
        Observation::ok(&intent.id, json!({}))
    }
}

#[derive(Default)]
struct Cards(Mutex<Vec<(String, bool)>>);
#[async_trait]
impl kernel::HumanGate for Cards {
    async fn confirm(&self, _: &str, detail: Option<&str>, escalated: bool) -> kernel::Approval {
        self.0
            .lock()
            .unwrap()
            .push((detail.unwrap_or_default().into(), escalated));
        kernel::Approval::Once
    }
}

#[tokio::test]
async fn a_command_asked_only_because_web_content_came_first_says_so_on_its_card() {
    let provider = Arc::new(LimitProvider::new(vec![Turn::Blocks(vec![
        Block::ToolIntent(ToolIntent {
            id: "run".into(),
            tool: "shell.exec".into(),
            args: json!({"command": "make install"}),
        }),
    ])]));
    let executor = Arc::new(Effectful(AtomicUsize::new(0)));
    let gate = Arc::new(Cards::default());
    let runner = Kernel::new(
        provider,
        Arc::new(InMemoryLog::new()),
        executor.clone(),
        Arc::new(Passthrough),
        Arc::new(MemArtifacts),
        Arc::new(AllowAll),
        gate.clone(),
        Arc::new(NoVerify),
    );
    let (_, stop) = runner
        .run_session(
            &Session::new(),
            vec![
                Message::user("install it"),
                Message::user("page text").carrying(kernel::TrustLabel::Web),
            ],
            Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, StopReason::Finished);
    assert_eq!(executor.0.load(Ordering::SeqCst), 1);
    let cards = gate.0.lock().unwrap();
    assert_eq!(
        cards.len(),
        1,
        "the web content alone sends it to the person"
    );
    let (detail, escalated) = &cards[0];
    assert!(escalated);
    assert!(
        detail.starts_with("This chat read web content earlier"),
        "the card says why it asks before what it runs: {detail}"
    );
}
