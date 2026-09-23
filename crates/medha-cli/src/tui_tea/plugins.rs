//! `/plugins`: the TUI surface over the same extension store as `medha plugins`.
//!
//! Every write goes through `extensions::Store`, so the TUI cannot grant or
//! enable anything the CLI could not. Changes apply to the running session.

use super::{Model, Picker, PickerKind, TuiEvent};
use crossterm::event::KeyCode;
use extensions::{Activation, ExtensionComponent, Grant, ListedPlugin, Scope, Store};
use tokio::sync::mpsc::UnboundedSender;

#[path = "plugins_commands.rs"]
pub(super) mod commands;
#[path = "plugins_hooks.rs"]
mod hooks;
#[path = "plugins_jobs.rs"]
mod jobs;
pub(super) use hooks::command as hooks_command;
use jobs::DiscoverRow;
pub(super) use jobs::JobDone;
pub(super) use jobs::job_done;

/// Rows above the plugins in the list: install, then discover.
const LIST_HEAD: usize = 2;

#[derive(Debug, Clone)]
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
    /// `review` is the start-up prompt for newly found or changed hook folders.
    Grant {
        row: Row,
        grant: Grant,
        review: bool,
    },
    Remove(Row),
    Notices(Vec<String>),
    /// `/hooks` step 1: when the hook runs.
    HookEvent,
    /// `/hooks` step 2 for tool events: which tools start it.
    HookTools(String),
}

#[derive(Debug, Clone)]
pub(super) struct Row {
    id: String,
    version: String,
    scope: Scope,
    activation: Activation,
    components: Vec<(&'static str, String)>,
    actions: Vec<(String, String)>,
    /// Installed from a repository, so it can be updated.
    updatable: bool,
    can_rollback: bool,
}

impl Row {
    fn from_listing(plugin: &ListedPlugin) -> Self {
        let manifest = &plugin.package.manifest;
        let mut components = Vec::new();
        let mut actions = Vec::new();
        for component in &manifest.components {
            let kind = match component {
                ExtensionComponent::Skill { .. } => "skill",
                ExtensionComponent::Mcp { .. } => "MCP server",
                ExtensionComponent::Hook { .. } => "hook",
                ExtensionComponent::Action { title, prompt, .. } => {
                    actions.push((title.clone(), prompt.clone()));
                    "action"
                }
            };
            components.push((kind, component.id().to_string()));
        }
        Self {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            scope: plugin.scope,
            activation: plugin.activation,
            components,
            actions,
            updatable: false,
            can_rollback: false,
        }
    }

    fn with_history(mut self, store: &Store) -> Self {
        if self.scope == Scope::User {
            let origin = store.origin(&self.id);
            if let Some(origin) = origin.as_ref().filter(|_| self.version == "0.0.0") {
                self.version = format!("@{}", crate::plugins_cmd::short(&origin.commit));
            }
            self.updatable = origin.is_some();
            self.can_rollback = store.can_rollback(&self.id);
        }
        self
    }

    fn marker(&self) -> &'static str {
        match self.activation {
            Activation::Enabled => "●",
            Activation::Disabled => "○",
            Activation::Changed | Activation::Collision | Activation::Shadowed => "⚠",
        }
    }

    fn summary(&self) -> String {
        let mut kinds: Vec<&str> = self.components.iter().map(|(kind, _)| *kind).collect();
        kinds.dedup();
        kinds.join(" · ")
    }
}

impl Screen {
    pub(super) fn title(&self) -> String {
        match self {
            Screen::List { .. } => {
                " plugins — ↑↓ select · Space on/off · Enter details · Esc close ".into()
            }
            Screen::Discover { query, .. } if query.is_empty() => {
                " discover plugins — type to search · ↑↓ select · Enter install · Esc back ".into()
            }
            Screen::Discover { .. } => {
                " discover plugins — ↑↓ select · Enter install · Esc clear search ".into()
            }
            Screen::Detail(row) => format!(" {} — ↑↓ select · Enter · Esc back ", row.id),
            Screen::Grant { review: true, .. } => {
                " allow these hooks? — Enter confirm · Esc ask me next time ".into()
            }
            Screen::Grant { row, .. } => format!(
                " allow {} the access listed above? — Enter confirm · Esc back ",
                row.id
            ),
            Screen::Remove(row) => format!(" remove {}? — Enter confirm · Esc back ", row.id),
            Screen::Notices(_) => " plugin warnings — Esc back ".into(),
            Screen::HookEvent => " add a hook: when should it run? — Enter · Esc cancel ".into(),
            Screen::HookTools(event) => {
                format!(" {event} hook: which tools start it? — Enter · Esc back ")
            }
        }
    }

