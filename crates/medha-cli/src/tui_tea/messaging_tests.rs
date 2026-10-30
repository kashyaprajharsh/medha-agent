//! `/messaging` through real key presses and real backend answers, and the card as drawn.

use super::*;
use crate::tui_tea::backend_ui::{MessagingAnswer, MessagingCall};
use crate::tui_tea::messaging::Screen;

const LINK: &str = "https://t.me/MedhaSetupBot?start=Zq9c2LwX4mTn8RbE1vYs6KaP3hJdF0Ug";

fn press(model: &mut Model, code: KeyCode) {
    input::handle_key(model, KeyEvent::new(code, KeyModifiers::NONE));
}

fn drawn(model: &Model, width: u16, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            crate::tui_tea::messaging_view::draw(f, model, Rect::new(0, height - 2, width, 2))
        })
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

fn calls(f: &Fixture) -> Vec<&MessagingCall> {
    f.model
        .remote
        .as_ref()
        .unwrap()
        .pending
        .iter()
        .filter_map(|pending| match &pending.effect {
            Effect::Messaging(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn reply(f: &mut Fixture, answer: MessagingAnswer) {
    finish_setup_request(f, Ok(Answer::Messaging(answer)));
}

fn overview() -> protocol::MessagingOverview {
    protocol::MessagingOverview {
        folder: Some("/work/medha".into()),
        folders: vec!["/work/medha".into()],
        steps: false,
        running: false,
        installed: false,
        platforms: vec![protocol::MessagingPlatform {
            name: "telegram".into(),
            label: "Telegram".into(),
            ready: false,
            account: None,
            home: false,
            steps: "1. In Telegram, open @BotFather and send /newbot.".into(),
            token: "Bot token".into(),
            one_tap: true,
            owner_only: false,
        }],
        people: vec![],
        waiting: vec![],
    }
}

fn setup(id: u64) -> protocol::MessagingSetup {
    protocol::MessagingSetup {
        id,
        link: LINK.into(),
        qr_text: gateway::one_tap::qr(LINK).unwrap(),
        qr_svg: String::new(),
        expires_in: 600,
    }
}

/// `/messaging` opened and its overview read; Telegram is the chosen row.
async fn opened() -> Fixture {
    let mut f = fixture().await;
    messaging::open(&mut f.model);
    assert!(matches!(calls(&f).as_slice(), [MessagingCall::Status]));
    reply(&mut f, MessagingAnswer::Overview(overview()));
    f
}

/// Telegram chosen, the code begun, the backend's answer shown.
async fn scanning() -> Fixture {
    let mut f = opened().await;
    press(&mut f.model, KeyCode::Enter);
    press(&mut f.model, KeyCode::Char('1'));
    assert!(
        matches!(calls(&f).as_slice(), [MessagingCall::Begin(begin)] if begin.platform == "telegram")
    );
    reply(&mut f, MessagingAnswer::Begun(setup(7)));
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pasted_token_is_never_drawn_and_reaches_the_backend_whole() {
    let mut f = opened().await;
    press(&mut f.model, KeyCode::Enter);
    press(&mut f.model, KeyCode::Char('2'));
    let token = "7043225511:AAHt3PZ-secret-part-of-the-token";
    input::handle_paste(&mut f.model, token.into());
    let rows = drawn(&f.model, 110, 30).join("\n");
    assert!(rows.contains("Bot token"), "{rows}");
    assert!(!rows.contains("secret-part-of-the-token"), "{rows}");
    press(&mut f.model, KeyCode::Enter);
    assert!(matches!(
        calls(&f).as_slice(),
        [MessagingCall::SaveToken(save)] if save.token.0 == token && save.platform == "telegram"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_code_is_drawn_whole_or_the_window_shows_the_link_alone() {
    let f = scanning().await;
    let code = gateway::one_tap::qr(LINK).unwrap();
    let tall = drawn(&f.model, 120, 60).join("\n");
    for line in code.lines() {
        assert!(
            tall.contains(line.trim_end()),
            "a row of the code is missing:\n{tall}"
        );
    }
    assert!(tall.contains("Tap Start, then Create bot."), "{tall}");

    let short = drawn(&f.model, 120, 14).join("\n");
    assert!(short.contains("Make the window taller"), "{short}");
    assert!(short.contains(LINK), "{short}");
    let first_row = code.lines().nth(1).unwrap().trim_end();
    assert!(!short.contains(first_row), "a cut code was drawn:\n{short}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scan_moves_on_only_for_its_own_setup_and_waits_for_yes() {
    let mut f = scanning().await;
    messaging::tick(&mut f.model);
    assert!(matches!(calls(&f).as_slice(), [MessagingCall::Await(wait)] if wait.id == 7));
    // An older setup's late answer changes nothing.
    let heard = protocol::MessagingStep::Heard {
        bot: "harsh_medha_bot".into(),
        name: "Harsh".into(),
        user: "1411410327".into(),
    };
    messaging::answered(
        &mut f.model,
        MessagingAnswer::Step {
            id: 6,
            step: heard.clone(),
        },
    );
    assert!(matches!(
        &f.model.messaging.as_ref().unwrap().screen,
        Screen::Scan(scan) if matches!(scan.step, protocol::MessagingStep::Waiting { .. })
    ));
    reply(&mut f, MessagingAnswer::Step { id: 7, step: heard });
    let rows = drawn(&f.model, 120, 60).join("\n");
    assert!(
        rows.contains("Is this you?") && rows.contains("Harsh"),
        "{rows}"
    );
    // Heard, nothing more is asked until the person answers.
    messaging::tick(&mut f.model);
    assert!(calls(&f).is_empty());
    press(&mut f.model, KeyCode::Enter);
    assert!(matches!(calls(&f).as_slice(), [MessagingCall::Save(save)] if save.id == 7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saying_no_ends_the_setup_and_says_what_to_do_with_the_bot() {
    let mut f = scanning().await;
    messaging::answered(
        &mut f.model,
        MessagingAnswer::Step {
            id: 7,
            step: protocol::MessagingStep::Heard {
                bot: "harsh_medha_bot".into(),
                name: "Someone".into(),
                user: "99".into(),
            },
        },
    );
    press(&mut f.model, KeyCode::Char('n'));
    let sent = calls(&f);
    assert!(
        sent.iter()
            .any(|call| matches!(call, MessagingCall::Cancel(cancel) if cancel.id == 7)),
        "the backend must drop the token at once"
    );
    assert!(
        !sent
            .iter()
            .any(|call| matches!(call, MessagingCall::Save(_)))
    );
    let panel = f.model.messaging.as_ref().unwrap();
    assert!(matches!(panel.screen, Screen::Overview { .. }));
    assert!(panel.problem.as_deref().unwrap().contains("@BotFather"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chosen_line_stays_chosen_when_the_gateway_line_appears_above_it() {
    let mut f = opened().await;
    let chosen = |f: &Fixture| {
        drawn(&f.model, 110, 22)
            .into_iter()
            .find(|row| row.contains("│ > "))
            .unwrap_or_default()
    };
    assert!(chosen(&f).contains("Telegram"), "{}", chosen(&f));
    // Nothing ready yet, so there is no gateway line to act on.
    assert!(!drawn(&f.model, 110, 22).join("\n").contains("Gateway"));
    let mut ready = overview();
    ready.running = true;
    ready.platforms[0].ready = true;
    ready.platforms[0].account = Some("@harsh_medha_bot".into());
    messaging::answered(&mut f.model, MessagingAnswer::Overview(ready));
    assert!(chosen(&f).contains("Telegram"), "{}", chosen(&f));
    assert!(drawn(&f.model, 110, 22).join("\n").contains("Gateway"));
}

#[tokio::test]
async fn tool_steps_are_off_until_chosen_and_the_line_shows_what_the_backend_saved() {
    let mut f = opened().await;
    let mut ready = overview();
    ready.platforms[0].ready = true;
    messaging::answered(&mut f.model, MessagingAnswer::Overview(ready.clone()));
    let steps_line = |f: &Fixture| {
        drawn(&f.model, 110, 24)
            .into_iter()
            .find(|row| row.contains("Tool steps"))
            .unwrap_or_default()
    };
    assert!(steps_line(&f).contains("○ off"), "{}", steps_line(&f));
    press(&mut f.model, KeyCode::Up);
    assert!(steps_line(&f).contains("│ > "), "{}", steps_line(&f));
    press(&mut f.model, KeyCode::Enter);
    assert!(
        matches!(
            calls(&f).last(),
            Some(MessagingCall::Steps(protocol::ShowMessagingSteps {
                show: true
            }))
        ),
        "Enter asks to show them"
    );
    // Drawn from the saved answer, not guessed before it.
    assert!(steps_line(&f).contains("○ off"));
    ready.steps = true;
    reply(&mut f, MessagingAnswer::Overview(ready));
    assert!(steps_line(&f).contains("● on"), "{}", steps_line(&f));
    assert!(steps_line(&f).contains("│ > "), "the line stays chosen");
}
