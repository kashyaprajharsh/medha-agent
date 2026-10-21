//! Drawing the connect screens as the approved terminal design shows them: one bordered card
//! above the composer, its title on the left and its keys on the right.

use super::connect::{
    List, Plan, Provider, Row, Screen, context_short, filtered, host_of, looks_like_key, masked,
    price_label, rows, settled_row,
};
use super::*;
use ratatui::widgets::{BorderType, Clear};

/// Where the name column ends and the detail column starts, as in the design.
const NAME: usize = 24;

struct Card {
    title: Vec<Span<'static>>,
    keys: String,
    lines: Vec<Line<'static>>,
    /// The line to keep in view when the card is taller than the space above the composer.
    focus: usize,
}

fn faint(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(theme::faint()))
}

fn plain(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(theme::text()))
}

fn bold(text: impl Into<String>) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
    )
}

fn gold(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(theme::accent()))
}

fn ok(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(theme::ok()))
}

/// The chosen row's band: the accent laid lightly over the card, where the terminal allows.
fn band() -> Color {
    match (theme::accent(), theme::code_bg()) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |a: u8, b: u8| ((u16::from(a) * 3 + u16::from(b) * 13) / 16) as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => theme::code_bg(),
    }
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// One list line: marker, name column, detail, and a state set flush right.
fn row(
    width: usize,
    chosen: bool,
    name: Span<'static>,
    detail: Span<'static>,
    state: Span<'static>,
) -> Line<'static> {
    let name_width = width_of(&name.content);
    let left = 2 + NAME.max(name_width + 2) + width_of(&detail.content);
    let gap = width.saturating_sub(left + width_of(&state.content)).max(1);
    let mut spans = vec![
        if chosen { gold("> ") } else { plain("  ") },
        name,
        plain(" ".repeat(NAME.saturating_sub(name_width).max(2))),
        detail,
        plain(" ".repeat(gap)),
        state,
    ];
    if chosen {
        let style = Style::default().bg(band());
        for span in &mut spans {
            span.style = span.style.patch(style);
        }
        if let Some(name) = spans.get_mut(1) {
            name.style = name.style.add_modifier(Modifier::BOLD);
        }
    }
    Line::from(spans)
}