    pub(super) fn labels(&self) -> Vec<String> {
        match self {
            Screen::List { plugins, notices } => {
                let mut rows = vec![
                    "➕ Install: owner/repo, name@marketplace, or a folder…".to_string(),
                    "🔎 Discover plugins from marketplaces".to_string(),
                ];
                rows.extend(plugins.iter().map(|row| {
                    format!(
                        "{} {}  {}  · {} · {}  — {}",
                        row.marker(),
                        row.id,
                        row.version,
                        row.scope.as_str(),
                        row.activation.as_str(),
                        row.summary()
                    )
                }));
                if !notices.is_empty() {
                    rows.push(format!("⚠ {} warning(s) — view", notices.len()));
                }
                rows.push("🩺 Check that plugins can run".to_string());
                rows
            }
            Screen::Discover { rows, query } => jobs::discover_labels(rows, query),
            Screen::Detail(row) => detail_actions(row)
                .into_iter()
                .map(|(label, _)| label)
                .collect(),
            Screen::Grant { review: true, .. } => vec![
                "Keep off (don't ask again)".to_string(),
                "Allow and turn on".to_string(),
            ],
            Screen::Grant { .. } => vec![
                "Keep disabled".to_string(),
                "Allow this access and enable".to_string(),
            ],
            Screen::Remove(_) => vec!["Keep plugin".to_string(), "Remove plugin".to_string()],
            Screen::Notices(notices) => notices
                .iter()
                .cloned()
                .chain(std::iter::once("← Back".to_string()))
                .collect(),
            Screen::HookEvent => hooks::event_labels(),
            Screen::HookTools(_) => hooks::tool_labels(),
        }
    }
}

#[derive(Debug, Clone)]
enum DetailAction {
    Enable,
    Disable,
    Run(String),
    Update,
    Rollback,
    Remove,
    Back,
}

fn detail_actions(row: &Row) -> Vec<(String, DetailAction)> {
    let mut actions = Vec::new();
    match row.activation {
        Activation::Enabled => actions.push(("Disable".to_string(), DetailAction::Disable)),
        Activation::Disabled => actions.push(("Enable".to_string(), DetailAction::Enable)),
        Activation::Changed => actions.push((
            "Review and enable the changed files".to_string(),
            DetailAction::Enable,
        )),
        Activation::Collision | Activation::Shadowed => {}
    }
    if row.activation == Activation::Enabled {
        for (title, prompt) in &row.actions {
            actions.push((
                format!("▶ Run action: {title}"),
                DetailAction::Run(prompt.clone()),
            ));
        }
    }
    if row.updatable {
        actions.push((
            "⟳ Update to the newest version".to_string(),
            DetailAction::Update,
        ));
    }
    if row.can_rollback {
        actions.push((
            "↶ Roll back to the previous version".to_string(),
            DetailAction::Rollback,
        ));
    }
    if row.scope == Scope::User {
        actions.push(("Remove".to_string(), DetailAction::Remove));
    }
    actions.push(("← Back".to_string(), DetailAction::Back));
    actions
}

pub(super) fn open(model: &mut Model) {
    reopen(model, None);
}

/// Rebuilds the list from disk, keeping the cursor on `focus` when it is listed.
fn reopen(model: &mut Model, focus: Option<(&str, Scope)>) {
    let Some(store) = model.plugins.clone() else {
        return model.push_notice("plugins are unavailable in this session");
    };
    match store.discover() {
        Ok(discovery) => {
            let notices = discovery.notices();
            let plugins: Vec<Row> = discovery
                .plugins
                .iter()
                .map(|plugin| Row::from_listing(plugin).with_history(&store))
                .collect();
            let selected = focus
                .and_then(|(id, scope)| {
                    plugins
                        .iter()
                        .position(|row| row.id == id && row.scope == scope)
                })
                .map_or(0, |index| index + LIST_HEAD);
            model.picker = Some(Picker::with_selected(
                PickerKind::Plugins(Screen::List { plugins, notices }),
                selected,
            ));
        }
        Err(error) => model.push_notice(format!("plugins: {error}")),
    }
}

