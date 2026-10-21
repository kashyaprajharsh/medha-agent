//! Connect a model (screens T1 to T5 of the approved terminal design): where a model comes
//! from, which provider a pasted key is for, a provider's key and models, and ChatGPT sign-in.
//! The screens live inside the model setup and drive its steps, so the backend calls are the
//! same ones every surface makes.

use super::*;
use crate::tui_tea::backend_ui::{ChatGptAnswer, ChatGptCall, Effect, request};
use crate::tui_tea::input::picker::models::{choose_provider, choose_saved};

/// Where OpenAI lets a person see and manage their plan's usage.
pub(super) const MANAGE_USAGE: &str = "https://chatgpt.com/settings/usage";
/// Rows of the API-keys group shown before the person searches.
const FIRST_KEYED: usize = 4;

pub(super) enum Screen {
    /// T1, and T3 while the search line holds a key.
    List(List),
    /// T2: the chosen provider's key and models, on the setup's own steps.
    Provider(Provider),
    /// T4: waiting for the browser to come back from ChatGPT.
    Waiting { link: String, id: u64, again: bool },
    /// Signed in, but plan use was not allowed.
    PlanOff { account: protocol::ChatGptAccount },
    /// T5: the plan's models.
    Plan(Plan),
    /// The signed-in ChatGPT accounts: use one, sign one out or in, or add another (`/chatgpt`).
    Accounts {
        accounts: Vec<protocol::ChatGptAccount>,
        selected: usize,
        loading: bool,
    },
}

#[derive(Default)]
pub(super) struct List {
    pub(super) query: String,
    pub(super) selected: usize,
    /// Local servers looked for when the list opened: `Some(n)` running with n models.
    pub(super) running: BTreeMap<String, Option<usize>>,
    /// The backend's reading of the key in the search line, with that key.
    pub(super) recognised: Option<(String, protocol::RecognisedKey)>,
    /// A key carried to whichever provider is chosen next.
    pub(super) carried: Option<String>,
    /// Showing the providers Medha cannot connect yet.
    pub(super) unavailable: bool,
    pub(super) opening: bool,
    /// When the search line last changed by a keystroke, and the key last sent to be read.
    edited: Option<Instant>,
    asked: Option<String>,
    /// Local servers still to look for, one at a time.
    probes: VecDeque<(String, protocol::DiscoverModels)>,
}

/// How long typing pauses before a typed key is read; a pasted one is read at once.
const PAUSE: Duration = Duration::from_millis(300);

#[derive(Default)]
pub(super) struct Provider {
    pub(super) found: Vec<protocol::DiscoveredModel>,
    pub(super) selected: usize,
    /// The model filter while it is being typed.
    pub(super) filter: Option<String>,
    pub(super) tested: Option<protocol::ModelTestResult>,
    /// A pasted key waiting for the setup to reach its key step.
    pub(super) key: Option<String>,
}

pub(super) struct Plan {
    pub(super) account: protocol::ChatGptAccount,
    pub(super) models: Vec<protocol::DiscoveredModel>,
    pub(super) selected: usize,
    pub(super) loading: bool,
    pub(super) tested: Option<protocol::ModelTestResult>,
}

/// One line of the T1 list; groups and the "more" line cannot be chosen.
pub(super) enum Row<'a> {
    Group(&'static str),
    ChatGpt,
    Keyed {
        provider: &'a protocol::ModelProvider,
        saved: bool,
    },
    More(usize),
    Local {
        provider: &'a protocol::ModelProvider,
        running: Option<usize>,
    },
    Own(&'a protocol::ModelProfile),
    Another,
    Owner(&'a protocol::KeyOwner),
    AnyOwner(usize),
    Unavailable(&'a protocol::ModelProvider),
}

impl Row<'_> {
    pub(super) fn choosable(&self) -> bool {
        !matches!(self, Row::Group(_) | Row::More(_) | Row::Unavailable(_))
    }

    /// The name the row shows first.
    #[cfg(test)]
    pub(super) fn name(&self) -> String {
        match self {
            Row::Group(title) => (*title).to_owned(),
            Row::ChatGpt => "ChatGPT".into(),
            Row::Keyed { provider, .. }
            | Row::Local { provider, .. }
            | Row::Unavailable(provider) => provider.name.clone(),
            Row::More(n) => format!("… {n} more"),
            Row::Own(profile) => host_of(&profile.base_url),
            Row::Another => "Another server".into(),
            Row::Owner(owner) => owner.name.clone(),
            Row::AnyOwner(_) => "Another provider".into(),
        }
    }
}

/// Text that reads as a pasted key rather than a search: the same rule the desktop list uses.
pub(super) fn looks_like_key(text: &str) -> bool {
    text.chars().count() >= 16
        && !text.chars().any(char::is_whitespace)
        && text.chars().any(|c| c.is_ascii_digit())
        && text.chars().any(|c| c.is_ascii_alphabetic())
}

/// A server on this computer answers without a key, so it is looked for unasked.
pub(super) fn is_local(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
}

/// "1.05M" or "400K", as the desktop shows a context size.
pub(super) fn context_short(tokens: Option<u32>) -> String {
    match tokens {
        None | Some(0) => String::new(),
        Some(n) if n >= 1_000_000 => {
            let millions = format!("{:.2}", f64::from(n) / 1_000_000.0);
            format!("{}M", millions.trim_end_matches('0').trim_end_matches('.'))
        }
        Some(n) => format!("{}K", (f64::from(n) / 1000.0).round()),
    }
}

/// "$2 / $10" or "$0.10 / $0.50" per million tokens in and out; empty without a list price.
pub(super) fn price_label(model: &protocol::DiscoveredModel) -> String {
    let dollars = |value: f64| {
        if value.fract() == 0.0 {
            format!("${value:.0}")
        } else {
            format!("${value:.2}")
        }
    };
    match (model.input_per_mtok, model.output_per_mtok) {
        (Some(input), Some(output)) => format!("{} / {}", dollars(input), dollars(output)),
        _ => String::new(),
    }
}

/// A key as it may be shown: its opening letters and last four, never the middle.
pub(super) fn masked(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}{}{tail}", "•".repeat((chars.len() - 8).min(16)))
}

pub(super) fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_owned();
            Some(
                url.port()
                    .map_or(host.clone(), |port| format!("{host}:{port}")),
            )
        })
        .unwrap_or_else(|| url.to_owned())
}

