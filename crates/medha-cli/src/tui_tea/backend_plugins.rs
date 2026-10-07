//! Plugin presentation and exact-hash grant review. Stores live in the backend.
pub(super) fn matching<'a>(rows: &'a [DiscoverRow], query: &str) -> Vec<&'a DiscoverRow> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut scored: Vec<(u32, &DiscoverRow)> = rows
        .iter()
        .filter_map(|row| {
            let mut total = 0;
            for word in &words {
                total += score(row, word)?;
            }
            Some((total, row))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, row)| row).collect()
}

fn score(row: &DiscoverRow, word: &str) -> Option<u32> {
    let name = row.name.to_lowercase();
    let parts: Vec<&str> = name.split(['-', '_', '.']).collect();
    if name == word {
        Some(100)
    } else if name.starts_with(word) {
        Some(80)
    } else if parts.iter().any(|part| part.starts_with(word)) {
        Some(70)
    } else if name.contains(word) {
        Some(60)
    } else if row.description.to_lowercase().contains(word) {
        Some(30)
    } else if row.market.to_lowercase().contains(word) {
        Some(20)
    } else if word.len() >= 4 && parts.iter().any(|part| one_edit(part, word)) {
        Some(15)
    } else if word.len() >= 3 && in_order(&name, word) {
        Some(10)
    } else {
        None
    }
}

fn in_order(text: &str, word: &str) -> bool {
    let mut letters = text.chars();
    word.chars().all(|c| letters.any(|t| t == c))
}

/// One insertion, deletion, substitution, or swap of neighbours apart.
fn one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let swapped = a.len() == b.len()
        && a.len() == prefix + suffix + 2
        && a[prefix] == b[prefix + 1]
        && a[prefix + 1] == b[prefix];
    swapped || (a.len().max(b.len()) - prefix - suffix <= 1 && a != b)
}

/// The first row is the search field; the last three add, refresh, and go back.
pub(super) fn discover_labels(all: &[DiscoverRow], query: &str) -> Vec<String> {
    let rows = matching(all, query);
    let search = if query.is_empty() {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for row in all {
            match counts.iter_mut().find(|(market, _)| *market == row.market) {
                Some((_, count)) => *count += 1,
                None => counts.push((row.market.clone(), 1)),
            }
        }
        let from: Vec<String> = counts
            .iter()
            .map(|(market, count)| format!("{market} {count}"))
            .collect();
        format!(
            "🔍 Type to search {} plugins — from {}",
            all.len(),
            from.join(" · ")
        )
    } else {
        format!("🔍 {query}▏  {} match(es)", rows.len())
    };
    let mut labels = vec![search];
    labels.extend(rows.iter().map(|row| {
        let description: String = row.description.chars().take(70).collect();
        format!(
            "{} {}  — {description}  [{}]",
            if row.installed { "●" } else { "○" },
            row.name,
            row.market
        )
    }));
    labels.push("➕ Add a marketplace (owner/repo)…".to_string());
    labels.push("⟳ Refresh marketplaces".to_string());
    labels.push("← Back".to_string());
    labels
}
use super::backend_ui::{After, Effect, request};
use super::*;
use protocol::{
    ChangeResource as Change, PluginActivation as Activation, PluginSummary as Row,
    ReadResource as Read,
};