/// Handles every key the plugin screens own; ↑/↓ fall through to the picker.
pub(super) fn handle_key(model: &mut Model, code: KeyCode, tx: &UnboundedSender<TuiEvent>) -> bool {
    let Some(picker) = model.picker.as_ref() else {
        return false;
    };
    let PickerKind::Plugins(screen) = &picker.kind else {
        return false;
    };
    let (screen, selected) = (screen.clone(), picker.selected);
    if let Screen::Discover { rows, mut query } = screen.clone() {
        match code {
            KeyCode::Char(c) => query.push(c),
            KeyCode::Backspace if !query.is_empty() => {
                query.pop();
            }
            KeyCode::Esc if !query.is_empty() => query.clear(),
            _ => return handle_screen_key(model, code, screen, selected, tx),
        }
        let first = usize::from(!jobs::matching(&rows, &query).is_empty());
        model.picker = Some(Picker {
            kind: PickerKind::Plugins(Screen::Discover { rows, query }),
            selected: first,
        });
        return true;
    }
    handle_screen_key(model, code, screen, selected, tx)
}

fn handle_screen_key(
    model: &mut Model,
    code: KeyCode,
    screen: Screen,
    selected: usize,
    tx: &UnboundedSender<TuiEvent>,
) -> bool {
    match code {
        KeyCode::Esc | KeyCode::Left => {
            model.picker = None;
            match screen {
                Screen::List { .. } | Screen::Grant { review: true, .. } | Screen::HookEvent => {}
                Screen::HookTools(_) => hooks::open(model),
                _ => open(model),
            }
            true
        }
        KeyCode::Char(' ') => {
            if let Screen::List { plugins, .. } = &screen
                && let Some(row) = selected
                    .checked_sub(LIST_HEAD)
                    .and_then(|index| plugins.get(index))
            {
                let row = row.clone();
                if row.activation == Activation::Enabled {
                    disable(model, &row);
                } else {
                    begin_enable(model, &row);
                }
            }
            true
        }
        KeyCode::Enter | KeyCode::Right => {
            enter(model, screen, selected, tx);
            true
        }
        _ => false,
    }
}

fn enter(model: &mut Model, screen: Screen, selected: usize, tx: &UnboundedSender<TuiEvent>) {
    model.picker = None;
    match screen {
        Screen::List { plugins, notices } => {
            let warnings_row = LIST_HEAD + plugins.len();
            if selected == 0 {
                model.input = "/plugins install ".into();
                model.cursor = model.input.len();
                model.push_notice(
                    "type owner/repo, a GitHub link, name@marketplace, or a folder — then Enter",
                );
            } else if selected == 1 {
                jobs::open_discover(model, tx);
            } else if let Some(row) = plugins.get(selected - LIST_HEAD) {
                show_details(model, row);
                model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Detail(
                    row.clone(),
                ))));
            } else if selected == warnings_row && !notices.is_empty() {
                model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Notices(notices))));
            } else {
                jobs::doctor(model);
            }
        }
        Screen::Discover { rows, query } => {
            jobs::enter_discover(model, &rows, &query, selected, tx)
        }
        Screen::Detail(row) => match detail_actions(&row).into_iter().nth(selected) {
            Some((_, DetailAction::Enable)) => begin_enable(model, &row),
            Some((_, DetailAction::Disable)) => disable(model, &row),
            Some((_, DetailAction::Run(prompt))) => {
                model.input = prompt;
                model.cursor = model.input.len();
                model.push_notice(format!(
                    "{}: action placed in the composer — edit it or press Enter to send",
                    row.id
                ));
            }
            Some((_, DetailAction::Update)) => jobs::update(model, &row.id, tx),
            Some((_, DetailAction::Rollback)) => jobs::rollback(model, &row.id),
            Some((_, DetailAction::Remove)) => {
                model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Remove(row))));
            }
            Some((_, DetailAction::Back)) | None => open(model),
        },
        Screen::Grant { row, grant, review } => {
            if selected == 1 {
                apply_enable(model, &row, &grant);
            } else if review {
                // Recording the choice is what stops the prompt from returning.
                let _ = with_store(model, |store| store.disable(&row.id, Some(row.scope)));
                model.push_notice(format!(
                    "{} stays off — turn it on any time in /plugins",
                    row.id
                ));
            } else {
                model.push_notice(format!("{} stays disabled", row.id));
            }
            if review {
                review_hooks(model);
            } else {
                reopen(model, Some((&row.id, row.scope)));
            }
        }
        Screen::Remove(row) => {
            if selected == 1 {
                let outcome = with_store(model, |store| store.remove(&row.id));
                match outcome {
                    Ok(()) => {
                        apply_plugins(model);
                        model.push_notice(format!("removed {}", row.id));
                    }
                    Err(error) => model.push_notice(error),
                }
            }
            open(model);
        }
        Screen::Notices(_) => open(model),
        Screen::HookEvent => hooks::choose_event(model, selected),
        Screen::HookTools(event) => hooks::choose_tools(model, &event, selected),
    }
}

