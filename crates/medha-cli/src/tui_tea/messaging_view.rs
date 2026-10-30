//! Drawing `/messaging` with the connect screens' card: one bordered card above the composer, its
//! title on the left and its keys on the right.

use super::connect::masked;
use super::connect_view::{Card, bold, draw_card, faint, gold, ok, plain, row, title, width_of};
use super::messaging::{FIRST, Panel, Row, Scan, Screen, rows};
use super::*;

/// The code is drawn light on dark in every theme, the way phone cameras read it.
fn code_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Black)
}

fn problem(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default().fg(theme::err()),
    ))
}

fn group(name: &str) -> Line<'static> {
    Line::from(vec![plain("  "), bold(name.to_owned())])
}

/// Where the folder is shown, with the home folder written as `~`.
fn shown(folder: &std::path::Path) -> String {
    match dirs::home_dir()
        .and_then(|home| folder.strip_prefix(home).ok().map(|rest| rest.to_owned()))
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => folder.display().to_string(),
    }
}

fn overview_card(panel: &Panel, selected: usize, width: usize) -> Card {
    let Some(overview) = &panel.overview else {
        return Card {
            title: title("Messaging".into(), "reach Medha from your phone"),
            keys: "Esc close".into(),
            lines: vec![
                Line::from(""),
                Line::from(faint("  Reading what is set up…")),
            ],
            focus: 0,
            caret: None,
        };
    };
    let all = rows(overview);
    let selected = if selected == FIRST {
        all.iter()
            .position(|row| matches!(row, Row::Platform(_)))
            .unwrap_or(0)
    } else {
        selected
    };
    let label = |name: &str| {
        overview
            .platforms
            .iter()
            .find(|platform| platform.name == name)
            .map_or_else(|| name.to_owned(), |platform| platform.label.clone())
    };
    let mut lines = vec![Line::from("")];
    let mut focus = 0;
    let mut placed = |lines: &mut Vec<Line<'static>>, index: usize, line: Line<'static>| {
        if index == selected {
            focus = lines.len();
        }
        lines.push(line);
    };
    for (index, item) in all.iter().enumerate() {
        let chosen = index == selected;
        match item {
            Row::Gateway => {
                let (detail, state) = match (overview.running, overview.installed) {
                    (true, true) => ("running, starts at login", ok("● on")),
                    (true, false) => ("running in a terminal", ok("● on")),
                    (false, true) => ("stopped until you start it or log in again", faint("○ off")),
                    (false, false) => ("stopped", faint("○ off")),
                };
                placed(
                    &mut lines,
                    index,
                    row(width, chosen, plain("Gateway"), faint(detail), state),
                );
            }
            Row::Steps => {
                let (detail, state) = if overview.steps {
                    ("each step shown in the chat as it runs", ok("● on"))
                } else {
                    ("the chat shows replies and approvals only", faint("○ off"))
                };
                placed(
                    &mut lines,
                    index,
                    row(width, chosen, plain("Tool steps"), faint(detail), state),
                );
                lines.push(Line::from(""));
            }
            Row::Platform(platform) => {
                let (detail, state) = if platform.ready {
                    (
                        platform.account.clone().unwrap_or_else(|| "set up".into()),
                        ok("● ready"),
                    )
                } else if platform.one_tap {
                    ("scan a code to make your own bot".into(), gold("Set up"))
                } else {
                    ("paste a bot token".into(), gold("Set up"))
                };
                placed(
                    &mut lines,
                    index,
                    row(
                        width,
                        chosen,
                        plain(platform.label.clone()),
                        faint(detail),
                        state,
                    ),
                );
                if platform.ready && platform.owner_only {
                    lines.push(Line::from(faint(
                        "    Only you can message this bot. To let others use Medha, set it up again with a bot you made in @BotFather.",
                    )));
                }
            }
            Row::Waiting(request) => {
                if !matches!(all.get(index.wrapping_sub(1)), Some(Row::Waiting(_))) {
                    lines.push(Line::from(""));
                    lines.push(group("Waiting for you to let them in"));
                }
                let asked = match request.minutes {
                    0 => "just now".to_owned(),
                    1 => "1 min ago".to_owned(),
                    n => format!("{n} min ago"),
                };
                placed(
                    &mut lines,
                    index,
                    row(
                        width,
                        chosen,
                        plain(request.name.clone()),
                        faint(format!("{asked}, Enter to type their code")),
                        faint(label(&request.platform)),
                    ),
                );
            }
            Row::Person(person) => {
                if !matches!(all.get(index.wrapping_sub(1)), Some(Row::Person(_))) {
                    lines.push(Line::from(""));
                    lines.push(group("Can use Medha"));
                }
                placed(
                    &mut lines,
                    index,
                    row(
                        width,
                        chosen,
                        plain(person.name.clone()),
                        faint(format!("{} {}", label(&person.platform), person.user)),
                        if chosen { faint("r remove") } else { plain("") },
                    ),
                );
            }
            Row::Folder { path, default } => {
                if !matches!(all.get(index.wrapping_sub(1)), Some(Row::Folder { .. })) {
                    lines.push(Line::from(""));
                    lines.push(group("Projects your phone can work in"));
                }
                let name = path
                    .file_name()
                    .map_or_else(|| shown(path), |name| name.to_string_lossy().into_owned());
                placed(
                    &mut lines,
                    index,
                    row(
                        width,
                        chosen,
                        plain(name),
                        faint(shown(path)),
                        if *default {
                            faint("new chats start here")
                        } else {
                            plain("")
                        },
                    ),
                );
            }
            Row::AddFolder => placed(
                &mut lines,
                index,
                row(
                    width,
                    chosen,
                    gold("+ Add a folder"),
                    faint("a project you want to reach from the phone"),
                    plain(""),
                ),
            ),
        }
    }
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    lines.push(Line::from(""));
    let keys = match all.get(selected) {
        Some(Row::Gateway) if overview.running => "↑↓ choose | Enter stop | Esc close",
        Some(Row::Gateway) => "↑↓ choose | Enter start | Esc close",
        Some(Row::Steps) if overview.steps => "↑↓ choose | Enter hide steps | Esc close",
        Some(Row::Steps) => "↑↓ choose | Enter show steps | Esc close",
        Some(Row::Person(_)) => "↑↓ choose | r remove | Esc close",
        Some(Row::Folder { default: true, .. }) => "↑↓ choose | Esc close",
        Some(Row::Folder { .. }) => "↑↓ choose | Enter start new chats here | r remove | Esc close",
        Some(Row::AddFolder) => "↑↓ choose | Enter add | Esc close",
        _ => "↑↓ choose | Enter open | Esc close",
    };
    Card {
        title: title("Messaging".into(), "reach Medha from your phone"),
        keys: keys.into(),
        lines,
        focus,
        caret: None,
    }
}