#[derive(Clone)]
pub(super) enum Screen {
    List {
        plugins: Vec<Row>,
        notices: Vec<String>,
    },
    Discover {
        rows: Vec<DiscoverRow>,
        query: String,
    },
    Detail(Row),
    Grant {
        row: Row,
        grant: protocol::Grant,
        review: bool,
    },
    Remove(Row),
    Notices(Vec<String>),
    HookEvent,
    HookTools(String),
}
#[derive(Clone)]
pub(super) struct DiscoverRow {
    market: String,
    name: String,
    description: String,
    installed: bool,
    id: String,
}
impl Screen {
    pub(super) fn title(&self) -> String {
        match self {
            Self::List { .. } => {
                " plugins — ↑↓ select · Space on/off · Enter details · Esc close ".into()
            }
            Self::Discover { .. } => {
                " discover plugins — type to search · ↑↓ select · Enter install · Esc back ".into()
            }
            Self::Detail(row) => format!(" {} — ↑↓ select · Enter · Esc back ", row.id),
            Self::Grant { review: true, .. } => {
                " allow these hooks? — Enter confirm · Esc ask next time ".into()
            }
            Self::Grant { row, .. } => format!(
                " allow {} the access listed above? — Enter confirm · Esc back ",
                row.id
            ),
            Self::Remove(row) => format!(" remove {}? — Enter confirm · Esc back ", row.id),
            Self::Notices(_) => " plugin warnings — Esc back ".into(),
            Self::HookEvent => " add a hook: when should it run? — Enter · Esc cancel ".into(),
            Self::HookTools(event) => {
                format!(" {event}: which tools start it? — Enter · Esc back ")
            }
        }
    }
    pub(super) fn labels(&self) -> Vec<String> {
        match self {
            Self::List { plugins, notices } => {
                let mut rows = vec![
                    "➕ Install owner/repo, name@marketplace or folder…".into(),
                    "🔎 Discover plugins from marketplaces".into(),
                ];
                rows.extend(plugins.iter().map(|row| {
                    format!(
                        "{} {} {} · {:?} · {} — {}",
                        if row.activation == Activation::Enabled {
                            "●"
                        } else {
                            "○"
                        },
                        row.id,
                        row.version,
                        row.scope,
                        row.health.as_deref().unwrap_or(activation(row.activation)),
                        row.components
                            .iter()
                            .map(|component| component.kind.as_str())
                            .collect::<Vec<_>>()
                            .join(" · ")
                    )
                }));
                if !notices.is_empty() {
                    rows.push(format!("⚠ {} warnings — view", notices.len()));
                }
                rows.push("🩺 Check that plugins can run".into());
                rows
            }
            Self::Discover { rows, query } => discover_labels(rows, query),
            Self::Detail(row) => actions(row).into_iter().map(|(label, _)| label).collect(),
            Self::Grant { review: true, .. } => vec![
                "Keep off (don't ask again)".into(),
                "Allow and turn on".into(),
            ],
            Self::Grant { .. } => vec![
                "Keep disabled".into(),
                "Allow this access and enable".into(),
            ],
            Self::Remove(_) => vec!["Keep plugin".into(), "Remove plugin".into()],
            Self::Notices(notices) => notices
                .iter()
                .cloned()
                .chain(std::iter::once("← Back".into()))
                .collect(),
            Self::HookEvent => crate::hook_files::EVENTS
                .iter()
                .map(|(event, description, _)| format!("{event} — {description}"))
                .chain(std::iter::once("📋 See installed hooks and plugins".into()))
                .collect(),
            Self::HookTools(_) => crate::hook_files::TOOLS
                .iter()
                .map(|(_, description)| (*description).into())
                .collect(),
        }
    }
}
fn activation(activation: Activation) -> &'static str {
    match activation {
        Activation::Enabled => "enabled",
        Activation::Disabled => "disabled",
        Activation::Changed => "changed",
        Activation::Collision => "collision",
        Activation::Shadowed => "shadowed",
    }
}
enum Action {
    Enable,
    Disable,
    Run(String),
    Update,
    Rollback,
    Remove,
    Back,
}
fn actions(row: &Row) -> Vec<(String, Action)> {
    let mut actions = Vec::new();
    match row.activation {
        Activation::Enabled => actions.push(("Disable".into(), Action::Disable)),
        Activation::Disabled | Activation::Changed => {
            actions.push(("Review access and enable".into(), Action::Enable))
        }
        _ => {}
    }
    if row.activation == Activation::Enabled {
        actions.extend(row.actions.iter().map(|action| {
            (
                format!("▶ Run action: {}", action.title),
                Action::Run(action.id.clone()),
            )
        }));
    }
    if row.updatable {
        actions.push(("⟳ Update".into(), Action::Update));
    }
    if row.can_rollback {
        actions.push(("↶ Roll back".into(), Action::Rollback));
    }
    if row.scope == protocol::ResourceScope::User {
        actions.push(("Remove".into(), Action::Remove));
    }
    actions.push(("← Back".into(), Action::Back));
    actions
}
fn screen(model: &mut Model, screen: Screen) {
    let selected = if let Screen::Discover { rows, query } = &screen {
        usize::from(!matching(rows, query).is_empty())
    } else {
        0
    };
    model.picker = Some(Picker::with_selected(
        PickerKind::BackendPlugins(Box::new(screen)),
        selected,
    ));
}
fn open(model: &mut Model) {
    request(model, Effect::Read(Read::Plugins, After::Plugins));
}
fn review(model: &mut Model, row: Row, startup: bool) {
    if let Some(blocked) = &row.blocked {
        model.push_notice(format!("{} cannot be enabled: {blocked}", row.id));
        return;
    }
    model.push_notice(format!(
        "{} · reviewed files {}\n{}",
        row.id, row.hash, row.access_description
    ));
    let Some(grant) = row.grant.clone() else {
        model.push_notice("The backend returned no verified access proposal for this plugin.");
        return;
    };
    if !startup && !grant.network && grant.read_paths.is_empty() && grant.write_paths.is_empty() {
        request(
            model,
            Effect::Change(
                Change::EnablePlugin {
                    id: row.id.clone(),
                    scope: row.scope,
                    hash: row.hash,
                    grant,
                },
                After::Reload(Box::new(After::PluginsAt(row.id))),
            ),
        );
    } else {
        screen(
            model,
            Screen::Grant {
                row,
                grant,
                review: startup,
            },
        );
    }
}
pub(super) fn catalogue(model: &mut Model, catalogue: protocol::PluginCatalogue, after: After) {
    let keep = if let After::PluginsAt(id) = &after {
        catalogue
            .plugins
            .iter()
            .position(|row| &row.id == id)
            .map(|index| index + 2)
    } else {
        None
    };
    let selected = match &after {
        After::PluginEnable(id) | After::PluginDisable(id) | After::PluginRemove(id) => {
            catalogue.plugins.iter().find(|row| &row.id == id).cloned()
        }
        _ => None,
    };
    let rows = catalogue.plugins.clone();
    let notices = catalogue.notices.clone();
    model.remote.as_mut().expect("viewer").plugins = catalogue;
    if let Some(row) = selected {
        match after {
            After::PluginEnable(_) => review(model, row, false),
            After::PluginDisable(_) => {
                let after = After::Reload(Box::new(After::PluginsAt(row.id.clone())));
                request(
                    model,
                    Effect::Change(
                        Change::DisablePlugin {
                            id: row.id,
                            scope: row.scope,
                        },
                        after,
                    ),
                );
            }
            After::PluginRemove(_) => screen(model, Screen::Remove(row)),
            _ => {}
        }
    } else if matches!(
        after,
        After::PluginEnable(_) | After::PluginDisable(_) | After::PluginRemove(_)
    ) {
        model.push_notice("Plugin not found.");
    } else if matches!(after, After::Startup) {
        review_startup(model);
    } else {
        screen(
            model,
            Screen::List {
                plugins: rows,
                notices,
            },
        );
        if let Some(selected) = keep
            && let Some(picker) = &mut model.picker
        {
            picker.selected = selected;
        }
    }
}
/// Defer the startup access review while another form owns the keyboard.
/// Taking the proposal prevents Esc from reopening it on every animation tick.
pub(super) fn review_startup(model: &mut Model) {
    if model.model_setup.is_some()
        || model.search_setup.is_some()
        || model.mcp_credential.is_some()
        || model.picker.is_some()
        || model.foreground_owned()
        || model.session_op.is_some()
        || !model.pending_approvals.is_empty()
        || model.clarify.is_some()
    {
        return;
    }
    if let Some(row) = model
        .remote
        .as_mut()
        .and_then(|peer| peer.plugins.hook_review.take())
    {
        review(model, row, true);
        model.dirty = true;
    }
}
pub(super) fn markets(model: &mut Model, markets: protocol::Marketplaces) {
    let rows = markets
        .marketplaces
        .into_iter()
        .flat_map(|market| {
            market.plugins.into_iter().map(move |plugin| DiscoverRow {
                id: extensions::sources::listed_id(&market.name, &plugin.name),
                market: market.name.clone(),
                name: plugin.name,
                description: plugin.description,
                installed: plugin.installed,
            })
        })
        .collect();
    screen(
        model,
        Screen::Discover {
            rows,
            query: String::new(),
        },
    );
}
pub(super) fn hooks(model: &mut Model, hooks: protocol::HookCatalogue) {
    let text = hooks
        .hooks
        .iter()
        .map(|hook| {
            format!(
                "{} / {} [{}] {}",
                hook.event, hook.file, hook.matcher, hook.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    model.push_notice(format!("hooks:\n{text}"));
    if let Some(row) = hooks.project {
        review(model, row, false);
    }
}
pub(super) fn handle_key(model: &mut Model, code: KeyCode) -> bool {
    let Some(Picker {
        kind: PickerKind::BackendPlugins(current),
        selected,
    }) = &model.picker
    else {
        return false;
    };
    let (current, selected) = (current.as_ref().clone(), *selected);
    let count = current.labels().len().max(1);
    if let Screen::Discover { rows, query } = &current {
        match code {
            KeyCode::Char(character) => {
                let mut query = query.clone();
                if query.len() < 1024 {
                    query.push(character);
                }
                screen(
                    model,
                    Screen::Discover {
                        rows: rows.clone(),
                        query,
                    },
                );
                return true;
            }
            KeyCode::Backspace => {
                let mut query = query.clone();
                query.pop();
                screen(
                    model,
                    Screen::Discover {
                        rows: rows.clone(),
                        query,
                    },
                );
                return true;
            }
            KeyCode::Esc if !query.is_empty() => {
                screen(
                    model,
                    Screen::Discover {
                        rows: rows.clone(),
                        query: String::new(),
                    },
                );
                return true;
            }
            _ => {}
        }
    }
    match code {
        KeyCode::Up => {
            if let Some(picker) = &mut model.picker {
                picker.selected = selected.checked_sub(1).unwrap_or(count - 1);
            }
        }
        KeyCode::Down => {
            if let Some(picker) = &mut model.picker {
                picker.selected = (selected + 1) % count;
            }
        }
        KeyCode::Esc | KeyCode::Left => match current {
            Screen::List { .. } | Screen::HookEvent | Screen::Grant { review: true, .. } => {
                model.picker = None
            }
            Screen::HookTools(_) => screen(model, Screen::HookEvent),
            _ => open(model),
        },
        KeyCode::Char(' ') => {
            if let Screen::List { plugins, .. } = current
                && let Some(row) = selected.checked_sub(2).and_then(|index| plugins.get(index))
            {
                if row.activation == Activation::Enabled {
                    request(
                        model,
                        Effect::Change(
                            Change::DisablePlugin {
                                id: row.id.clone(),
                                scope: row.scope,
                            },
                            After::Reload(Box::new(After::PluginsAt(row.id.clone()))),
                        ),
                    );
                } else {
                    review(model, row.clone(), false);
                }
            }
        }
        KeyCode::Enter | KeyCode::Right => enter(model, current, selected),
        _ => {}
    }
    model.dirty = true;
    true
}
fn enter(model: &mut Model, current: Screen, selected: usize) {
    model.picker = None;
    match current {
        Screen::List { plugins, notices } => {
            if selected == 0 {
                backend_features::prefill_command(
                    model,
                    "/plugins install ",
                    "Enter owner/repo, name@marketplace or a local folder.",
                );
            } else if selected == 1 {
                request(
                    model,
                    Effect::Read(Read::Marketplaces, After::DiscoverPlugins),
                );
            } else if let Some(row) = selected.checked_sub(2).and_then(|index| plugins.get(index)) {
                model.push_notice(format!(
                    "{} {}\n{}\n{}",
                    row.id,
                    row.version,
                    row.access_description,
                    row.last_failure.clone().unwrap_or_default()
                ));
                screen(model, Screen::Detail(row.clone()));
            } else if selected == plugins.len() + 2 && !notices.is_empty() {
                screen(model, Screen::Notices(notices));
            } else {
                request(model, Effect::Read(Read::PluginDoctor, After::Notice));
            }
        }
        Screen::Discover { rows, query } => {
            let found = matching(&rows, &query);
            if selected == 0 {
                screen(model, Screen::Discover { rows, query });
            } else if let Some(row) = found.get(selected - 1) {
                if row.installed {
                    request(
                        model,
                        Effect::Read(Read::Plugins, After::PluginEnable(row.id.clone())),
                    );
                } else {
                    request(
                        model,
                        Effect::Change(
                            Change::InstallPlugin {
                                source: format!("{}@{}", row.name, row.market),
                            },
                            After::PluginEnable(row.id.clone()),
                        ),
                    );
                }
            } else {
                match selected - found.len() - 1 {
                    0 => backend_features::prefill_command(
                        model,
                        "/plugins marketplace add ",
                        "Enter owner/repo or a repository URL.",
                    ),
                    1 => {
                        request(
                            model,
                            Effect::Change(Change::RefreshMarketplaces, After::DiscoverPlugins),
                        );
                    }
                    _ => open(model),
                }
            }
        }
        Screen::Detail(row) => match actions(&row)
            .into_iter()
            .nth(selected)
            .map(|(_, action)| action)
        {
            Some(Action::Enable) => review(model, row, false),
            Some(Action::Disable) => {
                let after = After::Reload(Box::new(After::PluginsAt(row.id.clone())));
                request(
                    model,
                    Effect::Change(
                        Change::DisablePlugin {
                            id: row.id,
                            scope: row.scope,
                        },
                        after,
                    ),
                );
            }
            Some(Action::Run(id)) => {
                let name = model
                    .remote
                    .as_ref()
                    .and_then(|peer| {
                        peer.plugins
                            .commands
                            .iter()
                            .find(|command| command.action_id == format!("{}/{id}", row.id))
                    })
                    .map(|command| command.name.clone());
                if let Some(name) = name {
                    backend_features::prefill_command(
                        model,
                        &name,
                        "Action placed in composer — add arguments or Enter to send. The backend checks that it is still enabled.",
                    );
                } else {
                    model.push_notice("This action has no slash-command name. Give its action a title to invoke it.");
                }
            }
            Some(Action::Update) => {
                request(
                    model,
                    Effect::Change(
                        Change::UpdatePlugin { id: row.id },
                        After::Reload(Box::new(After::Plugins)),
                    ),
                );
            }
            Some(Action::Rollback) => {
                request(
                    model,
                    Effect::Change(
                        Change::RollbackPlugin { id: row.id },
                        After::Reload(Box::new(After::Plugins)),
                    ),
                );
            }
            Some(Action::Remove) => screen(model, Screen::Remove(row)),
            _ => open(model),
        },
        Screen::Grant { row, grant, review } => {
            let after = After::Reload(Box::new(After::PluginsAt(row.id.clone())));
            if selected == 1 {
                request(
                    model,
                    Effect::Change(
                        Change::EnablePlugin {
                            id: row.id,
                            scope: row.scope,
                            hash: row.hash,
                            grant,
                        },
                        after,
                    ),
                );
            } else if review {
                request(
                    model,
                    Effect::Change(
                        Change::DisablePlugin {
                            id: row.id,
                            scope: row.scope,
                        },
                        after,
                    ),
                );
            } else {
                open(model);
            }
        }
        Screen::Remove(row) => {
            if selected == 1 {
                request(
                    model,
                    Effect::Change(
                        Change::RemovePlugin { id: row.id },
                        After::Reload(Box::new(After::Plugins)),
                    ),
                );
            } else {
                open(model);
            }
        }
        Screen::Notices(_) => open(model),
        Screen::HookEvent => match crate::hook_files::EVENTS.get(selected) {
            Some((event, _, true)) => screen(model, Screen::HookTools((*event).into())),
            Some((event, _, false)) => hook_input(model, event, "*"),
            None => {
                request(model, Effect::Read(Read::Hooks, After::Notice));
            }
        },
        Screen::HookTools(event) => {
            let matcher = crate::hook_files::TOOLS
                .get(selected)
                .map(|(matcher, _)| *matcher)
                .unwrap_or("*");
            hook_input(model, &event, matcher);
        }
    }
}
fn hook_input(model: &mut Model, event: &str, matcher: &str) {
    backend_features::prefill_command(
        model,
        &format!("/hooks add {event} {matcher} "),
        "Enter a shell command or script path. Exit 0 continues; exit 2 blocks and shows stderr.",
    );
}
pub(super) fn hooks_command(model: &mut Model, args: &str) {
    let parts: Vec<_> = args.splitn(4, ' ').collect();
    match parts.as_slice() {
        ["add", event, matcher, command] if !command.trim().is_empty() => { request(model, Effect::Change(Change::AddHook { event: (*event).into(), matcher: (*matcher).into(), command: command.trim().into() }, After::Reload(Box::new(After::Plugins)))); },
        [""] | [] => screen(model, Screen::HookEvent),
        ["remove", event, file] => { request(model, Effect::Change(Change::RemoveHook { event: (*event).into(), file: (*file).into() }, After::Reload(Box::new(After::Plugins)))); },
        _ => model.push_notice("usage: /hooks · /hooks add <event> <tools|*> <command or script> · /hooks remove <event> <file>"),
    }
}
pub(super) fn run_command(model: &mut Model, args: &str) {
    let (verb, rest) = args.split_once(' ').unwrap_or((args, ""));
    let rest = rest.trim();
    let effect = match verb {
        "" => {
            open(model);
            return;
        }
        "install" => Effect::Change(
            Change::InstallPlugin {
                source: rest.into(),
            },
            After::Plugins,
        ),
        "enable" => Effect::Read(Read::Plugins, After::PluginEnable(rest.into())),
        "disable" => Effect::Read(Read::Plugins, After::PluginDisable(rest.into())),
        "remove" => Effect::Read(Read::Plugins, After::PluginRemove(rest.into())),
        "update" => Effect::Change(
            Change::UpdatePlugin { id: rest.into() },
            After::Reload(Box::new(After::Plugins)),
        ),
        "rollback" => Effect::Change(
            Change::RollbackPlugin { id: rest.into() },
            After::Reload(Box::new(After::Plugins)),
        ),
        "doctor" => Effect::Read(Read::PluginDoctor, After::Notice),
        "discover" => Effect::Read(Read::Marketplaces, After::DiscoverPlugins),
        "marketplace" => {
            let (verb, argument) = rest.split_once(' ').unwrap_or((rest, ""));
            match verb {
                "add" => Effect::Change(
                    Change::AddMarketplace {
                        source: argument.into(),
                    },
                    After::DiscoverPlugins,
                ),
                "remove" => Effect::Change(
                    Change::RemoveMarketplace {
                        name: argument.into(),
                    },
                    After::DiscoverPlugins,
                ),
                "refresh" => Effect::Change(Change::RefreshMarketplaces, After::DiscoverPlugins),
                _ => Effect::Read(Read::Marketplaces, After::DiscoverPlugins),
            }
        }
        _ => {
            model.push_notice("usage: /plugins [install|enable|disable|remove|update|rollback|doctor|discover|marketplace]");
            return;
        }
    };
    request(model, effect);
}