/// `/plugins [install|enable|disable|remove|update|rollback|doctor|discover|marketplace]`.
pub(super) fn run_command(model: &mut Model, args: &str, tx: &UnboundedSender<TuiEvent>) {
    let (verb, rest) = args.split_once(' ').unwrap_or((args, ""));
    let rest = rest.trim();
    match (verb, rest) {
        ("", _) => open(model),
        ("install", spec) if !spec.is_empty() => jobs::install(model, spec, tx),
        ("update", id) if !id.is_empty() => jobs::update(model, id, tx),
        ("rollback", id) if !id.is_empty() => jobs::rollback(model, id),
        ("doctor", _) => jobs::doctor(model),
        ("discover", _) => jobs::open_discover(model, tx),
        ("marketplace", rest) => jobs::marketplace(model, rest, tx),
        ("enable" | "disable" | "remove", id) if !id.is_empty() => {
            let found = with_store(model, |store| store.inspect(id, None));
            let row = match found {
                Ok(plugin) => Row::from_listing(&plugin),
                Err(error) => return model.push_notice(error),
            };
            match verb {
                "enable" => begin_enable(model, &row),
                "disable" => disable(model, &row),
                _ => model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Remove(row)))),
            }
        }
        _ => model.push_notice(
            "usage: /plugins · install <owner/repo | name@marketplace | folder> · \
             enable|disable|remove|update|rollback <id> · discover · doctor · \
             marketplace add|remove|refresh <…>",
        ),
    }
}

fn begin_enable(model: &mut Model, row: &Row) {
    let plugin = match with_store(model, |store| store.inspect(&row.id, Some(row.scope))) {
        Ok(plugin) => plugin,
        Err(error) => return model.push_notice(error),
    };
    let grant = match plugin.requested_grant() {
        Ok(grant) => grant,
        Err(error) => return model.push_notice(format!("{}: {error}", row.id)),
    };
    if grant.is_empty() {
        return enable(model, row, &grant);
    }
    model.push_notice(access_text(&row.id, &plugin, &grant));
    model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Grant {
        row: row.clone(),
        grant,
        review: false,
    })));
}

/// Start-up prompt: one keypress per hook folder that is new or changed, asked
/// once. Installed plugins are not included; installing them was the decision.
pub(super) fn review_hooks(model: &mut Model) {
    model.picker = None;
    let pending = with_store(model, |store| store.needs_review()).unwrap_or_default();
    let Some(plugin) = pending.first() else {
        return;
    };
    let row = Row::from_listing(plugin);
    let grant = match plugin.requested_grant() {
        Ok(grant) => grant,
        Err(error) => {
            let _ = with_store(model, |store| store.disable(&row.id, Some(row.scope)));
            return model.push_notice(format!("{}: {error}", row.id));
        }
    };
    let state = if plugin.activation == Activation::Changed {
        "changed since you allowed them"
    } else {
        "were found"
    };
    let mut text = format!(
        "{}: hooks {state} in {}",
        row.id,
        plugin.package.root.display()
    );
    for component in &plugin.package.manifest.components {
        if let ExtensionComponent::Hook {
            id,
            points,
            matcher,
            ..
        } = component
        {
            let points: Vec<&str> = points.iter().map(|point| point.as_str()).collect();
            let tools = if matcher.is_empty() {
                "all tools".to_string()
            } else {
                matcher.join(", ")
            };
            text.push_str(&format!("\n  • {id} — {} · {tools}", points.join(", ")));
        }
    }
    if !grant.is_empty() {
        text.push('\n');
        text.push_str(&access_lines(plugin, &grant));
    }
    text.push_str("\n  Runs sandboxed; editing these files later asks again.");
    model.push_notice(text);
    model.picker = Some(Picker::new(PickerKind::Plugins(Screen::Grant {
        row,
        grant,
        review: true,
    })));
}