fn choose_card(panel: &Panel, label: &str, selected: usize, width: usize) -> Card {
    let mut lines = vec![
        Line::from(""),
        row(
            width,
            selected == 0,
            plain("1  Scan a code"),
            faint("Medha Setup makes a bot of your own"),
            gold("recommended"),
        ),
        row(
            width,
            selected == 1,
            plain("2  Use your own bot"),
            faint("one you made in @BotFather"),
            plain(""),
        ),
        Line::from(""),
        Line::from(faint(
            "  Either way the bot is yours and only you can message it. With 1, Medha Setup can read",
        )),
        Line::from(faint(
            "  the new bot's token for as long as the bot exists. Choose 2 if no one else should.",
        )),
    ];
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    Card {
        title: title(label.to_owned(), "how to make the bot"),
        keys: "↑↓ choose | Enter go on | Esc back".into(),
        lines,
        focus: 0,
        caret: None,
    }
}

/// What the person does now, beside or under the code; without the code, the link comes first.
fn scan_steps(panel: &Panel, scan: &Scan, with_code: bool) -> Vec<Line<'static>> {
    let left = scan
        .ends
        .saturating_duration_since(Instant::now())
        .as_secs();
    let clock = format!("{}:{:02} left", left / 60, left % 60);
    let waiting = matches!(scan.step, protocol::MessagingStep::Waiting { .. });
    let mut lines = Vec::new();
    if !with_code && waiting {
        lines.push(Line::from(faint(
            "Make the window taller to show the code, or open this link on your phone:",
        )));
        lines.push(Line::from(plain(scan.setup.link.clone())));
        lines.push(Line::from(""));
    }
    lines.extend(match &scan.step {
        protocol::MessagingStep::Waiting { .. } => vec![
            Line::from(vec![
                gold("● "),
                bold(if with_code {
                    "Scan this with your phone."
                } else {
                    "Telegram opens Medha Setup."
                }),
                faint(format!("  {clock}")),
            ]),
            Line::from(""),
            Line::from(plain(if with_code {
                "Telegram opens Medha Setup. Tap Start, then Create bot."
            } else {
                "Tap Start, then Create bot."
            })),
            Line::from(plain("Keep the name it suggests or pick your own.")),
        ],
        protocol::MessagingStep::Created { bot } => vec![
            Line::from(vec![ok("✓ "), bold(format!("@{bot} is yours."))]),
            Line::from(""),
            Line::from(plain("Send it any message from your phone,")),
            Line::from(plain("so Medha knows it is you.")),
        ],
        protocol::MessagingStep::Heard { name, user, .. } => vec![
            Line::from(vec![gold("● "), bold("Is this you?")]),
            Line::from(""),
            Line::from(vec![
                bold(name.clone()),
                faint(format!("  Telegram {user}")),
            ]),
            Line::from(""),
            Line::from(plain("Yes pairs you and starts the gateway.")),
        ],
    });
    if with_code {
        lines.push(Line::from(""));
        lines.push(Line::from(faint("No camera? Open the link on your phone:")));
        lines.push(Line::from(plain(scan.setup.link.clone())));
    }
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    lines
}

