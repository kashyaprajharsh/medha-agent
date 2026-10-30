//! `/messaging`: reach Medha from a phone. The same backend calls the desktop's Messaging page
//! makes; the token never comes back to the terminal, and a pasted one is never shown.

use super::*;
use crate::tui_tea::backend_ui::{Effect, MessagingAnswer, MessagingCall, request};

pub(super) struct Panel {
    pub(super) overview: Option<protocol::MessagingOverview>,
    pub(super) screen: Screen,
    /// Why the last step failed, said on the screen it failed on.
    pub(super) problem: Option<String>,
    /// A call that changes something is on its way; keys wait for it.
    pub(super) busy: bool,
}

pub(super) enum Screen {
    Overview {
        selected: usize,
    },
    /// Scan a code, or paste a bot made by hand.
    Choose {
        platform: String,
        selected: usize,
    },
    Scan(Scan),
    Token {
        platform: String,
        typed: String,
    },
    Code {
        platform: String,
        typed: String,
    },
    /// A folder typed in to open to messages.
    Folder {
        typed: String,
    },
    Revoke(protocol::MessagingPerson),
    Done(protocol::MessagingSaved),
}

pub(super) struct Scan {
    pub(super) setup: protocol::MessagingSetup,
    pub(super) step: protocol::MessagingStep,
    /// When the person's time to scan runs out, as the backend last said.
    pub(super) ends: Instant,
    awaiting: bool,
    /// The seconds left as last drawn, so the clock redraws only when it changes.
    drawn: u64,
}

/// The overview's selection before it is read: the first platform, once there is a list.
pub(super) const FIRST: usize = usize::MAX;

/// One line of the overview that Enter acts on.
#[derive(Clone)]
pub(super) enum Row {
    Gateway,
    /// Whether chats show each tool step.
    Steps,
    Platform(protocol::MessagingPlatform),
    Person(protocol::MessagingPerson),
    Waiting(protocol::MessagingRequest),
    /// A folder chats may work in; the first is where new chats start.
    Folder {
        path: std::path::PathBuf,
        default: bool,
    },
    AddFolder,
}

impl Row {
    /// The same platform, person or folder, whatever else about them changed.
    fn same(&self, other: &Row) -> bool {
        match (self, other) {
            (Row::Gateway, Row::Gateway)
            | (Row::Steps, Row::Steps)
            | (Row::AddFolder, Row::AddFolder) => true,
            (Row::Folder { path: a, .. }, Row::Folder { path: b, .. }) => a == b,
            (Row::Platform(a), Row::Platform(b)) => a.name == b.name,
            (Row::Person(a), Row::Person(b)) => a.platform == b.platform && a.user == b.user,
            (Row::Waiting(a), Row::Waiting(b)) => a.platform == b.platform && a.user == b.user,
            _ => false,
        }
    }
}

/// The gateway and folder rows show once there is something for them to act on.
pub(super) fn rows(overview: &protocol::MessagingOverview) -> Vec<Row> {
    let mut rows = Vec::new();
    let set_up = overview.platforms.iter().any(|platform| platform.ready) || overview.running;
    if set_up {
        rows.push(Row::Gateway);
        rows.push(Row::Steps);
    }
    rows.extend(overview.platforms.iter().cloned().map(Row::Platform));
    if set_up {
        rows.extend(overview.folders.iter().map(|path| Row::Folder {
            default: overview.folder.as_ref() == Some(path),
            path: path.clone(),
        }));
        rows.push(Row::AddFolder);
    }
    rows.extend(overview.waiting.iter().cloned().map(Row::Waiting));
    rows.extend(overview.people.iter().cloned().map(Row::Person));
    rows
}

pub(super) fn open(model: &mut Model) {
    model.picker = None;
    model.messaging = Some(Panel {
        overview: None,
        screen: Screen::Overview { selected: FIRST },
        problem: None,
        busy: false,
    });
    call(model, MessagingCall::Status);
}

fn call(model: &mut Model, call: MessagingCall) -> bool {
    let changes = !matches!(call, MessagingCall::Status | MessagingCall::Await(_));
    let sent = request(model, Effect::Messaging(call));
    if let Some(panel) = &mut model.messaging
        && sent
        && changes
    {
        panel.busy = true;
    }
    sent
}

fn screen_to(model: &mut Model, screen: Screen) {
    if let Some(panel) = &mut model.messaging {
        panel.screen = screen;
        panel.problem = None;
    }
}