fn enable(model: &mut Model, row: &Row, grant: &Grant) {
    apply_enable(model, row, grant);
    reopen(model, Some((&row.id, row.scope)));
}

fn apply_enable(model: &mut Model, row: &Row, grant: &Grant) {
    match with_store(model, |store| store.enable(&row.id, Some(row.scope), grant)) {
        Ok(_) => {
            apply_plugins(model);
            model.push_notice(format!("enabled {} — active now", row.id));
        }
        Err(error) => model.push_notice(error),
    }
}

fn disable(model: &mut Model, row: &Row) {
    match with_store(model, |store| store.disable(&row.id, Some(row.scope))) {
        Ok(_) => {
            apply_plugins(model);
            model.push_notice(format!("disabled {} — stopped now", row.id));
        }
        Err(error) => model.push_notice(error),
    }
    reopen(model, Some((&row.id, row.scope)));
}

pub(super) fn refresh_commands(model: &mut Model) {
    let builtin: Vec<&str> = super::COMMANDS.iter().map(|(name, _)| *name).collect();
    model.plugin_commands = model
        .plugins
        .as_ref()
        .map(|store| commands::load(store, &builtin))
        .unwrap_or_default();
}

fn apply_plugins(model: &mut Model) {
    refresh_commands(model);
    let Some(apply) = model.apply_plugins.clone() else {
        return;
    };
    model.plugins_changed = true;
    for warning in apply() {
        model.push_notice(format!("plugin: {warning}"));
    }
}

fn show_details(model: &mut Model, row: &Row) {
    let mut text = format!(
        "plugin {} {} · {} · {}",
        row.id,
        row.version,
        row.scope.as_str(),
        row.activation.as_str()
    );
    for (kind, id) in &row.components {
        text.push_str(&format!("\n  {kind:<10} {id}"));
    }
    if let Ok(plugin) = with_store(model, |store| store.inspect(&row.id, Some(row.scope))) {
        match plugin.requested_grant() {
            Ok(grant) if grant.is_empty() => text.push_str("\n  access: none requested"),
            Ok(grant) => {
                text.push('\n');
                text.push_str(&access_lines(&plugin, &grant));
            }
            Err(error) => text.push_str(&format!("\n  access: cannot be granted — {error}")),
        }
    }
    model.push_notice(text);
}

fn access_text(id: &str, plugin: &ListedPlugin, grant: &Grant) -> String {
    format!("{id} requests:\n{}", access_lines(plugin, grant))
}

fn access_lines(plugin: &ListedPlugin, grant: &Grant) -> String {
    let mut lines = Vec::new();
    if grant.network {
        lines.push(format!(
            "  network: allowed for this plugin's processes (asked for {}; hosts cannot be \
             limited individually)",
            plugin.package.manifest.permissions.network_hosts.join(", ")
        ));
    }
    if !grant.read_paths.is_empty() {
        lines.push(format!(
            "  read in workspace: {}",
            grant.read_paths.join(", ")
        ));
    }
    if !grant.write_paths.is_empty() {
        lines.push(format!(
            "  write in workspace: {}",
            grant.write_paths.join(", ")
        ));
    }
    lines.join("\n")
}

fn with_store<T>(
    model: &Model,
    operation: impl FnOnce(&Store) -> Result<T, extensions::Error>,
) -> Result<T, String> {
    let store = model
        .plugins
        .as_ref()
        .ok_or_else(|| "plugins are unavailable in this session".to_string())?;
    operation(store).map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