fn catalogue(model: &Model) -> (&[protocol::ModelProvider], &[protocol::ModelProfile]) {
    model.remote.as_ref().map_or((&[], &[]), |peer| {
        (&peer.models.providers[..], &peer.models.profiles[..])
    })
}

fn key_saved_at(profiles: &[protocol::ModelProfile], url: Option<&str>) -> bool {
    url.is_some_and(|url| profiles.iter().any(|p| p.key_present && p.base_url == url))
}

/// The T1 (or T3) rows for what the list holds now.
pub(super) fn rows<'a>(model: &'a Model, list: &'a List) -> Vec<Row<'a>> {
    let (providers, profiles) = catalogue(model);
    let mut rows = Vec::new();
    if list.unavailable {
        rows.push(Row::Group("Not available yet"));
        rows.extend(
            providers
                .iter()
                .filter(|p| !p.available)
                .map(Row::Unavailable),
        );
        return rows;
    }
    let query = list.query.trim();
    if let Some((key, found)) = &list.recognised
        && key == query
        && found.matched != protocol::KeyMatch::One
    {
        rows.extend(found.providers.iter().map(Row::Owner));
        rows.push(Row::AnyOwner(
            providers.iter().filter(|p| p.available).count(),
        ));
        return rows;
    }
    let searching = !query.is_empty() && !looks_like_key(query);
    let wanted = query.to_lowercase();
    let shows = |name: &str, id: &str| {
        !searching || name.to_lowercase().contains(&wanted) || id.contains(&wanted)
    };
    if shows("ChatGPT plan", "chatgpt") {
        rows.push(Row::Group("Plans"));
        rows.push(Row::ChatGpt);
    }
    let usable = providers
        .iter()
        .filter(|p| p.available && shows(&p.name, &p.id));
    let (local, keyed): (Vec<_>, Vec<_>) =
        usable.partition(|p| p.url.as_deref().is_some_and(is_local));
    if !keyed.is_empty() {
        rows.push(Row::Group("API keys"));
        let shown = if searching {
            keyed.len()
        } else {
            keyed.len().min(FIRST_KEYED)
        };
        rows.extend(keyed.iter().take(shown).map(|provider| Row::Keyed {
            provider,
            saved: key_saved_at(profiles, provider.url.as_deref()),
        }));
        if shown < keyed.len() {
            rows.push(Row::More(keyed.len() - shown));
        }
    }
    // A server of one's own that no catalogue provider covers, offered again by its address.
    let mut own: Vec<&protocol::ModelProfile> = Vec::new();
    for profile in profiles {
        let covered = providers
            .iter()
            .any(|p| p.url.as_deref() == Some(&profile.base_url));
        let listed = own.iter().any(|o| o.base_url == profile.base_url);
        if !covered && !listed && shows(&profile.base_url, &profile.name) {
            own.push(profile);
        }
    }
    let another = shows("Another server", "custom");
    if !local.is_empty() || !own.is_empty() || another {
        rows.push(Row::Group("This computer"));
        rows.extend(local.into_iter().map(|provider| {
            Row::Local {
                provider,
                running: provider
                    .url
                    .as_ref()
                    .and_then(|url| list.running.get(url).copied().flatten()),
            }
        }));
        rows.extend(own.into_iter().map(Row::Own));
        if another {
            rows.push(Row::Another);
        }
    }
    rows
}

fn list_mut(model: &mut Model) -> Option<&mut List> {
    match model.model_setup.as_mut()?.screen.as_mut()? {
        Screen::List(list) => Some(list),
        _ => None,
    }
}

/// Open T1 on a fresh setup, and look for servers already running on this computer.
pub(super) fn open(model: &mut Model) {
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    setup.screen = Some(Screen::List(List::default()));
    model.picker = None;
    let (providers, _) = catalogue(model);
    let local: VecDeque<_> = providers
        .iter()
        .filter(|p| p.available && p.auth == protocol::ModelAuth::None)
        .filter_map(|p| {
            let url = p.url.clone().filter(|url| is_local(url))?;
            let probe = protocol::DiscoverModels {
                provider_id: Some(p.id.clone()),
                auth: Some(protocol::ModelAuth::None),
                connection: Default::default(),
                protocol: p.protocol.unwrap_or(protocol::ModelProtocol::OpenAiChat),
                base_url: url.clone(),
                key: protocol::Secret(String::new()),
                allow_other_provider: false,
            };
            Some((url, probe))
        })
        .collect();
    list_mut(model).expect("list").probes = local;
    probe_next(model);
}