/// Leaves a setup in progress first, so its token is dropped at once rather than later.
fn back(model: &mut Model) {
    if let Some(Panel {
        screen: Screen::Scan(scan),
        ..
    }) = &model.messaging
    {
        let id = scan.setup.id;
        call(
            model,
            MessagingCall::Cancel(protocol::CancelMessagingSetup { id }),
        );
    }
    screen_to(model, Screen::Overview { selected: FIRST });
    call(model, MessagingCall::Status);
}

pub(super) fn handle_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(panel) = &model.messaging else {
        return false;
    };
    if panel.busy {
        return true;
    }
    match &panel.screen {
        Screen::Overview { selected } => overview_key(model, key, *selected),
        Screen::Choose { platform, selected } => {
            let (platform, selected) = (platform.clone(), *selected);
            choose_key(model, key, platform, selected)
        }
        Screen::Scan(_) => scan_key(model, key),
        Screen::Token { .. } | Screen::Code { .. } | Screen::Folder { .. } => {
            typing_key(model, key)
        }
        Screen::Revoke(person) => {
            let person = person.clone();
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    call(
                        model,
                        MessagingCall::Revoke(protocol::RevokeMessagingPerson {
                            platform: person.platform,
                            user: person.user,
                        }),
                    );
                }
                KeyCode::Char('n') | KeyCode::Esc => back(model),
                _ => {}
            }
            true
        }
        Screen::Done(_) => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                back(model);
            }
            true
        }
    }
}

fn overview_key(model: &mut Model, key: KeyEvent, selected: usize) -> bool {
    let rows = model
        .messaging
        .as_ref()
        .and_then(|panel| panel.overview.as_ref())
        .map(rows)
        .unwrap_or_default();
    let last = rows.len().saturating_sub(1);
    let selected = if selected == FIRST {
        rows.iter()
            .position(|row| matches!(row, Row::Platform(_)))
            .unwrap_or(0)
    } else {
        selected
    };
    let move_to = |model: &mut Model, selected: usize| {
        if let Some(panel) = &mut model.messaging {
            panel.screen = Screen::Overview { selected };
        }
    };
    match key.code {
        KeyCode::Esc => model.messaging = None,
        KeyCode::Up => move_to(model, selected.saturating_sub(1)),
        KeyCode::Down => move_to(model, (selected + 1).min(last)),
        KeyCode::Char('r') => match rows.get(selected) {
            Some(Row::Person(person)) => screen_to(model, Screen::Revoke(person.clone())),
            // Taking a folder off the list is undone by adding it again, so it asks nothing.
            Some(Row::Folder { path, .. }) => {
                let folder = path.clone();
                call(
                    model,
                    MessagingCall::Folder(protocol::ChangeMessagingFolder {
                        change: protocol::FolderChange::Remove,
                        folder,
                    }),
                );
            }
            _ => {}
        },
        KeyCode::Enter => match rows.get(selected) {
            Some(Row::Gateway) => {
                let running = model
                    .messaging
                    .as_ref()
                    .and_then(|panel| panel.overview.as_ref())
                    .is_some_and(|overview| overview.running);
                let action = if running {
                    protocol::GatewayAction::Stop
                } else {
                    protocol::GatewayAction::Start
                };
                call(
                    model,
                    MessagingCall::Gateway(protocol::ControlGateway { action }),
                );
            }
            Some(Row::Steps) => {
                let shown = model
                    .messaging
                    .as_ref()
                    .and_then(|panel| panel.overview.as_ref())
                    .is_some_and(|overview| overview.steps);
                call(
                    model,
                    MessagingCall::Steps(protocol::ShowMessagingSteps { show: !shown }),
                );
            }
            Some(Row::Platform(platform)) => screen_to(
                model,
                if platform.one_tap {
                    Screen::Choose {
                        platform: platform.name.clone(),
                        selected: 0,
                    }
                } else {
                    Screen::Token {
                        platform: platform.name.clone(),
                        typed: String::new(),
                    }
                },
            ),
            Some(Row::Waiting(request)) => screen_to(
                model,
                Screen::Code {
                    platform: request.platform.clone(),
                    typed: String::new(),
                },
            ),
            Some(Row::Person(person)) => screen_to(model, Screen::Revoke(person.clone())),
            Some(Row::Folder { path, default }) => {
                if !default {
                    let folder = path.clone();
                    call(
                        model,
                        MessagingCall::Folder(protocol::ChangeMessagingFolder {
                            change: protocol::FolderChange::Default,
                            folder,
                        }),
                    );
                }
            }
            Some(Row::AddFolder) => screen_to(
                model,
                Screen::Folder {
                    typed: std::env::current_dir()
                        .map(|here| here.display().to_string())
                        .unwrap_or_default(),
                },
            ),
            None => {}
        },
        _ => {}
    }
    true
}

