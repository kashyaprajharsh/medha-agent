//! The connect screens through real key presses and real backend answers, and the card as drawn.

use super::*;

fn provider(
    id: &str,
    name: &str,
    url: &str,
    auth: &str,
    available: bool,
) -> protocol::ModelProvider {
    serde_json::from_value(json!({
        "id": id, "name": name, "url": url, "parameters": [], "protocol": "open-ai-chat", "auth": auth,
        "available": available, "unavailable_reason": (!available).then_some("This provider needs a connection Medha does not speak yet."),
        "doc": null, "env": [], "models": [{"id": "m", "context_length": 8000}]
    }))
    .unwrap()
}

fn catalogue(f: &mut Fixture) {
    let peer = f.model.remote.as_mut().unwrap();
    peer.models.providers = vec![
        provider(
            "openai",
            "OpenAI",
            "https://api.openai.com/v1",
            "bearer",
            true,
        ),
        provider(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            "x-api-key",
            true,
        ),
        provider(
            "google",
            "Google Gemini",
            "https://generativelanguage.googleapis.com/v1",
            "x-goog-api-key",
            true,
        ),
        provider(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "bearer",
            true,
        ),
        provider(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com",
            "bearer",
            true,
        ),
        provider(
            "ollama",
            "Ollama",
            "http://localhost:11434/v1",
            "none",
            true,
        ),
        provider(
            "lmstudio",
            "LM Studio",
            "http://localhost:1234/v1",
            "none",
            true,
        ),
        provider(
            "future",
            "Native Future",
            "https://future.test/v1",
            "bearer",
            false,
        ),
    ];
    peer.models.profiles = vec![serde_json::from_value(json!({
        "name": "old", "model": "gpt-old", "protocol": "open-ai-chat", "base_url": "https://api.openai.com/v1",
        "context_limit": null, "default": true, "key_present": true, "requires_key": true
    }))
    .unwrap()];
}