/// Look for one local server at a time, so a slot is always free for what the person does.
fn probe_next(model: &mut Model) {
    let Some((url, probe)) = list_mut(model).and_then(|list| list.probes.pop_front()) else {
        return;
    };
    request(model, Effect::Probe { url, probe });
}

/// Start a ChatGPT sign-in on a fresh setup, without looking for local servers first.
fn sign_in(model: &mut Model, client_id: Option<String>, consent: bool) {
    model.form_generation = model.form_generation.wrapping_add(1);
    model.picker = None;
    let mut setup = ModelSetup::new();
    setup.screen = Some(Screen::List(List::default()));
    model.model_setup = Some(setup);
    begin_chatgpt(model, client_id, consent);
}

/// Keys while a connect screen is up; `false` lets the setup's own steps take the key.
pub(super) fn handle_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(screen) = model.model_setup.as_ref().and_then(|s| s.screen.as_ref()) else {
        return false;
    };
    match screen {
        Screen::List(_) => list_key(model, key),
        Screen::Provider(_) => provider_key(model, key),
        Screen::Waiting { .. } => waiting_key(model, key),
        Screen::PlanOff { .. } => plan_off_key(model, key),
        Screen::Plan(_) => plan_key(model, key),
        Screen::Accounts { .. } => accounts_key(model, key),
    }
}

/// `/chatgpt`: the accounts, as the desktop's ChatGPT accounts sheet shows them.
pub(super) fn open_accounts(model: &mut Model) {
    if !backend_features::boundary_allowed(model) {
        return;
    }
    model.form_generation = model.form_generation.wrapping_add(1);
    model.picker = None;
    let mut setup = ModelSetup::new();
    setup.screen = Some(Screen::Accounts {
        accounts: Vec::new(),
        selected: 0,
        loading: true,
    });
    model.model_setup = Some(setup);
    request(model, Effect::ChatGpt(ChatGptCall::Accounts));
}

fn accounts_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Screen::Accounts {
        accounts,
        selected,
        loading,
    }) = model.model_setup.as_mut().and_then(|s| s.screen.as_mut())
    else {
        return true;
    };
    if *loading && key.code != KeyCode::Esc {
        return true;
    }
    let picked = accounts.get(*selected).cloned();
    let last = accounts.len().saturating_sub(1);
    match key.code {
        KeyCode::Up => *selected = selected.saturating_sub(1),
        KeyCode::Down => *selected = (*selected + 1).min(last),
        KeyCode::Esc => {
            model.form_generation = model.form_generation.wrapping_add(1);
            model.model_setup = None;
        }
        KeyCode::Char('n') => sign_in(model, None, false),
        KeyCode::Enter => match picked {
            Some(account) if !account.signed_in => sign_in(model, Some(account.client_id), false),
            Some(account) if !account.active => {
                *loading = true;
                let switch = protocol::SwitchChatGptAccount {
                    client_id: account.client_id,
                };
                request(model, Effect::ChatGpt(ChatGptCall::Switch(switch)));
            }
            _ => {}
        },
        KeyCode::Char('o') => {
            if let Some(account) = picked.filter(|a| a.signed_in) {
                *loading = true;
                let out = protocol::SignOutChatGpt {
                    client_id: account.client_id,
                };
                request(model, Effect::ChatGpt(ChatGptCall::SignOut(out)));
            }
        }
        _ => {}
    }
    true
}

/// Leave adding a model: back to the model menu when there are models, else simply close.
fn close(model: &mut Model) {
    model.form_generation = model.form_generation.wrapping_add(1);
    model.model_setup = None;
    model.picker = None;
    let has_models = model
        .remote
        .as_ref()
        .is_some_and(|peer| !peer.models.profiles.is_empty());
    if has_models {
        backend_features::open_model_picker(model);
    }
}

fn move_in(rows: &[Row<'_>], selected: usize, down: bool) -> usize {
    let choosable: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].choosable()).collect();
    let at = choosable.iter().position(|&i| i >= selected).unwrap_or(0);
    let next = if down {
        (at + 1).min(choosable.len().saturating_sub(1))
    } else {
        at.saturating_sub(1)
    };
    choosable.get(next).copied().unwrap_or(0)
}

/// The first row that can be chosen, at or after `selected`.
pub(super) fn settled_row(rows: &[Row<'_>], selected: usize) -> usize {
    (selected..rows.len())
        .chain(0..selected)
        .find(|&i| rows.get(i).is_some_and(Row::choosable))
        .unwrap_or(0)
}

fn list_key(model: &mut Model, key: KeyEvent) -> bool {
    let list = list_mut(model).expect("list screen");
    if list.opening {
        if key.code == KeyCode::Esc {
            close(model);
        }
        return true;
    }
    match key.code {
        KeyCode::Esc if list.unavailable => list.unavailable = false,
        KeyCode::Esc if !list.query.is_empty() => {
            list.query.clear();
            list.recognised = None;
            list.selected = 0;
        }
        KeyCode::Esc => close(model),
        KeyCode::Up | KeyCode::Down => {
            let down = key.code == KeyCode::Down;
            let list = model.model_setup.as_ref().and_then(|s| s.screen.as_ref());
            let Some(Screen::List(list)) = list else {
                return true;
            };
            let rows = rows(model, list);
            let next = move_in(&rows, settled_row(&rows, list.selected), down);
            list_mut(model).expect("list").selected = next;
        }
        KeyCode::Enter => choose_row(model),
        KeyCode::Backspace => {
            list.query.pop();
            list.selected = 0;
            list.edited = Some(Instant::now());
        }
        KeyCode::Char('?') if list.query.is_empty() => list.unavailable = !list.unavailable,
        // The catalogue refreshes itself weekly; this asks for it now.
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            request(model, Effect::RefreshProviders);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            list.query.push(c);
            list.selected = 0;
            list.edited = Some(Instant::now());
        }
        _ => {}
    }
    true
}