fn choose_key(model: &mut Model, key: KeyEvent, platform: String, selected: usize) -> bool {
    let pick = match key.code {
        KeyCode::Up | KeyCode::Down => {
            if let Some(panel) = &mut model.messaging {
                panel.screen = Screen::Choose {
                    platform,
                    selected: 1 - selected.min(1),
                };
            }
            return true;
        }
        KeyCode::Char('1') => 0,
        KeyCode::Char('2') => 1,
        KeyCode::Enter => selected,
        KeyCode::Esc => {
            back(model);
            return true;
        }
        _ => return true,
    };
    if pick == 0 {
        call(
            model,
            MessagingCall::Begin(protocol::BeginMessagingSetup { platform }),
        );
    } else {
        screen_to(
            model,
            Screen::Token {
                platform,
                typed: String::new(),
            },
        );
    }
    true
}

fn scan_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Panel {
        screen: Screen::Scan(scan),
        ..
    }) = &model.messaging
    else {
        return true;
    };
    let (link, id, heard) = (
        scan.setup.link.clone(),
        scan.setup.id,
        matches!(scan.step, protocol::MessagingStep::Heard { .. }),
    );
    match key.code {
        KeyCode::Char('c') => model.pending_clipboard = Some(link),
        KeyCode::Char('o') => model.pending_link = Some(link),
        KeyCode::Enter | KeyCode::Char('y') if heard => {
            call(
                model,
                MessagingCall::Save(protocol::SaveMessagingSetup { id, folder: None }),
            );
        }
        KeyCode::Char('n') if heard => {
            let bot = match &model.messaging {
                Some(Panel {
                    screen:
                        Screen::Scan(Scan {
                            step: protocol::MessagingStep::Heard { bot, .. },
                            ..
                        }),
                    ..
                }) => bot.clone(),
                _ => String::new(),
            };
            back(model);
            if let Some(panel) = &mut model.messaging {
                panel.problem = Some(format!(
                    "Setup stopped. Delete @{bot} in @BotFather, then set up again."
                ));
            }
        }
        KeyCode::Esc => back(model),
        _ => {}
    }
    true
}

fn typing_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(panel) = &mut model.messaging else {
        return true;
    };
    let typed = match &mut panel.screen {
        Screen::Token { typed, .. } | Screen::Code { typed, .. } | Screen::Folder { typed } => {
            typed
        }
        _ => return true,
    };
    match key.code {
        KeyCode::Esc => back(model),
        KeyCode::Backspace => {
            typed.pop();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => typed.push(c),
        KeyCode::Enter if !typed.trim().is_empty() => {
            let typed = typed.trim().to_owned();
            let next = match &panel.screen {
                Screen::Token { platform, .. } => {
                    MessagingCall::SaveToken(protocol::SaveMessagingToken {
                        platform: platform.clone(),
                        token: protocol::Secret(typed),
                        folder: None,
                    })
                }
                Screen::Code { platform, .. } => {
                    MessagingCall::Approve(protocol::ApproveMessagingPerson {
                        platform: platform.clone(),
                        code: typed,
                    })
                }
                _ => MessagingCall::Folder(protocol::ChangeMessagingFolder {
                    change: protocol::FolderChange::Add,
                    folder: expanded(&typed),
                }),
            };
            call(model, next);
        }
        _ => {}
    }
    true
}

/// A typed folder, with `~` meaning the home folder as at a shell.
fn expanded(typed: &str) -> std::path::PathBuf {
    match (typed.strip_prefix("~"), dirs::home_dir()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => std::path::PathBuf::from(typed),
    }
}

/// A pasted token or code goes to the line being typed.
pub(super) fn paste(model: &mut Model, text: &str) -> bool {
    match model.messaging.as_mut().map(|panel| &mut panel.screen) {
        Some(
            Screen::Token { typed, .. } | Screen::Code { typed, .. } | Screen::Folder { typed },
        ) => {
            typed.push_str(text.trim());
            true
        }
        Some(_) => true,
        None => false,
    }
}