fn list_card(model: &Model, list: &List, width: usize) -> Card {
    let query = list.query.trim();
    let shown = if looks_like_key(query) {
        masked(query)
    } else {
        list.query.clone()
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            faint("search or paste a key › "),
            bold(shown),
            gold("▏"),
        ]),
        Line::from(""),
    ];
    if let Some(key) = &list.carried {
        lines.push(Line::from(faint(format!(
            "Pick the provider for {}. Nothing is sent until you choose.",
            masked(key)
        ))));
        lines.push(Line::from(""));
    }
    let all = rows(model, list);
    let selected = settled_row(&all, list.selected);
    let asking = all
        .iter()
        .any(|row| matches!(row, Row::Owner(_) | Row::AnyOwner(_)));
    if asking {
        let several = list
            .recognised
            .as_ref()
            .is_some_and(|(_, found)| found.matched == protocol::KeyMatch::Several);
        let opening: String = query
            .chars()
            .take_while(|c| *c != '-')
            .chain(['-'])
            .collect();
        lines.push(Line::from(bold("Which provider is this key for?")));
        lines.push(Line::from(faint(if several {
            format!("Several providers use keys that start with {opening}. Nothing is sent until you choose.")
        } else {
            "Medha does not know this key's opening letters. Nothing is sent until you choose.".to_owned()
        })));
        lines.push(Line::from(""));
    }
    let mut focus = lines.len();
    for (i, entry) in all.iter().enumerate() {
        let chosen = i == selected && entry.choosable() && !list.opening;
        if chosen {
            focus = lines.len();
        }
        let line = match entry {
            Row::Group(title) => {
                if lines
                    .last()
                    .is_some_and(|line| !line.spans.is_empty() && i > 0)
                {
                    lines.push(Line::from(""));
                }
                Line::from(faint(*title))
            }
            Row::ChatGpt => row(
                width,
                chosen,
                plain("ChatGPT"),
                plain("Plus, Pro, or Business, with Codex. No key needed."),
                if list.opening {
                    gold("opening ChatGPT…")
                } else if chosen {
                    gold("Enter: continue with ChatGPT")
                } else {
                    faint("")
                },
            ),
            Row::Keyed { provider, saved } => row(
                width,
                chosen,
                plain(provider.name.clone()),
                plain(provider_detail(provider)),
                if *saved {
                    ok("● key saved")
                } else {
                    faint("add key")
                },
            ),
            Row::More(n) => Line::from(vec![
                faint(format!("  … {n} more")),
                plain(" ".repeat(NAME.saturating_sub(width_of(&format!("… {n} more"))))),
                faint("type to search"),
            ]),
            Row::Local { provider, running } => row(
                width,
                chosen,
                plain(provider.name.clone()),
                plain(provider.url.as_deref().map(host_of).unwrap_or_default()),
                match running {
                    Some(n) => ok(format!(
                        "● running, {n} {}",
                        if *n == 1 { "model" } else { "models" }
                    )),
                    None => faint("not running"),
                },
            ),
            Row::Own(profile) => row(
                width,
                chosen,
                plain(host_of(&profile.base_url)),
                plain("your server"),
                if profile.key_present {
                    ok("● key saved")
                } else {
                    faint("")
                },
            ),
            Row::Another => row(
                width,
                chosen,
                plain("Another server"),
                plain("any address that speaks OpenAI or Gemini"),
                faint("type an address"),
            ),
            Row::Owner(owner) => row(
                width,
                chosen,
                plain(owner.name.clone()),
                plain(owner_detail(model, &owner.id)),
                faint(owner.reason.clone().unwrap_or_default()),
            ),
            Row::AnyOwner(n) => row(
                width,
                chosen,
                faint("Another provider"),
                faint(format!("search all {n}")),
                faint(""),
            ),
            Row::Unavailable(provider) => Line::from(vec![
                plain(format!("  {}", provider.name)),
                plain(" ".repeat(NAME.saturating_sub(width_of(&provider.name)).max(2))),
                faint(
                    provider
                        .unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "Not available yet.".into()),
                ),
            ]),
        };
        lines.push(line);
    }
    lines.push(Line::from(""));
    let (providers, _) = model
        .remote
        .as_ref()
        .map_or((&[][..], ()), |peer| (&peer.models.providers[..], ()));
    let missing = providers.iter().filter(|p| !p.available).count();
    lines.push(Line::from(faint(if asking {
        "Medha never tries a key on several providers.".to_owned()
    } else if list.unavailable {
        "Esc returns to the list.".to_owned()
    } else if missing == 1 {
        "1 provider needs a connection Medha doesn't have yet. Press ? to see it.".to_owned()
    } else if missing > 1 {
        format!(
            "{missing} providers need a connection Medha doesn't have yet. Press ? to see them."
        )
    } else {
        String::new()
    })));
    let keys = if asking || !list.query.is_empty() {
        "↑↓ move | Enter choose | Esc clear"
    } else {
        "/ search | ↑↓ move | Enter choose | Ctrl-R refresh | Esc close"
    };
    Card {
        title: vec![bold("connect a model")],
        keys: keys.into(),
        lines,
        focus,
    }
}

fn provider_detail(provider: &protocol::ModelProvider) -> String {
    match provider.models.len() {
        0 => provider.url.as_deref().map(host_of).unwrap_or_default(),
        1 => "1 model".into(),
        n => format!("{n} models"),
    }
}

fn owner_detail(model: &Model, id: &str) -> String {
    model
        .remote
        .as_ref()
        .and_then(|peer| peer.models.providers.iter().find(|p| p.id == id))
        .map(provider_detail)
        .unwrap_or_default()
}