/// Text that reads as a key goes to the backend once, which names its provider.
fn ask_about_key(model: &mut Model) {
    let Some(list) = list_mut(model) else { return };
    let query = list.query.trim().to_owned();
    if !looks_like_key(&query) || list.asked.as_deref() == Some(query.as_str()) {
        return;
    }
    list.asked = Some(query.clone());
    if !request(
        model,
        Effect::Recognise(protocol::RecogniseKey {
            key: protocol::Secret(query),
        }),
    ) && let Some(list) = list_mut(model)
    {
        list.asked = None;
    }
}

/// A key typed by hand is read once typing pauses, not at every keystroke.
pub(super) fn tick(model: &mut Model) {
    let paused = list_mut(model)
        .and_then(|list| list.edited)
        .is_some_and(|at| at.elapsed() >= PAUSE);
    if paused {
        list_mut(model).expect("list").edited = None;
        ask_about_key(model);
    }
}

/// Pasted text goes to the line the screen is on; `false` leaves it to the setup's own steps.
pub(super) fn paste(model: &mut Model, text: &str) -> bool {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    match model.model_setup.as_mut().and_then(|s| s.screen.as_mut()) {
        Some(Screen::List(list)) => {
            for c in clean.chars() {
                if list.query.len() + c.len_utf8() > 256 {
                    break;
                }
                list.query.push(c);
            }
            list.selected = 0;
            list.edited = None;
            ask_about_key(model);
            true
        }
        Some(Screen::Provider(provider)) if provider.filter.is_some() => {
            provider.filter.as_mut().expect("filter").push_str(&clean);
            provider.selected = 0;
            true
        }
        Some(Screen::Provider(_)) | None => false,
        Some(_) => true,
    }
}

/// The backend named a key's provider: one owner opens it, several ask which.
pub(super) fn recognised(model: &mut Model, key: String, found: protocol::RecognisedKey) {
    let Some(list) = list_mut(model) else { return };
    if list.query.trim() != key {
        return;
    }
    if let Some(warning) = &found.warning {
        model.push_notice(warning.clone());
    }
    let list = list_mut(model).expect("list");
    list.selected = 0;
    if found.matched == protocol::KeyMatch::One
        && let Some(owner) = found.providers.first().cloned()
    {
        list.recognised = Some((key.clone(), found));
        choose_owner(model, &owner.id, key);
        return;
    }
    list.recognised = Some((key, found));
}

fn choose_owner(model: &mut Model, id: &str, key: String) {
    let (providers, _) = catalogue(model);
    let Some(provider) = providers.iter().find(|p| p.id == id).cloned() else {
        model.push_notice("That provider is not in this catalogue; choose it from the list.");
        return;
    };
    open_provider(model, Some(provider), Some(key));
}

fn choose_row(model: &mut Model) {
    let Some(Screen::List(list)) = model.model_setup.as_ref().and_then(|s| s.screen.as_ref())
    else {
        return;
    };
    let rows = rows(model, list);
    let carried = list.carried.clone().or_else(|| {
        let query = list.query.trim();
        looks_like_key(query).then(|| query.to_owned())
    });
    enum Choice {
        ChatGpt,
        Provider(protocol::ModelProvider),
        Owner(String),
        Own(protocol::ModelProfile),
        Another,
        AnyOwner,
    }
    let choice = match rows.get(settled_row(&rows, list.selected)) {
        Some(Row::ChatGpt) => Choice::ChatGpt,
        Some(Row::Keyed { provider, .. } | Row::Local { provider, .. }) => {
            Choice::Provider((*provider).clone())
        }
        Some(Row::Owner(owner)) => Choice::Owner(owner.id.clone()),
        Some(Row::Own(profile)) => Choice::Own((*profile).clone()),
        Some(Row::Another) => Choice::Another,
        Some(Row::AnyOwner(_)) => Choice::AnyOwner,
        _ => return,
    };
    match choice {
        Choice::ChatGpt => begin_chatgpt(model, None, false),
        Choice::Provider(provider) => open_provider(model, Some(provider), carried),
        Choice::Owner(id) => choose_owner(model, &id, carried.unwrap_or_default()),
        Choice::Own(profile) => {
            let (providers, _) = catalogue(model);
            let providers = providers.to_vec();
            to_provider(model, carried);
            choose_saved(model, profile, &providers);
            settle(model);
        }
        Choice::Another => open_provider(model, None, carried),
        Choice::AnyOwner => {
            // Keep the key; the person picks its provider from the whole list.
            let list = list_mut(model).expect("list");
            list.carried = carried;
            list.query.clear();
            list.recognised = None;
            list.selected = 0;
        }
    }
}