/// Keeps a scan asking the backend, a step at a time, until the owner's message is heard;
/// `true` when the clock on screen moved.
pub(super) fn tick(model: &mut Model) -> bool {
    let moved = match &mut model.messaging {
        Some(Panel {
            screen: Screen::Scan(scan),
            ..
        }) => {
            let left = scan
                .ends
                .saturating_duration_since(Instant::now())
                .as_secs();
            std::mem::replace(&mut scan.drawn, left) != left
        }
        _ => false,
    };
    ask(model);
    moved
}

fn ask(model: &mut Model) {
    let wants = matches!(
        &model.messaging,
        Some(Panel { screen: Screen::Scan(scan), busy: false, .. })
            if !scan.awaiting && !matches!(scan.step, protocol::MessagingStep::Heard { .. })
    );
    if !wants {
        return;
    }
    let Some(Panel {
        screen: Screen::Scan(scan),
        ..
    }) = &model.messaging
    else {
        return;
    };
    let id = scan.setup.id;
    if call(
        model,
        MessagingCall::Await(protocol::AwaitMessagingSetup { id }),
    ) && let Some(Panel {
        screen: Screen::Scan(scan),
        ..
    }) = &mut model.messaging
    {
        scan.awaiting = true;
    }
}

pub(super) fn answered(model: &mut Model, answer: MessagingAnswer) {
    let Some(panel) = &mut model.messaging else {
        return;
    };
    panel.busy = false;
    match answer {
        MessagingAnswer::Overview(overview) => {
            // A folder just added is shown in the list it joined.
            if matches!(panel.screen, Screen::Folder { .. }) {
                panel.screen = Screen::Overview { selected: FIRST };
                panel.problem = None;
            }
            let all = rows(&overview);
            let before = panel.overview.as_ref().map(rows).unwrap_or_default();
            if let Screen::Overview { selected } = &mut panel.screen {
                // The same line stays chosen when lines above it come or go.
                let kept = before
                    .get(*selected)
                    .and_then(|old| all.iter().position(|row| row.same(old)));
                *selected = match kept {
                    Some(at) if *selected != FIRST => at,
                    _ if *selected == FIRST => all
                        .iter()
                        .position(|row| matches!(row, Row::Platform(_)))
                        .unwrap_or(0),
                    _ => (*selected).min(all.len().saturating_sub(1)),
                };
            }
            panel.overview = Some(overview);
        }
        MessagingAnswer::Begun(setup) => {
            panel.problem = None;
            panel.screen = Screen::Scan(Scan {
                ends: Instant::now() + Duration::from_secs(setup.expires_in),
                step: protocol::MessagingStep::Waiting {
                    expires_in: setup.expires_in,
                },
                drawn: setup.expires_in,
                setup,
                awaiting: false,
            });
        }
        MessagingAnswer::Step { id, step } => {
            if let Screen::Scan(scan) = &mut panel.screen
                && scan.setup.id == id
            {
                if let protocol::MessagingStep::Waiting { expires_in } = &step {
                    scan.ends = Instant::now() + Duration::from_secs(*expires_in);
                }
                scan.step = step;
                scan.awaiting = false;
            }
        }
        MessagingAnswer::Saved(saved) => {
            panel.problem = None;
            panel.screen = Screen::Done(saved);
        }
        MessagingAnswer::Approved(person) => {
            panel.screen = Screen::Overview { selected: FIRST };
            panel.problem = None;
            model.push_notice(format!("{} can now message Medha.", person.name));
            call(model, MessagingCall::Status);
        }
        MessagingAnswer::Changed => {
            if matches!(panel.screen, Screen::Revoke(_)) {
                panel.screen = Screen::Overview { selected: FIRST };
            }
            call(model, MessagingCall::Status);
        }
    }
}

pub(super) fn failed(model: &mut Model, call: &MessagingCall, why: &str) {
    let Some(panel) = &mut model.messaging else {
        return;
    };
    panel.busy = false;
    match call {
        // The setup ended on the backend: back to the overview, saying why.
        MessagingCall::Await(_) => {
            panel.screen = Screen::Overview { selected: FIRST };
            panel.problem = Some(why.to_owned());
        }
        MessagingCall::Cancel(_) => {}
        _ => panel.problem = Some(why.to_owned()),
    }
}