fn model_line(
    width: usize,
    chosen: bool,
    found: &protocol::DiscoveredModel,
    columns: bool,
) -> Line<'static> {
    let name = found.name.clone().unwrap_or_else(|| found.id.clone());
    if !columns {
        return row(
            width,
            chosen,
            plain(name),
            faint("included in your plan"),
            faint(""),
        );
    }
    let context = context_short(found.context_length);
    let price = price_label(found);
    let right = format!("{context:>8}  {price:>17}");
    row(width, chosen, plain(name), plain(""), plain(right))
}

fn tested_line(result: &protocol::ModelTestResult) -> Line<'static> {
    let passed =
        |check: &protocol::ModelTestCheck| check.status == protocol::ModelTestStatus::Passed;
    if passed(&result.text) && passed(&result.streaming) && passed(&result.tools) {
        return Line::from(vec![
            ok("✓ replies, streams and calls tools"),
            faint("   checked just now"),
        ]);
    }
    let failed: Vec<String> = [
        ("replies", &result.text),
        ("streams", &result.streaming),
        ("calls tools", &result.tools),
    ]
    .into_iter()
    .filter(|(_, check)| !passed(check))
    .map(|(what, check)| format!("{what}: {}", check.message))
    .collect();
    Line::from(Span::styled(
        format!("✗ {}", failed.join(" · ")),
        Style::default().fg(theme::err()),
    ))
}