fn to_provider(model: &mut Model, key: Option<String>) {
    if let Some(setup) = model.model_setup.as_mut() {
        setup.screen = Some(Screen::Provider(Provider {
            key: key.filter(|key| !key.is_empty()),
            ..Provider::default()
        }));
        setup.editor = Editor::default();
    }
}

fn open_provider(
    model: &mut Model,
    provider: Option<protocol::ModelProvider>,
    key: Option<String>,
) {
    if provider.as_ref().is_some_and(|p| !p.available) {
        choose_provider(model, provider);
        return;
    }
    to_provider(model, key);
    choose_provider(model, provider);
    settle(model);
}

fn provider_mut(model: &mut Model) -> Option<&mut Provider> {
    match model.model_setup.as_mut()?.screen.as_mut()? {
        Screen::Provider(provider) => Some(provider),
        _ => None,
    }
}

/// Hand a carried key to the setup once it asks for one, so it is checked like a typed key.
pub(super) fn settle(model: &mut Model) {
    let at_key = model
        .model_setup
        .as_ref()
        .is_some_and(|s| matches!(s.step, ModelSetupStep::ApiKey));
    let Some(key) = provider_mut(model).and_then(|p| p.key.take()) else {
        return;
    };
    if !at_key {
        provider_mut(model).expect("provider").key = Some(key);
        return;
    }
    if let Some(setup) = model.model_setup.as_mut() {
        setup.editor.cursor = key.len();
        setup.editor.text = key;
    }
    backend_features::advance_model_setup(model);
}

/// Discovery answered: the server's models, or the catalogue's with the reason it stood in.
pub(super) fn discovered(model: &mut Model, listing: protocol::ModelListing) {
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    let protocol::ModelListing { models, unverified } = listing;
    match setup.screen.as_mut() {
        Some(Screen::Provider(provider)) => {
            setup.step = ModelSetupStep::ModelId;
            setup.editor = Editor::default();
            provider.found = models;
            provider.selected = 0;
            provider.filter = None;
            let empty = provider.found.is_empty();
            if let Some(why) = unverified {
                model.push_notice(format!("Live discovery failed: {why}. Catalogue models can still be chosen; availability for this account is unverified. A manual ID is also accepted."));
            } else if empty {
                model.push_notice("The server listed no models. Type the model ID it uses.");
            }
        }
        Some(Screen::Plan(plan)) => {
            setup.step = ModelSetupStep::ModelId;
            plan.models = models;
            plan.loading = false;
            plan.selected = 0;
        }
        _ => {}
    }
}

/// Discovery failed with nothing to stand in: the model ID is typed, or the plan screen closes.
pub(super) fn discovery_failed(model: &mut Model, why: &str) {
    let Some(setup) = model.model_setup.as_mut() else {
        return;
    };
    match setup.screen {
        Some(Screen::Plan(_)) => {
            back_to_list(model);
            model.push_notice(format!("Your plan's models could not be listed: {why}"));
        }
        _ => {
            setup.step = ModelSetupStep::ModelId;
            setup.editor = Editor::default();
            model.push_notice(format!(
                "Live discovery failed: {why}. Type the model ID the server uses."
            ));
        }
    }
}

pub(super) fn filtered(provider: &Provider) -> Vec<&protocol::DiscoveredModel> {
    let wanted = provider
        .filter
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    provider
        .found
        .iter()
        .filter(|m| {
            wanted.is_empty()
                || m.id.to_lowercase().contains(&wanted)
                || m.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&wanted))
        })
        .collect()
}

/// Whether T2 is choosing from a found list rather than on one of the setup's text steps.
pub(super) fn choosing(model: &Model) -> bool {
    model.model_setup.as_ref().is_some_and(|setup| {
        matches!(
            setup.step,
            ModelSetupStep::ModelId | ModelSetupStep::ReviewTest
        ) && matches!(&setup.screen, Some(Screen::Provider(p)) if !p.found.is_empty())
    })
}

fn chosen(model: &Model) -> Option<protocol::DiscoveredModel> {
    let Some(Screen::Provider(provider)) = model.model_setup.as_ref()?.screen.as_ref() else {
        return None;
    };
    filtered(provider)
        .get(provider.selected)
        .map(|m| (*m).clone())
}

fn provider_key(model: &mut Model, key: KeyEvent) -> bool {
    // The protocol for a server of one's own is picked from the list drawn in the card.
    if matches!(
        model.picker.as_ref().map(|p| &p.kind),
        Some(PickerKind::ModelProtocol)
    ) {
        if key.code == KeyCode::Esc {
            model.picker = None;
            back_to_list(model);
            return true;
        }
        return false;
    }
    let busy = model.model_setup.as_ref().is_some_and(|setup| {
        matches!(
            setup.step,
            ModelSetupStep::Discovering
                | ModelSetupStep::CheckingKey
                | ModelSetupStep::Resolving
                | ModelSetupStep::Saving
                | ModelSetupStep::Activating
                | ModelSetupStep::Testing
        )
    });
    if busy {
        if key.code == KeyCode::Esc {
            back_to_list(model);
        }
        return true;
    }
    if !choosing(model) {
        if key.code == KeyCode::Esc {
            back_to_list(model);
            return true;
        }
        // The setup's own text steps: key, endpoint values, address, model ID, context size.
        return false;
    }
    let provider = provider_mut(model).expect("provider");
    if let Some(filter) = provider.filter.as_mut() {
        match key.code {
            KeyCode::Esc => provider.filter = None,
            KeyCode::Enter => add_chosen(model),
            KeyCode::Backspace => {
                filter.pop();
                provider.selected = 0;
            }
            KeyCode::Up => provider.selected = provider.selected.saturating_sub(1),
            KeyCode::Down => provider.selected += 1,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                filter.push(c);
                provider.selected = 0;
            }
            _ => {}
        }
        clamp(model);
        return true;
    }
    match key.code {
        KeyCode::Esc => back_to_list(model),
        KeyCode::Up => provider.selected = provider.selected.saturating_sub(1),
        KeyCode::Down => provider.selected += 1,
        KeyCode::Char('/') => provider.filter = Some(String::new()),
        KeyCode::Enter => add_chosen(model),
        KeyCode::Tab => replace_key(model),
        KeyCode::Char('a') => advanced(model),
        KeyCode::Char('c') => check(model),
        _ => {}
    }
    clamp(model);
    true
}