fn scan_card(panel: &Panel, scan: &Scan, width: usize, room: usize) -> Card {
    let code: Vec<String> = scan.setup.qr_text.lines().map(str::to_owned).collect();
    let code_width = code.iter().map(|line| width_of(line)).max().unwrap_or(0);
    let keys = match scan.step {
        protocol::MessagingStep::Waiting { .. } => "c copy link | o open link | Esc cancel",
        protocol::MessagingStep::Created { .. } => "Esc cancel",
        protocol::MessagingStep::Heard { .. } => "Enter yes, that's me | n no | Esc cancel",
    };
    let title = title("Telegram".into(), "make your bot");
    // Once the bot exists the code has done its job; a code cut short cannot be scanned, so a
    // short window shows the link alone.
    let waiting = matches!(scan.step, protocol::MessagingStep::Waiting { .. });
    if !waiting || code.len() + 2 > room {
        let mut lines = vec![Line::from("")];
        lines.extend(scan_steps(panel, scan, false).into_iter().map(|line| {
            let mut spans = vec![plain("  ")];
            spans.extend(line.spans);
            Line::from(spans)
        }));
        lines.push(Line::from(""));
        return Card {
            title,
            keys: keys.into(),
            lines,
            focus: 0,
            caret: None,
        };
    }
    let steps = scan_steps(panel, scan, true);
    let beside = width >= code_width + 4 + 44;
    let mut lines = Vec::new();
    if beside {
        let gap = 4;
        for index in 0..code.len().max(steps.len() + 1) {
            let mut spans = Vec::new();
            match code.get(index) {
                Some(line) => spans.push(Span::styled(line.clone(), code_style())),
                None => spans.push(plain(" ".repeat(code_width))),
            }
            spans.push(plain(" ".repeat(gap)));
            // Steps start a line down, level with the code's first modules.
            if let Some(step) = index.checked_sub(1).and_then(|at| steps.get(at)) {
                spans.extend(step.spans.clone());
            }
            lines.push(Line::from(spans));
        }
    } else {
        lines.extend(
            code.iter()
                .map(|line| Line::from(Span::styled(line.clone(), code_style()))),
        );
        lines.push(Line::from(""));
        lines.extend(steps);
    }
    Card {
        title,
        keys: keys.into(),
        lines,
        focus: 0,
        caret: None,
    }
}

fn token_card(panel: &Panel, platform: &protocol::MessagingPlatform, typed: &str) -> Card {
    let mut lines = vec![Line::from("")];
    lines.extend(
        platform
            .steps
            .lines()
            .map(|step| Line::from(plain(format!("  {step}")))),
    );
    lines.push(Line::from(""));
    let prompt = format!("  {}: ", platform.token);
    let shown = masked(typed);
    let at = lines.len();
    let caret = Some((at, width_of(&prompt) + width_of(&shown)));
    lines.push(Line::from(vec![bold(prompt), plain(shown)]));
    lines.push(Line::from(faint(format!(
        "  Paste it here. It is checked with {}, then kept in Medha's key store, never shown.",
        platform.label
    ))));
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    lines.push(Line::from(""));
    Card {
        title: title(platform.label.clone(), "a bot you made yourself"),
        keys: if panel.busy {
            "Checking…"
        } else {
            "Enter save | Esc back"
        }
        .into(),
        focus: at,
        lines,
        caret,
    }
}