fn press(model: &mut Model, code: KeyCode) {
    input::handle_key(model, KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_text(model: &mut Model, text: &str) {
    for c in text.chars() {
        press(model, KeyCode::Char(c));
    }
}

/// The card as a terminal of this size shows it, one string per row.
fn drawn(model: &Model, width: u16, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| crate::tui_tea::connect_view::draw(f, model, Rect::new(0, height - 2, width, 2)))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn pending(f: &Fixture) -> Vec<&Effect> {
    f.model
        .remote
        .as_ref()
        .unwrap()
        .pending
        .iter()
        .map(|p| &p.effect)
        .collect()
}

fn answer(f: &mut Fixture, answer: Answer) {
    finish_setup_request(f, Ok(answer));
}

/// Answer the local-server probes the list sends on opening: nothing is running.
fn no_local_servers(f: &mut Fixture) {
    while matches!(pending(f).first(), Some(Effect::Probe { .. })) {
        let Some(Effect::Probe { url, .. }) = pending(f).first().copied() else {
            unreachable!()
        };
        let url = url.clone();
        answer(f, Answer::Probed { url, count: None });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_offers_plans_keys_and_this_computer_and_looks_for_local_servers() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    let probes = |f: &Fixture| -> Vec<String> {
        pending(f)
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Probe { url, .. } => Some(url.clone()),
                _ => None,
            })
            .collect()
    };
    // One server at a time, so a request slot is always free for the person.
    assert_eq!(probes(&f), ["http://localhost:11434/v1"]);
    answer(
        &mut f,
        Answer::Probed {
            url: "http://localhost:11434/v1".into(),
            count: Some(4),
        },
    );
    assert_eq!(probes(&f), ["http://localhost:1234/v1"]);
    answer(
        &mut f,
        Answer::Probed {
            url: "http://localhost:1234/v1".into(),
            count: None,
        },
    );

    let card = drawn(&f.model, 110, 32);
    println!("{}", card.join("\n"));
    let text = card.join("\n");
    for expected in [
        "connect a model",
        "/ search | ↑↓ move | Enter choose | Ctrl-R refresh | Esc close",
        "search or paste a key ›",
        "Plans",
        "> ChatGPT",
        "Enter: continue with ChatGPT",
        "API keys",
        "● key saved",
        "… 1 more",
        "This computer",
        "● running, 4 models",
        "not running",
        "Another server",
        "1 provider needs a connection Medha doesn't have yet. Press ? to see it.",
    ] {
        assert!(text.contains(expected), "missing {expected:?}\n{text}");
    }
    // Moving skips group titles and the "more" line.
    press(&mut f.model, KeyCode::Down);
    assert!(
        drawn(&f.model, 110, 32)
            .iter()
            .any(|line| line.contains("> OpenAI"))
    );
    press(&mut f.model, KeyCode::Char('?'));
    assert!(
        drawn(&f.model, 110, 32)
            .join("\n")
            .contains("Native Future")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pasted_key_with_several_owners_waits_for_a_choice_then_is_checked_where_chosen() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    no_local_servers(&mut f);
    input::handle_paste(&mut f.model, "sk-3a91abcdef0123456789".into());
    let asked: Vec<_> = pending(&f)
        .into_iter()
        .filter(|effect| matches!(effect, Effect::Recognise(_)))
        .collect();
    assert_eq!(asked.len(), 1, "a pasted key is read once, at once");
    let owner = |id: &str, name: &str| protocol::KeyOwner {
        id: id.into(),
        name: name.into(),
        available: true,
        reason: None,
    };
    let found = protocol::RecognisedKey {
        matched: protocol::KeyMatch::Several,
        providers: vec![owner("openai", "OpenAI"), owner("deepseek", "DeepSeek")],
        warning: None,
    };
    answer(
        &mut f,
        Answer::Recognised {
            key: "sk-3a91abcdef0123456789".into(),
            found,
        },
    );
    let text = drawn(&f.model, 110, 24).join("\n");
    assert!(text.contains("Which provider is this key for?"), "{text}");
    assert!(
        text.contains("Several providers use keys that start with sk-."),
        "{text}"
    );
    assert!(
        text.contains("sk-3"),
        "the key shows only its opening letters\n{text}"
    );
    assert!(
        !text.contains("abcdef0123"),
        "the middle of the key is never drawn\n{text}"
    );
    assert!(
        pending(&f).is_empty(),
        "nothing is sent until the person chooses"
    );

    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Enter);
    let checked = pending(&f).into_iter().find_map(|effect| match effect {
        Effect::CheckKey(check) => Some(check.clone()),
        _ => None,
    });
    let checked = checked.expect("the key is checked for the chosen provider");
    assert_eq!(checked.base_url, "https://api.deepseek.com");
    assert_eq!(checked.key.0, "sk-3a91abcdef0123456789");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_key_is_read_once_typing_pauses_never_per_keystroke() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    no_local_servers(&mut f);
    type_text(&mut f.model, "sk-3a91abcdef0123456789");
    connect::tick(&mut f.model);
    assert!(
        pending(&f).is_empty(),
        "nothing is asked while the person is still typing"
    );
    std::thread::sleep(std::time::Duration::from_millis(350));
    connect::tick(&mut f.model);
    connect::tick(&mut f.model);
    let asked: Vec<String> = pending(&f)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Recognise(request) => Some(request.key.0.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(asked, ["sk-3a91abcdef0123456789"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn found_models_are_listed_with_prices_and_enter_saves_the_highlighted_one() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    no_local_servers(&mut f);
    // OpenAI already holds a saved key here, so its models are looked for at once.
    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Enter);
    assert!(
        matches!(pending(&f)[..], [Effect::Discover(_)]),
        "{:?}",
        pending(&f).len()
    );
    let model =
        |id: &str, name: &str, context: u32, input: f64, output: f64| protocol::DiscoveredModel {
            id: id.into(),
            name: Some(name.into()),
            context_length: Some(context),
            input_per_mtok: Some(input),
            output_per_mtok: Some(output),
            tools: Some(true),
        };
    answer(
        &mut f,
        Answer::Discovered(protocol::ModelListing {
            models: vec![
                model("gpt-6.1-sol", "GPT-6.1 Sol", 1_050_000, 2.0, 10.0),
                model("gpt-6-luna", "GPT-6 Luna", 1_050_000, 0.1, 0.5),
                model("gpt-5.3-codex", "GPT-5.3 Codex", 400_000, 1.75, 14.0),
            ],
            unverified: None,
        }),
    );
    let card = drawn(&f.model, 110, 24);
    println!("{}", card.join("\n"));
    let text = card.join("\n");
    for expected in [
        "OpenAI",
        "api.openai.com/v1",
        "● saved",
        "Tab replaces it",
        "> GPT-6.1 Sol",
        "1.05M",
        "$2 / $10",
        "$0.10 / $0.50",
        "400K",
        "Enter adds GPT-6.1 Sol",
    ] {
        assert!(text.contains(expected), "missing {expected:?}\n{text}");
    }
    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Enter);
    let saved = pending(&f).into_iter().find_map(|effect| match effect {
        Effect::Change(protocol::ChangeResource::SaveModel(draft), _) => Some(draft.clone()),
        _ => None,
    });
    let saved = saved.expect("Enter saves the highlighted model");
    assert_eq!(saved.model, "gpt-6-luna");
    assert_eq!(saved.context_limit, Some(1_050_000));
    assert_eq!(saved.provider_id.as_deref(), Some("openai"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn esc_on_a_provider_returns_to_the_list_and_esc_on_the_list_closes() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Enter);
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().screen,
        Some(connect::Screen::Provider(_))
    ));
    press(&mut f.model, KeyCode::Esc);
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().screen,
        Some(connect::Screen::List(_))
    ));
    press(&mut f.model, KeyCode::Esc);
    assert!(f.model.model_setup.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_accounts_sign_out_through_the_backend_and_a_signed_out_one_signs_in_again() {
    let mut f = fixture().await;
    catalogue(&mut f);
    type_text(&mut f.model, "/chatgpt");
    press(&mut f.model, KeyCode::Enter);
    assert!(matches!(
        pending(&f)[..],
        [Effect::ChatGpt(ChatGptCall::Accounts)]
    ));
    let account = |id: &str, email: &str, active: bool, signed_in: bool| protocol::ChatGptAccount {
        client_id: id.into(),
        email: Some(email.into()),
        active,
        signed_in,
        plan_enabled: signed_in,
    };
    answer(
        &mut f,
        Answer::ChatGpt(ChatGptAnswer::Accounts(vec![
            account("oaiapp_1", "riya@example.com", true, true),
            account("oaiapp_2", "riya@work.example", false, false),
        ])),
    );
    let card = drawn(&f.model, 110, 14);
    println!("{}", card.join("\n"));
    let text = card.join("\n");
    for expected in [
        "ChatGPT accounts",
        "riya@example.com",
        "● Active · Plan use on",
        "Signed out",
        "o: sign out",
    ] {
        assert!(text.contains(expected), "missing {expected:?}\n{text}");
    }
    press(&mut f.model, KeyCode::Char('o'));
    assert!(
        matches!(pending(&f)[..], [Effect::ChatGpt(ChatGptCall::SignOut(p))] if p.client_id == "oaiapp_1")
    );
    answer(
        &mut f,
        Answer::ChatGpt(ChatGptAnswer::SignedOut { revoked: false }),
    );
    assert!(
        matches!(pending(&f)[..], [Effect::ChatGpt(ChatGptCall::Accounts)]),
        "the list is read again"
    );
    answer(
        &mut f,
        Answer::ChatGpt(ChatGptAnswer::Accounts(vec![
            account("oaiapp_1", "riya@example.com", false, false),
            account("oaiapp_2", "riya@work.example", false, false),
        ])),
    );
    press(&mut f.model, KeyCode::Down);
    press(&mut f.model, KeyCode::Enter);
    let begin = pending(&f).into_iter().find_map(|effect| match effect {
        Effect::ChatGpt(ChatGptCall::Begin(p)) => Some(p.client_id.clone()),
        _ => None,
    });
    assert_eq!(
        begin,
        Some(Some("oaiapp_2".into())),
        "signing in again keeps that account's registration"
    );
}

#[test]
fn a_plan_refusal_is_read_from_the_turn_error_and_nothing_else_is() {
    let refused = "OpenAI refused: subscription_sharing_usage_limit_exceeded (try later)";
    assert_eq!(
        connect::plan_refusal(refused).as_deref(),
        Some("subscription_sharing_usage_limit_exceeded")
    );
    assert_eq!(
        connect::plan_refusal("status 429: rate limited").as_deref(),
        None
    );
    assert_eq!(
        connect::plan_refusal("subscription_sharing_").as_deref(),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plan_limit_pauses_the_chat_u_opens_manage_usage_and_f_switches_to_the_stand_in() {
    let mut f = fixture().await;
    catalogue(&mut f);
    let peer = f.model.remote.as_mut().unwrap();
    peer.models.fallback = vec!["old".into()];
    f.model
        .history
        .push("Now write the migration for the new column.".into());
    turn_event(
        &mut f.model,
        protocol::TurnEvent::Error {
            message: "subscription_sharing_usage_limit_exceeded".into(),
        },
    );
    assert_eq!(
        f.model.plan_limit.as_deref(),
        Some("subscription_sharing_usage_limit_exceeded")
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 6)).unwrap();
    terminal
        .draw(|frame| {
            crate::tui_tea::connect_view::draw_plan_limit(frame, &f.model, Rect::new(0, 4, 110, 2))
        })
        .unwrap();
    let shown: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    println!("{shown}");
    for expected in [
        "Your ChatGPT plan's limit for Medha is reached.",
        "u: Manage usage",
        "f: switch this chat to old",
    ] {
        assert!(shown.contains(expected), "missing {expected:?}");
    }

    // Typing goes to the composer: a message starting with f is not a key press.
    f.model.input = "fix".into();
    press(&mut f.model, KeyCode::Char('u'));
    assert!(f.model.pending_link.is_none());
    f.model.input.clear();
    f.model.cursor = 0;

    press(&mut f.model, KeyCode::Char('u'));
    assert_eq!(
        f.model.pending_link.take().as_deref(),
        Some(connect::MANAGE_USAGE)
    );
    press(&mut f.model, KeyCode::Char('f'));
    assert!(matches!(pending(&f)[..], [Effect::SavedProfile(p)] if p.profile == "old"));
    assert!(f.model.plan_limit.is_none());
    assert_eq!(
        f.model.input, "Now write the migration for the new column.",
        "the message is ready to send again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_signs_in_through_the_backend_takes_one_more_pass_and_saves_a_plan_model() {
    let mut f = fixture().await;
    catalogue(&mut f);
    backend_features::begin_model_setup(&mut f.model);
    no_local_servers(&mut f);
    press(&mut f.model, KeyCode::Enter);
    assert!(matches!(
        pending(&f)[..],
        [Effect::ChatGpt(ChatGptCall::Begin(_))]
    ));
    let link = |id: u64| protocol::ChatGptSignIn {
        link: format!("https://auth.test/authorize?attempt={id}"),
        id,
    };
    answer(&mut f, Answer::ChatGpt(ChatGptAnswer::Begun(link(1))));
    assert!(matches!(pending(&f)[..], [Effect::ChatGpt(ChatGptCall::Finish(p))] if p.id == 1));
    let card = drawn(&f.model, 110, 16);
    println!("{}", card.join("\n"));
    let text = card.join("\n");
    assert!(
        text.contains("Continue in your browser.") && text.contains("attempt=1"),
        "{text}"
    );
    // The link is handed to the terminal loop to open; the screen never opens a browser itself.
    assert_eq!(
        f.model.pending_link.take().as_deref(),
        Some("https://auth.test/authorize?attempt=1")
    );
    press(&mut f.model, KeyCode::Char('c'));
    assert_eq!(
        f.model.pending_clipboard.as_deref(),
        Some("https://auth.test/authorize?attempt=1")
    );
    press(&mut f.model, KeyCode::Char('o'));
    assert_eq!(
        f.model.pending_link.take().as_deref(),
        Some("https://auth.test/authorize?attempt=1")
    );

    // OpenAI asks for one more pass as the issued client.
    let again = protocol::ChatGptSignInOutcome {
        account: None,
        again: Some(link(2)),
    };
    answer(
        &mut f,
        Answer::ChatGpt(ChatGptAnswer::Finished {
            id: 1,
            outcome: again,
        }),
    );
    assert!(
        drawn(&f.model, 110, 16)
            .join("\n")
            .contains("One more step in your browser.")
    );
    assert!(matches!(pending(&f)[..], [Effect::ChatGpt(ChatGptCall::Finish(p))] if p.id == 2));

    let account = protocol::ChatGptAccount {
        client_id: "oaiapp_1".into(),
        email: Some("riya@example.com".into()),
        active: true,
        signed_in: true,
        plan_enabled: true,
    };
    let done = protocol::ChatGptSignInOutcome {
        account: Some(account),
        again: None,
    };
    // A late answer for the replaced attempt changes nothing.
    connect::chatgpt(
        &mut f.model,
        ChatGptAnswer::Finished {
            id: 1,
            outcome: done.clone(),
        },
    );
    assert!(matches!(
        f.model.model_setup.as_ref().unwrap().screen,
        Some(connect::Screen::Waiting { id: 2, .. })
    ));
    answer(
        &mut f,
        Answer::ChatGpt(ChatGptAnswer::Finished {
            id: 2,
            outcome: done,
        }),
    );
    let discovery = pending(&f).into_iter().find_map(|effect| match effect {
        Effect::Discover(request) => Some(request.clone()),
        _ => None,
    });
    let discovery = discovery.expect("the plan's models are asked for");
    assert_eq!(discovery.auth, Some(protocol::ModelAuth::ChatGptPlan));
    assert!(discovery.key.0.is_empty(), "a plan model sends no key");
    answer(
        &mut f,
        Answer::Discovered(protocol::ModelListing {
            models: vec![protocol::DiscoveredModel {
                id: "gpt-6.1-sol".into(),
                name: Some("GPT-6.1 Sol".into()),
                ..Default::default()
            }],
            unverified: None,
        }),
    );
    let card = drawn(&f.model, 110, 16);
    println!("{}", card.join("\n"));
    let text = card.join("\n");
    for expected in [
        "ChatGPT",
        "riya@example.com, plan use on",
        "from your plan, in OpenAI's order",
        "> GPT-6.1 Sol",
        "included in your plan",
        "You're using your ChatGPT plan.",
    ] {
        assert!(text.contains(expected), "missing {expected:?}\n{text}");
    }
    press(&mut f.model, KeyCode::Enter);
    let saved = pending(&f).into_iter().find_map(|effect| match effect {
        Effect::Change(protocol::ChangeResource::SaveModel(draft), _) => Some(draft.clone()),
        _ => None,
    });
    let saved = saved.expect("Enter saves the plan model");
    assert_eq!(saved.auth, Some(protocol::ModelAuth::ChatGptPlan));
    assert_eq!(saved.protocol, protocol::ModelProtocol::OpenAiResponses);
    assert_eq!(saved.base_url, "https://api.openai.com/v1");
    assert!(saved.key.is_none());
}