fn clamp(model: &mut Model) {
    if let Some(provider) = provider_mut(model) {
        let shown = filtered(provider).len();
        provider.selected = provider.selected.min(shown.saturating_sub(1));
    }
}

fn back_to_list(model: &mut Model) {
    model.form_generation = model.form_generation.wrapping_add(1);
    model.picker = None;
    let fresh = ModelSetup::new();
    model.model_setup = Some(fresh);
    open(model);
}

fn take_chosen(model: &mut Model) -> bool {
    let Some(found) = chosen(model) else {
        return false;
    };
    let setup = model.model_setup.as_mut().expect("setup");
    setup.model = found.id;
    setup.max_ctx = found.context_length;
    true
}

fn add_chosen(model: &mut Model) {
    if take_chosen(model) {
        let setup = model.model_setup.as_mut().expect("setup");
        if setup.max_ctx.is_some() {
            backend_features::finish_model_setup(model);
        } else {
            // The server gave no size: ask for it, as adding a model always has.
            setup.step = ModelSetupStep::ContextWindow;
            setup.editor = Editor::default();
            model.push_notice(
                "The server didn't report a context window. Enter it in tokens, or leave it blank.",
            );
        }
        return;
    }
    // Nothing matches the filter: what was typed is the model ID, as the server names it.
    let typed = provider_mut(model)
        .and_then(|p| p.filter.clone())
        .unwrap_or_default();
    if let Some(setup) = model.model_setup.as_mut() {
        setup.step = ModelSetupStep::ModelId;
        setup.editor.cursor = typed.len();
        setup.editor.text = typed;
    }
    if let Some(provider) = provider_mut(model) {
        provider.found.clear();
        provider.filter = None;
    }
}

/// `a`: the size and other limits go through the setup's own steps before saving.
fn advanced(model: &mut Model) {
    if take_chosen(model) {
        let setup = model.model_setup.as_mut().expect("setup");
        setup.max_ctx = None;
        setup.step = ModelSetupStep::ContextWindow;
        setup.editor = Editor::default();
    }
}

/// `c`: the same synthetic check as the setup's test, on the highlighted model; never by itself.
fn check(model: &mut Model) {
    if !take_chosen(model) {
        return;
    }
    let setup = model.model_setup.as_mut().expect("setup");
    let draft = backend_features::setup_draft(setup);
    setup.step = ModelSetupStep::Testing;
    if !request(model, Effect::TestModel(protocol::TestModel { draft }))
        && let Some(setup) = model.model_setup.as_mut()
    {
        setup.step = ModelSetupStep::ModelId;
    }
}

/// A check finished for a connect screen: show it beside the list, which stays choosable.
pub(super) fn tested(model: &mut Model, result: protocol::ModelTestResult) -> bool {
    let Some(setup) = model.model_setup.as_mut() else {
        return false;
    };
    match setup.screen.as_mut() {
        Some(Screen::Provider(provider)) => {
            provider.tested = Some(result);
            setup.step = ModelSetupStep::ModelId;
            true
        }
        Some(Screen::Plan(plan)) => {
            plan.tested = Some(result);
            setup.step = ModelSetupStep::ModelId;
            true
        }
        _ => false,
    }
}

/// Tab: type a new key for this server; the found list returns after it is checked.
fn replace_key(model: &mut Model) {
    if let Some(setup) = model.model_setup.as_mut() {
        setup.step = ModelSetupStep::ApiKey;
        setup.editor = Editor::default();
    }
    if let Some(provider) = provider_mut(model) {
        provider.found.clear();
        provider.tested = None;
    }
}

pub(super) fn begin_chatgpt(model: &mut Model, client_id: Option<String>, consent: bool) {
    if let Some(list) = list_mut(model) {
        list.opening = true;
    }
    let begin = protocol::BeginChatGptSignIn { client_id, consent };
    if !request(model, Effect::ChatGpt(ChatGptCall::Begin(begin)))
        && let Some(list) = list_mut(model)
    {
        list.opening = false;
    }
}

fn wait_for(model: &mut Model, sign_in: protocol::ChatGptSignIn, again: bool) {
    let protocol::ChatGptSignIn { link, id } = sign_in;
    model.pending_link = Some(link.clone());
    if let Some(setup) = model.model_setup.as_mut() {
        setup.screen = Some(Screen::Waiting { link, id, again });
    }
    let finish = protocol::FinishChatGptSignIn { id };
    request(model, Effect::ChatGpt(ChatGptCall::Finish(finish)));
}