fn provider_card(model: &Model, setup: &ModelSetup, provider: &Provider, width: usize) -> Card {
    let brand = setup
        .provider
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| {
            if setup.base_url.is_empty() {
                "Another server".into()
            } else {
                host_of(&setup.base_url)
            }
        });
    let address = setup
        .base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_owned();
    let mut lines = vec![Line::from("")];
    let mut focus = 0;
    // Choosing what the server speaks, for a server of one's own.
    if let Some(Picker {
        kind: PickerKind::ModelProtocol,
        selected,
    }) = &model.picker
    {
        lines.push(Line::from(faint("What does this server speak?")));
        for (i, (label, _)) in MODEL_PROTOCOLS.iter().enumerate() {
            if i == *selected {
                focus = lines.len();
            }
            lines.push(row(
                width,
                i == *selected,
                plain(*label),
                faint(""),
                faint(""),
            ));
        }
        return Card {
            title: vec![bold(brand)],
            keys: "↑↓ move | Enter choose | Esc back".into(),
            lines,
            focus,
        };
    }
    let saved = model.remote.as_ref().is_some_and(|peer| {
        peer.models
            .profiles
            .iter()
            .any(|p| p.key_present && p.base_url == setup.base_url)
    });
    let typing = matches!(
        setup.step,
        ModelSetupStep::EndpointParameter
            | ModelSetupStep::BaseUrl
            | ModelSetupStep::ApiKey
            | ModelSetupStep::OtherProviderKey
            | ModelSetupStep::ContextWindow
            | ModelSetupStep::Certificate
    ) || (matches!(setup.step, ModelSetupStep::ModelId) && provider.found.is_empty());
    if !matches!(setup.step, ModelSetupStep::ApiKey)
        && setup.auth != Some(protocol::ModelAuth::None)
    {
        match (setup.api_key.is_empty(), saved) {
            // A typed key shows its opening letters; a saved one never leaves the backend.
            (false, _) => lines.push(Line::from(vec![
                faint("key     "),
                ok("● "),
                plain(masked(&setup.api_key)),
                faint("   Tab replaces it."),
            ])),
            (true, true) => lines.push(Line::from(vec![
                faint("key     "),
                ok("● "),
                plain("saved on this computer."),
                faint(" Tab replaces it."),
            ])),
            (true, false)
                if !matches!(
                    setup.step,
                    ModelSetupStep::EndpointParameter | ModelSetupStep::BaseUrl
                ) =>
            {
                lines.push(Line::from(vec![faint("key     "), faint("none")]));
            }
            (true, false) => {}
        }
        lines.push(Line::from(""));
    }
    if typing {
        let (text, _) = (&setup.editor.text, setup.editor.cursor);
        let shown = if setup.is_secret() {
            "•".repeat(text.chars().count())
        } else {
            text.clone()
        };
        lines.push(Line::from(faint(setup.prompt())));
        focus = lines.len();
        lines.push(Line::from(vec![gold("› "), bold(shown), gold("▏")]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            gold("Enter"),
            faint(" continues    Esc: back to the list"),
        ]));
        return Card {
            title: title(brand, &address),
            keys: "Enter continue | Esc back".into(),
            lines,
            focus,
        };
    }
    let busy = matches!(
        setup.step,
        ModelSetupStep::Discovering
            | ModelSetupStep::CheckingKey
            | ModelSetupStep::Resolving
            | ModelSetupStep::Saving
            | ModelSetupStep::Activating
    );
    if busy {
        let doing = match setup.step {
            ModelSetupStep::Discovering => format!("Finding {brand}'s models…"),
            ModelSetupStep::CheckingKey => "Reading the key's opening letters…".into(),
            ModelSetupStep::Resolving => "Checking the address…".into(),
            _ => "Saving the model…".into(),
        };
        lines.push(Line::from(vec![gold("● "), plain(doing)]));
        return Card {
            title: title(brand, &address),
            keys: "Esc back".into(),
            lines,
            focus,
        };
    }
    let shown = filtered(provider);
    let filter = provider.filter.as_deref();
    let mut header = vec![faint("  model   ")];
    match filter {
        Some(text) => header.extend([faint("filter › "), bold(text.to_owned()), gold("▏")]),
        None => header.push(faint("/ to filter")),
    }
    let right = format!("{:>8}  {:>17}", "context", "per 1M, in / out");
    let used: usize = header.iter().map(|span| width_of(&span.content)).sum();
    header.push(plain(
        " ".repeat(width.saturating_sub(used + width_of(&right)).max(1)),
    ));
    header.push(faint(right));
    lines.push(Line::from(header));
    for (i, found) in shown.iter().enumerate() {
        if i == provider.selected {
            focus = lines.len();
        }
        lines.push(model_line(width, i == provider.selected, found, true));
    }
    if shown.is_empty() {
        lines.push(Line::from(faint("  No model matches the filter.")));
    }
    lines.push(Line::from(""));
    if matches!(setup.step, ModelSetupStep::Testing) {
        lines.push(Line::from(vec![
            gold("● "),
            plain("Checking: up to four small requests, which your provider may bill…"),
        ]));
        lines.push(Line::from(""));
    } else if let Some(result) = &provider.tested {
        lines.push(tested_line(result));
        lines.push(Line::from(""));
    }
    let name = shown
        .get(provider.selected)
        .map(|m| m.name.clone().unwrap_or_else(|| m.id.clone()))
        .unwrap_or_default();
    lines.push(Line::from(vec![
        gold("Enter"),
        bold(format!(" adds {name}")),
        faint("    a: name, limits and advanced settings    "),
        faint(if provider.tested.is_some() {
            "c: check again"
        } else {
            "c: check connection"
        }),
    ]));
    let keys = if filter.is_some() {
        "↑↓ move | Enter add | Esc stop filtering"
    } else {
        "↑↓ move | Enter add | Tab key | a advanced | Esc back"
    };
    Card {
        title: title(brand, &address),
        keys: keys.into(),
        lines,
        focus,
    }
}

fn title(name: String, detail: &str) -> Vec<Span<'static>> {
    vec![bold(name), faint(format!("   {detail}"))]
}

fn waiting_card(link: &str, again: bool) -> Card {
    let lines = vec![
        Line::from(""),
        if again {
            Line::from(vec![
                gold("● "),
                bold("One more step in your browser."),
                plain(" Pick your ChatGPT account once more to finish connecting Medha."),
            ])
        } else {
            Line::from(vec![
                gold("● "),
                bold("Continue in your browser."),
                plain(" Sign in to ChatGPT and allow Medha to use your plan."),
            ])
        },
        Line::from(faint(
            "  This screen moves on by itself when you are done. Medha waits up to 5 minutes.",
        )),
        Line::from(""),
        Line::from(faint(
            "  Browser did not open? Open this link on this computer:",
        )),
        Line::from(plain(format!("  {link}"))),
        Line::from(""),
        Line::from(faint(
            "  Nothing is kept until ChatGPT sends you back. No API key is used.",
        )),
    ];
    Card {
        title: title("ChatGPT".into(), "your Plus, Pro, or Business plan"),
        keys: "c copy link | o open again | Esc cancel".into(),
        lines,
        focus: 0,
    }
}

