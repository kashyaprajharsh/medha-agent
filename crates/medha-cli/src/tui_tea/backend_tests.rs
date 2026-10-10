use super::*;
use serde_json::json;
#[path = "feature_tests.rs"]
mod parity;

fn setup_provider() -> protocol::ModelProvider {
    serde_json::from_value(json!({"id":"cloudflare-workers-ai","name":"Cloudflare Workers AI","url":"https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1","parameters":["CLOUDFLARE_ACCOUNT_ID"],"protocol":"open-ai-chat","auth":"bearer","available":true,"unavailable_reason":null,"doc":null,"env":[],"models":[{"id":"fixture-model","context_length":8000}]})).unwrap()
}
fn finish_setup_request(f: &mut Fixture, answer: Result<Answer, String>) {
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: answer,
        },
    );
}
fn setup_enter(model: &mut Model) {
    input::handle_key(model, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_setup_searches_pasted_text_resolves_parameters_and_keeps_secrets_out_of_transcript()
 {
    let mut f = fixture().await;
    f.model.remote.as_mut().unwrap().models.providers = vec![setup_provider()];
    begin_model_setup(&mut f.model);
    input::handle_paste(&mut f.model, "Cloudflare\n".into());
    assert!(
        matches!(&f.model.picker.as_ref().unwrap().kind, PickerKind::ProviderPreset { query, .. } if query == "Cloudflare")
    );
    assert!(f.model.input.is_empty());
    setup_enter(&mut f.model);
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().step,
        ModelSetupStep::EndpointParameter
    ));
    input::handle_paste(&mut f.model, "my-account".into());
    setup_enter(&mut f.model);
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect, Effect::ResolveDeployment(resolve) if resolve.parameters["CLOUDFLARE_ACCOUNT_ID"] == "my-account")
    );
    finish_setup_request(
        &mut f,
        Ok(Answer::Deployment(protocol::ModelDeployment {
            provider_id: "cloudflare-workers-ai".into(),
            protocol: protocol::ModelProtocol::OpenAiChat,
            auth: protocol::ModelAuth::Bearer,
            base_url: "https://api.cloudflare.com/client/v4/accounts/my-account/ai/v1".into(),
        })),
    );
    input::handle_paste(&mut f.model, "dummy-secret".into());
    setup_enter(&mut f.model);
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect, Effect::Discover(discover) if discover.key.0 == "dummy-secret" && discover.auth == Some(protocol::ModelAuth::Bearer))
    );
    finish_setup_request(
        &mut f,
        Err("Discovery unavailable; use catalogue or enter manually".into()),
    );
    input::handle_paste(&mut f.model, "fixture".into());
    assert!(
        matches!(&f.model.picker.as_ref().unwrap().kind, PickerKind::ModelDiscovery { query, .. } if query == "fixture")
    );
    setup_enter(&mut f.model);
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().step,
        ModelSetupStep::ConfirmTest
    ));
    setup_enter(&mut f.model); // blank -> Save, not Test
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect, Effect::Change(protocol::ChangeResource::SaveModel(draft), _) if draft.provider_id.as_deref() == Some("cloudflare-workers-ai") && draft.base_url.ends_with("my-account/ai/v1") && draft.model == "fixture-model")
    );
    assert!(f.model.history.is_empty());
    assert!(
        !f.model
            .items
            .iter()
            .any(|entry| format!("{:?}", entry.item).contains("dummy-secret"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_model_test_is_owned_by_its_form_and_does_not_save_or_activate() {
    let mut f = fixture().await;
    let mut setup = ModelSetup::new();
    setup.protocol = kernel::Protocol::OpenAiChat;
    setup.base_url = "http://localhost/v1".into();
    setup.model = "fixture".into();
    setup.step = ModelSetupStep::ConfirmTest;
    f.model.model_setup = Some(setup);
    input::handle_paste(&mut f.model, "test".into());
    setup_enter(&mut f.model);
    assert!(matches!(
        &f.model
            .remote
            .as_ref()
            .unwrap()
            .pending
            .front()
            .unwrap()
            .effect,
        Effect::TestModel(_)
    ));
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().step,
        ModelSetupStep::Testing
    ));
    input::handle_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    let check = protocol::ModelTestCheck {
        status: protocol::ModelTestStatus::Passed,
        message: "old test completed".into(),
        failure: None,
    };
    finish_setup_request(
        &mut f,
        Ok(Answer::Tested(protocol::ModelTestResult {
            text: check.clone(),
            streaming: check.clone(),
            tools: check,
        })),
    );
    assert!(f.model.model_setup.is_none());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    assert!(
        !f.model
            .items
            .iter()
            .any(|entry| format!("{:?}", entry.item).contains("old test completed"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalogue_refresh_preserves_provider_search_and_the_current_chat() {
    let mut f = fixture().await;
    f.model.remote.as_mut().unwrap().models.providers = vec![setup_provider()];
    begin_model_setup(&mut f.model);
    input::handle_paste(&mut f.model, "cloud".into());
    input::handle_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
    );
    assert!(matches!(
        f.model
            .remote
            .as_ref()
            .unwrap()
            .pending
            .front()
            .unwrap()
            .effect,
        Effect::RefreshProviders
    ));
    let conversation = f.model.remote.as_ref().unwrap().conversation.clone();
    finish_setup_request(
        &mut f,
        Ok(Answer::Providers(protocol::ProviderCatalogue {
            revision: "new".into(),
            providers: vec![setup_provider()],
        })),
    );
    assert!(
        matches!(&f.model.picker.as_ref().unwrap().kind, PickerKind::ProviderPreset { query, .. } if query == "cloud")
    );
    assert_eq!(f.model.remote.as_ref().unwrap().conversation, conversation);
    assert_eq!(f.model.remote.as_ref().unwrap().models.revision, "new");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_selection_chooses_its_protocol_and_only_custom_setup_asks_for_one() {
    let mut f = fixture().await;
    let mut gemini = setup_provider();
    gemini.id = "google".into();
    gemini.name = "Google Gemini".into();
    gemini.protocol = Some(protocol::ModelProtocol::GeminiInteractions);
    gemini.auth = protocol::ModelAuth::XGoogApiKey;
    gemini.url = Some("https://generativelanguage.googleapis.com/v1".into());
    gemini.parameters.clear();
    let mut unavailable = gemini.clone();
    unavailable.id = "future".into();
    unavailable.available = false;
    unavailable.unavailable_reason = Some("Native protocol not implemented".into());
    f.model.remote.as_mut().unwrap().models.providers = vec![unavailable, gemini];
    begin_model_setup(&mut f.model);
    assert!(matches!(
        f.model.picker.as_ref().unwrap().kind,
        PickerKind::ProviderPreset { .. }
    ));
    setup_enter(&mut f.model);
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    assert!(matches!(
        f.model.picker.as_ref().unwrap().kind,
        PickerKind::ProviderPreset { .. }
    ));
    assert!(
        f.model
            .picker
            .as_ref()
            .unwrap()
            .kind
            .labels()
            .iter()
            .any(|label| label.contains("Native protocol not implemented"))
    );
    input::handle_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
    );
    setup_enter(&mut f.model);
    let setup = f.model.model_setup.as_ref().unwrap();
    assert_eq!(setup.protocol, kernel::Protocol::GeminiInteractions);
    assert!(matches!(setup.step, ModelSetupStep::ApiKey));
    assert!(f.model.picker.is_none());

    begin_model_setup(&mut f.model);
    f.model.picker.as_mut().unwrap().selected = 2; // Custom follows both records.
    setup_enter(&mut f.model);
    assert!(matches!(
        f.model.picker.as_ref().unwrap().kind,
        PickerKind::ModelProtocol
    ));
    setup_enter(&mut f.model);
    assert!(f.model.picker.is_none());
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().step,
        ModelSetupStep::BaseUrl
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_deployment_setup_reuses_only_its_exact_key_and_preserves_its_auth_and_pinned_url() {
    let mut f = fixture().await;
    let profile: protocol::ModelProfile = serde_json::from_value(json!({
        "provider_id":"cloudflare-workers-ai", "auth":"x-api-key",
        "name":"old-model", "model":"old", "protocol":"open-ai-chat",
        "base_url":"https://gateway.test/tenant-a/v1", "context_limit":8000,
        "default":true, "key_present":true, "requires_key":true
    }))
    .unwrap();
    let mut duplicate = profile.clone();
    duplicate.name = "other-model".into();
    let peer = f.model.remote.as_mut().unwrap();
    peer.models.profiles = vec![profile, duplicate];
    peer.models.providers = vec![setup_provider()];
    begin_model_setup(&mut f.model);
    let labels = f.model.picker.as_ref().unwrap().kind.labels();
    assert_eq!(labels[0], "Saved — https://gateway.test/tenant-a/v1");
    assert_eq!(
        labels
            .iter()
            .filter(|label| label.starts_with("Saved"))
            .count(),
        1
    );
    setup_enter(&mut f.model);
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect,
        Effect::Discover(request) if request.base_url == "https://gateway.test/tenant-a/v1" && request.key.0.is_empty() && request.auth == Some(protocol::ModelAuth::XApiKey))
    );
    finish_setup_request(
        &mut f,
        Ok(Answer::Discovered(vec![protocol::DiscoveredModel {
            id: "new".into(),
            context_length: Some(8000),
        }])),
    );
    setup_enter(&mut f.model);
    setup_enter(&mut f.model);
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect,
        Effect::Change(protocol::ChangeResource::SaveModel(draft), _) if draft.base_url == "https://gateway.test/tenant-a/v1" && draft.auth == Some(protocol::ModelAuth::XApiKey) && draft.key.is_none() && draft.provider_id.as_deref() == Some("cloudflare-workers-ai"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_provider_needs_no_key_and_a_different_tenant_cannot_reuse_a_saved_key() {
    let mut f = fixture().await;
    let mut provider = setup_provider();
    provider.parameters.clear();
    provider.url = Some("http://localhost:11434/v1".into());
    provider.auth = protocol::ModelAuth::None;
    f.model.remote.as_mut().unwrap().models.providers = vec![provider.clone()];
    begin_model_setup(&mut f.model);
    setup_enter(&mut f.model);
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending.front().unwrap().effect,
        Effect::Discover(request) if request.auth == Some(protocol::ModelAuth::None) && request.key.0.is_empty())
    );
    finish_setup_request(&mut f, Err("Local server is not running".into()));

    provider.auth = protocol::ModelAuth::Bearer;
    provider.url = Some("https://gateway.test/tenant-b/v1".into());
    let peer = f.model.remote.as_mut().unwrap();
    peer.models.providers = vec![provider];
    peer.models.profiles = vec![
        serde_json::from_value(json!({
            "name":"tenant-a", "model":"old", "protocol":"open-ai-chat",
            "base_url":"https://gateway.test/tenant-a/v1", "context_limit":null,
            "default":true, "key_present":true, "requires_key":true
        }))
        .unwrap(),
    ];
    begin_model_setup(&mut f.model);
    f.model.picker.as_mut().unwrap().selected = 1; // Provider follows the saved deployment.
    setup_enter(&mut f.model);
    setup_enter(&mut f.model); // A blank key must be refused for tenant-b.
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().step,
        ModelSetupStep::ApiKey
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_recovery_has_a_place_when_all_human_controls_and_bytes_are_busy() {
    let mut f = fixture().await;
    let peer = f.model.remote.as_mut().unwrap();
    peer.control = CONTROL_REQUESTS;
    peer.bytes = REQUEST_BYTES;
    recover(&mut f.model);
    let peer = f.model.remote.as_ref().unwrap();
    assert!(peer.recovering);
    assert!(matches!(
        peer.pending.back().unwrap().effect,
        Effect::Snapshot
    ));
    assert_eq!(peer.control, CONTROL_REQUESTS + 1);
    assert!(peer.bytes <= REQUEST_BYTES + 256);
    recover(&mut f.model);
    assert_eq!(f.model.remote.as_ref().unwrap().pending.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returned_text_overload_keeps_both_drafts_in_one_private_recovery_file() {
    let mut f = fixture().await;
    let original = "अ".repeat(MAX_COMPOSER_BYTES / 3 - 10);
    f.model.input = original.clone();
    let returned = "returned 🦀".repeat(100);
    restore_draft(&mut f.model, returned.clone());
    assert!(f.model.should_quit);
    assert_eq!(f.model.input, original);
    assert_eq!(f.model.recovery_draft.as_deref(), Some(returned.as_str()));
    let path = save_recovery(&f.model.input, &f.model.pastes, &returned).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains(&original) && saved.contains(&returned));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn recovery_without_a_file_preserves_text_without_executing_terminal_controls() {
    let current = "unsent\nअ\x1b[2J";
    let pastes = vec!["paste\0\r🦀".into()];
    let returned = "returned\t\x1b]52;c;contents\x07";
    let mut output = Vec::new();
    write_recovery_fallback(&mut output, current, &pastes, returned).unwrap();
    assert!(!output.contains(&0x1b));
    assert!(!output.contains(&0x07));
    let saved: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(saved["current_composer"], current);
    assert_eq!(saved["pastes"], json!(pastes));
    assert_eq!(saved["returned_unsent_text"], returned);
    let mut buffer = [0_u8; 1];
    let mut unavailable = std::io::Cursor::new(&mut buffer[..]);
    assert!(write_recovery_fallback(&mut unavailable, current, &pastes, returned).is_err());
}

struct Fixture {
    model: Model,
    view: View,
    _root: tempfile::TempDir,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let address = if cfg!(windows) {
        format!(r"\\.\pipe\medha-tui-{}", ulid::Ulid::new())
    } else {
        root.path().join("s").display().to_string()
    };
    let listener = wire::bind(&address).unwrap();
    let server = tokio::spawn(listener.serve(|stream| { tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(stream); let mut read = tokio::io::BufReader::new(read);
        let roles = wire::Roles { host: "backend", guest: "client" };
        let Some(id) = wire::admit(&mut read, &mut write, "fixture", roles).await else { return; };
        wire::write_frame(&mut write, &json!({"id":id,"result":{"protocol":1,"build":wire::BUILD_ID,"capabilities":wire::CAPABILITIES}})).await;
        while let Some(frame) = wire::read_frame(&mut read).await {
            if frame["method"] == "session.attach" { wire::write_frame(&mut write, &json!({"id":frame["id"],"result":{"session":"chat","stream":"stream","head":0,"replayed":0,"gap":false}})).await; }
        }
    }); }));
    let (backend, connection) = tokio::task::spawn_blocking(move || {
        let backend = Backend::joined_to(address, "fixture");
        let connection = backend.connection().unwrap();
        (backend, connection)
    })
    .await
    .unwrap();
    let (view, _) = View::attach(
        connection.clone(),
        "chat".into(),
        None,
        ViewLimits::default(),
    )
    .await
    .unwrap();
    let mut model = Model::new(
        "fixture".into(),
        None,
        kernel::ReasoningConfig::default(),
        lockfile::UiConfig::default(),
        HashMap::new(),
        WorkspaceView {
            root: root.path().into(),
            execution: "native".into(),
        },
    );
    model.remote = Some(Peer {
        backend,
        connection,
        chat: view.chat(),
        folder: root.path().into(),
        startup: Default::default(),
        generation: 1,
        conversation: ulid::Ulid::new().to_string(),
        turn: 1,
        live: protocol::LiveState {
            agents: Vec::new(),
            tasks: Vec::new(),
            pending_patches: 0,
        },
        models: protocol::ModelCatalogue {
            profiles: Vec::new(),
            providers: Vec::new(),
            revision: String::new(),
            search: protocol::SearchProvider::Duckduckgo,
            searxng_url: None,
        },
        plugins: protocol::PluginCatalogue {
            plugins: Vec::new(),
            notices: Vec::new(),
            commands: Vec::new(),
            hook_review: None,
        },
        skills: None,
        mcp: Vec::new(),
        patches: Vec::new(),
        settings: None,
        sending: false,
        agent_requests: 0,
        settled_turn: 0,
        pending_steers: Vec::new(),
        normal: 0,
        control: 0,
        bytes: 0,
        pending: VecDeque::new(),
        live_pending: false,
        recovering: false,
        ended: false,
        approvals_answering: Default::default(),
        questions_answering: Default::default(),
        questions: VecDeque::new(),
    });
    Fixture {
        model,
        view,
        _root: root,
        server,
    }
}

fn snapshot(model: &Model, text: &str) -> protocol::PresentationSnapshot {
    protocol::PresentationSnapshot {
        current_turn_from: None,
        conversation: model.remote.as_ref().unwrap().conversation.clone(),
        turn: 1,
        items: vec![protocol::PresentationItem::User {
            text: text.into(),
            images: Vec::new(),
        }],
        revision: 0,
        running: false,
        pending_steers: Vec::new(),
        force_aborting: false,
        settings: None,
        omitted_items: 0,
        approvals: Vec::new(),
        questions: Vec::new(),
        metrics: Default::default(),
        agents: Vec::new(),
        roster: Vec::new(),
        omitted_agent_views: 0,
        cursor: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_replace_the_old_projection_without_consuming_new_composer_text() {
    let mut f = fixture().await;
    f.model.input = "unfinished draft".into();
    f.model.push_main_item(Item::User("old".into()));
    let snap = snapshot(&f.model, "new");
    restore_snapshot(&mut f.model, snap.clone());
    restore_snapshot(&mut f.model, snap);
    assert_eq!(f.model.items.len(), 1);
    assert!(matches!(&f.model.items[0].item, Item::User(text) if text == "new"));
    assert_eq!(f.model.input, "unfinished draft");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controls_have_reserved_slots_when_two_slow_queries_are_in_flight() {
    let mut f = fixture().await;
    assert!(request(
        &mut f.model,
        Effect::Read(Read::SkillSearch { query: "a".into() }, After::Skills)
    ));
    assert!(request(
        &mut f.model,
        Effect::Read(Read::SkillSearch { query: "b".into() }, After::Skills)
    ));
    assert!(!request(
        &mut f.model,
        Effect::Read(Read::Skills, After::Skills)
    ));
    assert!(request(&mut f.model, Effect::Cancel));
    assert!(request(&mut f.model, Effect::Abort));
    assert_eq!(f.model.remote.as_ref().unwrap().pending.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_attachment_admission_cannot_turn_a_steer_into_a_new_turn() {
    let mut f = fixture().await;
    f.model.running = true;
    f.model.remote.as_mut().unwrap().turn = 7;
    let generation = f.model.images.begin().unwrap();
    send(&mut f.model, "still the same task".into());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    f.model.running = false;
    f.model.images.failed(generation);
    let held = f.model.backend_deferred.take().unwrap();
    send_held(&mut f.model, held);
    let Some(Pending {
        effect: Effect::Send(draft),
        ..
    }) = f.model.remote.as_mut().unwrap().pending.pop_front()
    else {
        panic!("message not staged");
    };
    assert!(matches!(
        draft.request.intent,
        Some(protocol::SendIntent::Steer { turn: 7 })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_form_receipts_release_the_slot_but_keep_the_form_and_draft() {
    let mut f = fixture().await;
    let question = protocol::QuestionPrompt {
        question_id: 11,
        questions: vec![protocol::Question {
            prompt: "Pick a database".into(),
            header: "DB".into(),
            multi_select: false,
            options: vec![protocol::QuestionOption {
                label: "SQLite".into(),
                description: String::new(),
                recommended: true,
            }],
        }],
    };
    super::question(&mut f.model, question).unwrap();
    submit_clarify(&mut f.model);
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    assert!(
        f.model
            .remote
            .as_ref()
            .unwrap()
            .questions_answering
            .contains(&11)
    );
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Ok(Answer::Accepted(protocol::Accepted { accepted: false })),
        },
    );
    assert!(f.model.clarify.is_some());
    assert!(
        !f.model
            .remote
            .as_ref()
            .unwrap()
            .questions_answering
            .contains(&11)
    );
    submit_clarify(&mut f.model);
    assert!(
        f.model
            .remote
            .as_ref()
            .unwrap()
            .questions_answering
            .contains(&11)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_recovered_at_the_same_barrier_keeps_its_review_position() {
    let mut f = fixture().await;
    let prompt = protocol::ApprovalPrompt {
        gate_id: 5,
        action: "shell".into(),
        detail: None,
        escalated: false,
        kind: protocol::ApprovalKind::Action,
        choices: vec![
            protocol::ApprovalDecision::Once,
            protocol::ApprovalDecision::Deny,
        ],
        folder: None,
        path: None,
    };
    approval(&mut f.model, prompt.clone());
    f.model.approval_sel = 1;
    f.model.approval_ready = true;
    f.model.approval_expanded = true;
    let mut snap = snapshot(&f.model, "history");
    snap.approvals.push(prompt);
    restore_snapshot(&mut f.model, snap);
    assert_eq!(f.model.approval_sel, 1);
    assert!(f.model.approval_ready);
    assert!(f.model.approval_expanded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_receipt_does_not_erase_a_new_large_paste_typed_while_waiting() {
    let mut f = fixture().await;
    send(&mut f.model, "first".into());
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    input::handle_paste(&mut f.model, "é".repeat(1001));
    let draft = f.model.input.clone();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Ok(Answer::Sent(protocol::MessageAccepted {
                accepted: true,
                steered: false,
                turn: 1,
            })),
        },
    );
    assert_eq!(f.model.input, draft);
    assert_eq!(
        input::expand_paste_tokens(&f.model.pastes, &f.model.input).unwrap(),
        "é".repeat(1001)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_generation_receipts_cannot_change_the_new_chat() {
    let mut f = fixture().await;
    request(&mut f.model, Effect::Cancel);
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    f.model.remote.as_mut().unwrap().generation = 2;
    f.model.cancelling = true;
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Ok(Answer::Cancelled(protocol::Cancelled { cancelled: false })),
        },
    );
    assert!(f.model.cancelling);
    assert_eq!(f.model.remote.as_ref().unwrap().control, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returned_text_never_changes_an_open_secret_form() {
    let mut f = fixture().await;
    f.model.input = "next task".into();
    f.model.model_setup = Some(ModelSetup::update_key(
        "model".into(),
        kernel::Protocol::OpenAiChat,
        "http://localhost/v1".into(),
    ));
    input::handle_paste(&mut f.model, "sk-kept-exactly".into());
    turn_event(
        &mut f.model,
        protocol::TurnEvent::Returned {
            contents: vec!["unsent message".into()],
        },
    );
    assert_eq!(f.model.edited().0, "sk-kept-exactly");
    assert_eq!(f.model.input, "next task\nunsent message");
    assert!(f.model.pastes.is_empty());
    assert!(f.model.history.is_empty());
    handle_model_setup_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    assert_eq!(f.model.edited().0, "next task\nunsent message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_form_before_its_save_finishes_cannot_activate_that_profile() {
    let mut f = fixture().await;
    f.model.model_setup = Some(ModelSetup::new());
    f.model.model_setup.as_mut().unwrap().step = ModelSetupStep::Saving;
    let effect = Effect::Change(
        protocol::ChangeResource::SaveModel(protocol::ModelDraft {
            provider_id: None,
            auth: None,
            headers: BTreeMap::new(),
            name: None,
            protocol: protocol::ModelProtocol::OpenAiChat,
            base_url: "http://localhost/v1".into(),
            model: "m".into(),
            context_limit: None,
            key: None,
        }),
        After::Models,
    );
    assert!(request(&mut f.model, effect));
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    handle_model_setup_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    let catalogue = f.model.remote.as_ref().unwrap().models.clone();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Ok(Answer::Resource(Box::new(Resource::ModelSaved {
                name: "saved".into(),
                catalogue,
            }))),
        },
    );
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    assert!(f.model.model_setup.is_none());
    assert_eq!(f.model.remote.as_ref().unwrap().normal, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_activation_leaves_the_saved_profile_without_resaving_it() {
    let mut f = fixture().await;
    f.model.model_setup = Some(ModelSetup::new());
    f.model.model_setup.as_mut().unwrap().step = ModelSetupStep::Activating;
    request(
        &mut f.model,
        Effect::SavedProfile(protocol::ActivateSavedProfile {
            profile: "saved".into(),
        }),
    );
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Err("provider unavailable".into()),
        },
    );
    assert!(f.model.model_setup.is_none());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_turns_keep_their_assistant_answers_separate() {
    let mut f = fixture().await;
    turn_event(&mut f.model, protocol::TurnEvent::Started { turn: 1 });
    turn_event(
        &mut f.model,
        protocol::TurnEvent::Text {
            delta: "first".into(),
        },
    );
    turn_event(&mut f.model, protocol::TurnEvent::Done { stopped: None });
    turn_event(&mut f.model, protocol::TurnEvent::Started { turn: 2 });
    turn_event(
        &mut f.model,
        protocol::TurnEvent::Text {
            delta: "second".into(),
        },
    );
    let answers: Vec<_> = f
        .model
        .items
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["first", "second"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_slash_request_restores_the_original_command_beside_new_text() {
    let mut f = fixture().await;
    f.model.input = "/skill info missing".into();
    submit(&mut f.model);
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    f.model.input = "new draft".into();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Err("not found".into()),
        },
    );
    assert_eq!(f.model.input, "new draft\n/skill info missing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_returned_messages_are_compact_in_the_editor_and_can_be_sent_intact() {
    let mut f = fixture().await;
    let raw = "returned é\n".repeat(1000) + "last line";
    restore_draft(&mut f.model, raw.clone());
    assert!(f.model.input.len() < 100);
    assert_eq!(f.model.resolve_pastes(&f.model.input).unwrap(), raw);
    restore_draft(&mut f.model, raw.clone());
    assert_eq!(f.model.pastes.len(), 1);
    let draft = f.model.input.clone();
    send(&mut f.model, draft);
    let Pending {
        effect: Effect::Send(draft),
        ..
    } = f.model.remote.as_ref().unwrap().pending.front().unwrap()
    else {
        panic!("no send");
    };
    assert_eq!(draft.request.content, raw);
}