/// The plan model as every surface saves it: OpenAI's Responses route, signed in, no key.
pub(super) fn plan_draft(model: &str, context: Option<u32>) -> protocol::ModelDraft {
    protocol::ModelDraft {
        provider_id: Some("openai".into()),
        auth: Some(protocol::ModelAuth::ChatGptPlan),
        connection: Default::default(),
        name: None,
        protocol: protocol::ModelProtocol::OpenAiResponses,
        base_url: "https://api.openai.com/v1".into(),
        model: model.into(),
        context_limit: context,
        key: None,
        allow_other_provider: false,
    }
}

/// The backend answered a sign-in call.
pub(super) fn chatgpt(model: &mut Model, answer: ChatGptAnswer) {
    if model
        .model_setup
        .as_ref()
        .and_then(|s| s.screen.as_ref())
        .is_none()
    {
        return;
    }
    let screen = model.model_setup.as_ref().and_then(|s| s.screen.as_ref());
    let current = match (&answer, screen) {
        (ChatGptAnswer::Begun(_), Some(Screen::List(list))) => list.opening,
        (ChatGptAnswer::Finished { id, .. }, Some(Screen::Waiting { id: waiting, .. })) => {
            id == waiting
        }
        (
            ChatGptAnswer::Accounts(_) | ChatGptAnswer::Switched | ChatGptAnswer::SignedOut { .. },
            Some(Screen::Accounts { .. }),
        ) => true,
        _ => false,
    };
    if !current {
        // An answer for a sign-in this screen moved on from: free its port if it is still open.
        if let ChatGptAnswer::Begun(sign_in) = answer {
            let cancel = protocol::CancelChatGptSignIn { id: sign_in.id };
            request(model, Effect::ChatGpt(ChatGptCall::Cancel(cancel)));
        }
        return;
    }
    match answer {
        ChatGptAnswer::Begun(sign_in) => wait_for(model, sign_in, false),
        ChatGptAnswer::Finished { outcome, .. } => {
            if let Some(again) = outcome.again {
                wait_for(model, again, true);
                return;
            }
            let Some(account) = outcome.account else {
                return;
            };
            let setup = model.model_setup.as_mut().expect("setup");
            if !account.plan_enabled {
                setup.screen = Some(Screen::PlanOff { account });
                return;
            }
            setup.screen = Some(Screen::Plan(Plan {
                account,
                models: Vec::new(),
                selected: 0,
                loading: true,
                tested: None,
            }));
            let plan = plan_draft("discovery", None);
            setup.step = ModelSetupStep::Discovering;
            request(
                model,
                Effect::Discover(protocol::DiscoverModels {
                    provider_id: plan.provider_id,
                    auth: plan.auth,
                    connection: plan.connection,
                    protocol: plan.protocol,
                    base_url: plan.base_url,
                    key: protocol::Secret(String::new()),
                    allow_other_provider: false,
                }),
            );
        }
        ChatGptAnswer::Cancelled => {}
        ChatGptAnswer::Accounts(list) => {
            if let Some(Screen::Accounts {
                accounts,
                selected,
                loading,
            }) = model.model_setup.as_mut().and_then(|s| s.screen.as_mut())
            {
                *selected = (*selected).min(list.len().saturating_sub(1));
                *accounts = list;
                *loading = false;
            }
        }
        ChatGptAnswer::Switched => {
            request(model, Effect::ChatGpt(ChatGptCall::Accounts));
        }
        ChatGptAnswer::SignedOut { revoked } => {
            if !revoked {
                model.push_notice("Signed out here. OpenAI did not confirm it; you can disconnect Medha in ChatGPT settings.");
            }
            request(model, Effect::ChatGpt(ChatGptCall::Accounts));
        }
    }
}

/// A sign-in call failed: say why, unless the person already moved on from that sign-in.
pub(super) fn chatgpt_failed(model: &mut Model, call: &ChatGptCall, why: &str) {
    let screen = model.model_setup.as_ref().and_then(|s| s.screen.as_ref());
    let current = match (call, screen) {
        (ChatGptCall::Begin(_), Some(Screen::List(list))) => list.opening,
        (ChatGptCall::Finish(p), Some(Screen::Waiting { id, .. })) => p.id == *id,
        (
            ChatGptCall::Accounts | ChatGptCall::Switch(_) | ChatGptCall::SignOut(_),
            Some(Screen::Accounts { .. }),
        ) => {
            // The accounts stay on screen; only the reason is new.
            if let Some(Screen::Accounts { loading, .. }) =
                model.model_setup.as_mut().and_then(|s| s.screen.as_mut())
            {
                *loading = false;
            }
            model.push_notice(why.trim_start_matches("Error: ").to_owned());
            return;
        }
        _ => false,
    };
    if current {
        back_to_list(model);
        model.push_notice(why.trim_start_matches("Error: ").to_owned());
    }
}

fn waiting_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Screen::Waiting { link, id, .. }) =
        model.model_setup.as_ref().and_then(|s| s.screen.as_ref())
    else {
        return true;
    };
    let (link, id) = (link.clone(), *id);
    match key.code {
        KeyCode::Char('c') => model.pending_clipboard = Some(link),
        KeyCode::Char('o') => model.pending_link = Some(link),
        KeyCode::Esc => {
            let cancel = protocol::CancelChatGptSignIn { id };
            request(model, Effect::ChatGpt(ChatGptCall::Cancel(cancel)));
            back_to_list(model);
        }
        _ => {}
    }
    true
}