fn plan_off_card(account: &protocol::ChatGptAccount) -> Card {
    let who = account
        .email
        .clone()
        .unwrap_or_else(|| "your account".into());
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            bold("Plan use is off for Medha."),
            plain(" You signed in, but did not allow Medha to use your ChatGPT plan,"),
        ]),
        Line::from(plain("so Medha cannot send requests with it yet.")),
        Line::from(""),
        Line::from(vec![
            gold("Enter"),
            plain(" allow plan use (opens ChatGPT again)"),
            faint("    Esc: back to the list"),
        ]),
    ];
    Card {
        title: title("ChatGPT".into(), &format!("{who}, plan use off")),
        keys: "Enter allow | Esc back".into(),
        lines,
        focus: 0,
    }
}

fn plan_card(setup: &ModelSetup, plan: &Plan, width: usize) -> Card {
    let who = plan
        .account
        .email
        .clone()
        .unwrap_or_else(|| "your account".into());
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            faint("model   "),
            faint("from your plan, in OpenAI's order"),
        ]),
    ];
    let mut focus = 0;
    if plan.loading {
        lines.push(Line::from(vec![
            gold("● "),
            plain("Finding your plan's models…"),
        ]));
    }
    for (i, found) in plan.models.iter().enumerate() {
        if i == plan.selected {
            focus = lines.len();
        }
        lines.push(model_line(width, i == plan.selected, found, false));
    }
    lines.push(Line::from(""));
    if matches!(setup.step, ModelSetupStep::Testing) {
        lines.push(Line::from(vec![
            gold("● "),
            plain("Checking the connection…"),
        ]));
        lines.push(Line::from(""));
    } else if matches!(
        setup.step,
        ModelSetupStep::Saving | ModelSetupStep::Activating
    ) {
        lines.push(Line::from(vec![gold("● "), plain("Saving the model…")]));
        lines.push(Line::from(""));
    } else if let Some(result) = &plan.tested {
        lines.push(tested_line(result));
        lines.push(Line::from(""));
    }
    if let Some(found) = plan.models.get(plan.selected) {
        let name = found.name.clone().unwrap_or_else(|| found.id.clone());
        lines.push(Line::from(vec![
            gold("Enter"),
            bold(format!(" adds {name}")),
            faint("    a: name, limits and advanced settings    c: check connection"),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(faint(
        "You're using your ChatGPT plan. Medha's requests count toward its usage limits;",
    )));
    lines.push(Line::from(faint("on Plus the five-hour limit is shared with ChatGPT and your other apps. /usage manage opens Manage usage.")));
    Card {
        title: title("ChatGPT".into(), &format!("{who}, plan use on")),
        keys: "↑↓ move | Enter add | s switch account | Esc back".into(),
        lines,
        focus,
    }
}

fn accounts_card(
    accounts: &[protocol::ChatGptAccount],
    selected: usize,
    loading: bool,
    width: usize,
) -> Card {
    let mut lines = vec![
        Line::from(""),
        Line::from(faint("Models on your ChatGPT plan use the active account.")),
        Line::from(""),
    ];
    let mut focus = 0;
    if loading && accounts.is_empty() {
        lines.push(Line::from(vec![
            gold("● "),
            plain("Reading your accounts…"),
        ]));
    }
    if !loading && accounts.is_empty() {
        lines.push(Line::from(plain("No ChatGPT account is signed in yet.")));
    }
    for (i, account) in accounts.iter().enumerate() {
        if i == selected {
            focus = lines.len();
        }
        let state = if !account.signed_in {
            faint("Signed out")
        } else {
            let plan = if account.plan_enabled {
                "Plan use on"
            } else {
                "Plan use off"
            };
            if account.active {
                ok(format!("● Active · {plan}"))
            } else {
                faint(format!("Signed in · {plan}"))
            }
        };
        let email = account
            .email
            .clone()
            .unwrap_or_else(|| account.client_id.clone());
        lines.push(row(
            width,
            i == selected && !loading,
            plain(email),
            faint(""),
            state,
        ));
    }
    lines.push(Line::from(""));
    let action = match accounts.get(selected) {
        Some(a) if !a.signed_in => "Enter: sign in    ",
        Some(a) if !a.active => "Enter: use this one    o: sign out    ",
        Some(_) => "o: sign out    ",
        None => "",
    };
    lines.push(Line::from(vec![
        faint(action),
        faint("n: add another account"),
    ]));
    Card {
        title: vec![bold("ChatGPT accounts")],
        keys: "↑↓ move | Enter use or sign in | o sign out | n add | Esc close".into(),
        lines,
        focus,
    }
}

/// The pause after the ChatGPT plan refused a turn (T6): its keys work while nothing is typed.
pub(super) fn draw_plan_limit(f: &mut Frame, model: &Model, input_area: Rect) {
    let Some(code) = &model.plan_limit else {
        return;
    };
    let reached = code.contains("usage_limit");
    let head = if reached {
        vec![
            gold("■ "),
            gold("Your ChatGPT plan's limit for Medha is reached."),
            plain(" Medha paused instead of switching models."),
        ]
    } else {
        vec![
            gold("■ "),
            gold(format!("ChatGPT turned down this request ({code}).")),
        ]
    };
    let switch = match super::connect::stand_in(model) {
        Some(name) => format!("f: switch this chat to {name}"),
        None => "/model: choose another model".into(),
    };
    let lines = vec![
        Line::from(head),
        Line::from(vec![
            faint("  u: Manage usage (opens ChatGPT settings)    "),
            faint(switch),
            faint("    Esc: dismiss"),
        ]),
    ];
    if input_area.y < lines.len() as u16 {
        return;
    }
    let height = lines.len() as u16;
    let area = Rect::new(
        input_area.x,
        input_area.y - height,
        input_area.width,
        height,
    );
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::bg())),
        area,
    );
    f.render_widget(Paragraph::new(lines), area);
}