fn folder_card(panel: &Panel, typed: &str) -> Card {
    let mut lines = vec![
        Line::from(""),
        Line::from(plain(
            "  A chat on your phone can work in this folder once you start it there with /new.",
        )),
        Line::from(""),
    ];
    let prompt = "  Folder: ";
    let at = lines.len();
    let caret = Some((at, width_of(prompt) + width_of(typed)));
    lines.push(Line::from(vec![bold(prompt), plain(typed.to_owned())]));
    lines.push(Line::from(faint(
        "  A project folder. Your home folder, the whole disk and Medha's own data are refused.",
    )));
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    lines.push(Line::from(""));
    Card {
        title: title("Add a folder".into(), "for your phone"),
        keys: "Enter add | Esc back".into(),
        focus: at,
        lines,
        caret,
    }
}

fn code_card(panel: &Panel, typed: &str) -> Card {
    let mut lines = vec![
        Line::from(""),
        Line::from(plain(
            "  They were sent a code when they messaged the bot. Ask them for it.",
        )),
        Line::from(""),
    ];
    let prompt = "  Code: ";
    let at = lines.len();
    let caret = Some((at, width_of(prompt) + width_of(typed)));
    lines.push(Line::from(vec![bold(prompt), plain(typed.to_owned())]));
    if let Some(why) = &panel.problem {
        lines.push(Line::from(""));
        lines.push(problem(why));
    }
    lines.push(Line::from(""));
    Card {
        title: title("Let someone in".into(), "with the code they were sent"),
        keys: "Enter approve | Esc back".into(),
        focus: at,
        lines,
        caret,
    }
}

fn revoke_card(person: &protocol::MessagingPerson, label: &str) -> Card {
    Card {
        title: title(format!("Remove {}?", person.name), label),
        keys: "y remove | n keep".into(),
        lines: vec![
            Line::from(""),
            Line::from(plain(format!(
                "  {} can no longer message Medha. They can ask again, and get a new code.",
                person.name
            ))),
            Line::from(""),
        ],
        focus: 0,
        caret: None,
    }
}

fn done_card(saved: &protocol::MessagingSaved) -> Card {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            ok("  ✓ "),
            bold(if saved.paired { "Paired." } else { "Saved." }),
            plain(if saved.paired {
                format!(
                    " Message {} from your phone; Medha works in {}.",
                    saved.account,
                    shown(&saved.folder)
                )
            } else {
                format!(" Medha works in {}.", shown(&saved.folder))
            }),
        ]),
    ];
    match &saved.problem {
        None => lines.push(Line::from(faint(
            "  The gateway runs now, at login and after a crash.",
        ))),
        Some(why) => {
            lines.push(problem(&format!("It could not be started at login: {why}")));
            lines.push(Line::from(faint(
                "  Start it yourself with: medha gateway run",
            )));
        }
    }
    if !saved.paired {
        lines.push(Line::from(""));
        lines.push(Line::from(plain(format!(
            "  Message {} from your phone. It answers with a code; approve it here in /messaging.",
            saved.account
        ))));
    }
    lines.push(Line::from(""));
    Card {
        title: title("Messaging".into(), "ready"),
        keys: "Enter done".into(),
        lines,
        focus: 0,
        caret: None,
    }
}

pub(super) fn draw(f: &mut Frame, model: &Model, input_area: Rect) {
    let Some(panel) = &model.messaging else {
        return;
    };
    let width = input_area.width.saturating_sub(4) as usize;
    let platform = |name: &str| {
        panel
            .overview
            .as_ref()
            .and_then(|overview| overview.platforms.iter().find(|p| p.name == name))
    };
    let card = match &panel.screen {
        Screen::Overview { selected } => overview_card(panel, *selected, width),
        Screen::Choose {
            platform: name,
            selected,
        } => choose_card(
            panel,
            platform(name).map_or(name.as_str(), |p| p.label.as_str()),
            *selected,
            width,
        ),
        Screen::Scan(scan) => scan_card(panel, scan, width, input_area.y as usize),
        Screen::Token {
            platform: name,
            typed,
        } => match platform(name) {
            Some(platform) => token_card(panel, platform, typed),
            None => return,
        },
        Screen::Code { typed, .. } => code_card(panel, typed),
        Screen::Folder { typed } => folder_card(panel, typed),
        Screen::Revoke(person) => revoke_card(
            person,
            platform(&person.platform).map_or(person.platform.as_str(), |p| p.label.as_str()),
        ),
        Screen::Done(saved) => done_card(saved),
    };
    draw_card(f, card, input_area);
}