fn plan_off_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Screen::PlanOff { account }) =
        model.model_setup.as_ref().and_then(|s| s.screen.as_ref())
    else {
        return true;
    };
    let client = account.client_id.clone();
    match key.code {
        KeyCode::Enter => sign_in(model, Some(client), true),
        KeyCode::Esc => back_to_list(model),
        _ => {}
    }
    true
}

fn plan_key(model: &mut Model, key: KeyEvent) -> bool {
    let Some(Screen::Plan(plan)) = model.model_setup.as_mut().and_then(|s| s.screen.as_mut())
    else {
        return true;
    };
    let busy = plan.loading
        || model.model_setup.as_ref().is_some_and(|s| {
            matches!(
                s.step,
                ModelSetupStep::Saving | ModelSetupStep::Activating | ModelSetupStep::Testing
            )
        });
    if busy && key.code != KeyCode::Esc {
        return true;
    }
    let Some(Screen::Plan(plan)) = model.model_setup.as_mut().and_then(|s| s.screen.as_mut())
    else {
        return true;
    };
    let last = plan.models.len().saturating_sub(1);
    let picked = plan.models.get(plan.selected).cloned();
    match key.code {
        KeyCode::Up => plan.selected = plan.selected.saturating_sub(1),
        KeyCode::Down => plan.selected = (plan.selected + 1).min(last),
        KeyCode::Esc => back_to_list(model),
        KeyCode::Char('s') => {
            sign_in(model, None, false);
        }
        KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('c') if picked.is_some() => {
            let found = picked.expect("picked");
            let setup = model.model_setup.as_mut().expect("setup");
            let draft = plan_draft(&found.id, found.context_length);
            setup.protocol = kernel::Protocol::OpenAiResponses;
            setup.auth = draft.auth;
            setup.provider_id = draft.provider_id.clone();
            setup.base_url = draft.base_url.clone();
            setup.model = found.id;
            setup.max_ctx = found.context_length;
            match key.code {
                KeyCode::Enter => backend_features::finish_model_setup(model),
                KeyCode::Char('a') => {
                    setup.max_ctx = None;
                    setup.step = ModelSetupStep::ContextWindow;
                    setup.editor = Editor::default();
                    setup.screen = None;
                }
                _ => {
                    setup.step = ModelSetupStep::Testing;
                    if !request(model, Effect::TestModel(protocol::TestModel { draft }))
                        && let Some(setup) = model.model_setup.as_mut()
                    {
                        setup.step = ModelSetupStep::ModelId;
                    }
                }
            }
        }
        _ => {}
    }
    true
}

/// OpenAI's code when a failed turn was the ChatGPT plan refusing it; the desktop reads the same.
pub(super) fn plan_refusal(message: &str) -> Option<String> {
    ["subscription_sharing_", "chatpass_v2_"]
        .iter()
        .find_map(|prefix| {
            let at = message.find(prefix)?;
            let code: String = message[at..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            (code.len() > prefix.len()).then_some(code)
        })
}

/// The saved model this chat can switch to when the plan stops: the first stand-in that is not a plan.
pub(super) fn stand_in(model: &Model) -> Option<String> {
    let peer = model.remote.as_ref()?;
    let plan = |name: &str| {
        peer.models
            .profiles
            .iter()
            .any(|p| p.name == name && p.auth == Some(protocol::ModelAuth::ChatGptPlan))
    };
    peer.models
        .fallback
        .iter()
        .find(|name| !plan(name))
        .cloned()
}

/// Whether this chat is on a ChatGPT plan model.
pub(super) fn on_plan(model: &Model) -> bool {
    model
        .remote
        .as_ref()
        .and_then(|peer| peer.settings.as_ref())
        .is_some_and(|settings| {
            settings
                .profiles
                .iter()
                .any(|p| p.name == settings.profile && p.chatgpt_plan)
        })
}

/// Keys on the plan-limit pause, while nothing is typed: u opens Manage usage, f switches models.
pub(super) fn plan_limit_key(model: &mut Model, key: KeyEvent) -> bool {
    if model.plan_limit.is_none() || !model.input.is_empty() || model.model_setup.is_some() {
        return false;
    }
    match key.code {
        KeyCode::Char('u') => {
            model.pending_link = Some(MANAGE_USAGE.into());
            true
        }
        KeyCode::Char('f') => {
            let Some(name) = stand_in(model) else {
                return false;
            };
            if !backend_features::boundary_allowed(model)
                || !request(
                    model,
                    Effect::SavedProfile(protocol::ActivateSavedProfile {
                        profile: name.clone(),
                    }),
                )
            {
                return true;
            }
            model.plan_limit = None;
            // The refused message goes back in the composer, ready to send to the new model.
            if let Some(last) = model.history.last().cloned() {
                backend_ui::restore_draft(model, last);
            }
            model.push_notice(format!(
                "Switched this chat to {name}. Press Enter to send your message again."
            ));
            true
        }
        KeyCode::Esc => {
            model.plan_limit = None;
            true
        }
        _ => false,
    }
}

/// A probe of a local server answered: how many models it lists, or none when it is not up.
pub(super) fn probed(model: &mut Model, url: String, count: Option<usize>) {
    if let Some(list) = list_mut(model) {
        list.running.insert(url, count.filter(|&n| n > 0));
    }
    probe_next(model);
}