/// Draw the connect card for the screen the setup is on, above the composer.
pub(super) fn draw(f: &mut Frame, model: &Model, input_area: Rect) {
    let Some(setup) = &model.model_setup else {
        return;
    };
    let Some(screen) = &setup.screen else { return };
    let width = input_area.width.saturating_sub(4) as usize;
    let card = match screen {
        Screen::List(list) => list_card(model, list, width),
        Screen::Provider(provider) => provider_card(model, setup, provider, width),
        Screen::Waiting { link, again, .. } => waiting_card(link, *again),
        Screen::PlanOff { account } => plan_off_card(account),
        Screen::Plan(plan) => plan_card(setup, plan, width),
        Screen::Accounts {
            accounts,
            selected,
            loading,
        } => accounts_card(accounts, *selected, *loading, width),
    };
    let available = input_area.y;
    if available < 4 {
        return;
    }
    let height = (card.lines.len() as u16 + 2).min(available);
    let area = Rect::new(
        input_area.x,
        input_area.y - height,
        input_area.width,
        height,
    );
    let mut title = vec![plain(" ")];
    title.extend(card.title);
    title.push(plain(" "));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::border()))
        .title(Line::from(title))
        .title_top(Line::from(faint(format!(" {} ", card.keys))).right_aligned())
        .padding(ratatui::widgets::Padding::horizontal(1));
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::code_bg())),
        area,
    );
    let inner = block.inner(area);
    let rows = inner.height.max(1) as usize;
    let scroll = card.focus.saturating_sub(rows.saturating_sub(2));
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(card.lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}
