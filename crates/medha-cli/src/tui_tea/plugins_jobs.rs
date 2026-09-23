//! Plugin work that may touch the network runs off the UI thread; its outcome
//! comes back as a `TuiEvent::PluginJob` and continues the flow here.

use super::{Row, Screen, apply_plugins, begin_enable, open, reopen, with_store};
use crate::tui_tea::{Model, Picker, PickerKind, TuiEvent};
use extensions::sources::{self, Marketplaces, Spec};
use extensions::{Activation, Scope, Store};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub(crate) struct JobDone {
    messages: Vec<String>,
    then: After,
}

#[derive(Debug)]
enum After {
    Nothing,
    List(Option<String>),
    /// Offer to turn the plugin on, showing any access it asks for.
    Enable(String),
    Discover,
}

impl JobDone {
    fn failed(error: impl std::fmt::Display) -> Self {
        Self {
            messages: vec![format!("plugins: {error}")],
            then: After::Nothing,
        }
    }
}

pub(crate) fn job_done(model: &mut Model, done: JobDone) {
    for message in done.messages {
        model.push_notice(message);
    }
    match done.then {
        After::Nothing => {}
        After::List(focus) => {
            apply_plugins(model);
            reopen(model, focus.as_deref().map(|id| (id, Scope::User)));
        }
        After::Discover => show_discover(model, String::new()),
        After::Enable(id) => {
            apply_plugins(model);
            match with_store(model, |store| store.inspect(&id, Some(Scope::User))) {
                Ok(plugin) => begin_enable(model, &Row::from_listing(&plugin)),
                Err(error) => model.push_notice(error),
            }
        }
    }
}

fn spawn(
    model: &mut Model,
    tx: &UnboundedSender<TuiEvent>,
    label: String,
    job: impl FnOnce(Store, Marketplaces) -> JobDone + Send + 'static,
) {
    let (Some(store), Some(markets)) = (model.plugins.clone(), model.plugin_markets.clone()) else {
        return model.push_notice("plugins are unavailable in this session");
    };
    model.picker = None;
    model.push_notice(format!("({label} …)"));
    let tx = tx.clone();
    tokio::spawn(async move {
        let done = tokio::task::spawn_blocking(move || job(store, markets))
            .await
            .unwrap_or_else(JobDone::failed);
        let _ = tx.send(TuiEvent::PluginJob(done));
    });
}

/// Folders install at once; repositories and marketplaces are fetched in the
/// background. Either way the next step offers to turn the plugin on.
pub(super) fn install(model: &mut Model, spec: &str, tx: &UnboundedSender<TuiEvent>) {
    let spec = crate::plugins_cmd::expand_home(spec);
    if let Spec::Local(path) = sources::parse(&spec) {
        match with_store(model, |store| store.install(&path)) {
            Ok(package) => {
                model.push_notice(format!(
                    "installed {} {}",
                    package.manifest.id, package.manifest.version
                ));
                job_done(
                    model,
                    JobDone {
                        messages: Vec::new(),
                        then: After::Enable(package.manifest.id),
                    },
                );
            }
            Err(error) => model.push_notice(error),
        }
        return;
    }
    spawn(
        model,
        tx,
        format!("installing {spec}"),
        move |store, markets| match store.install_from(&spec, &markets) {
            Ok(package) => JobDone {
                messages: vec![format!(
                    "installed {} {}",
                    package.manifest.id, package.manifest.version
                )],
                then: After::Enable(package.manifest.id),
            },
            Err(error) => JobDone::failed(error),
        },
    );
}

pub(super) fn update(model: &mut Model, id: &str, tx: &UnboundedSender<TuiEvent>) {
    let id = id.to_string();
    spawn(
        model,
        tx,
        format!("checking {id} for updates"),
        move |store, _| {
            let plan = match store.plan_update(&id) {
                Ok(plan) => plan,
                Err(error) => return JobDone::failed(error),
            };
            if plan.up_to_date() {
                return JobDone {
                    messages: vec![format!(
                        "{id} is up to date ({})",
                        crate::plugins_cmd::short(&plan.to_commit)
                    )],
                    then: After::List(Some(id)),
                };
            }
            let summary = format!(
                "updated {id} {} → {} ({} → {}); ↶ Roll back restores the previous version",
                plan.from_version,
                plan.to_version,
                crate::plugins_cmd::short(&plan.from_commit),
                crate::plugins_cmd::short(&plan.to_commit)
            );
            let needs_review = plan.was_enabled && plan.access_changed();
            match store.apply_update(plan, false) {
                Ok(_) if needs_review => JobDone {
                    messages: vec![summary, format!("{id} asks for different access now")],
                    then: After::Enable(id),
                },
                Ok(_) => JobDone {
                    messages: vec![summary],
                    then: After::List(Some(id)),
                },
                Err(error) => JobDone::failed(error),
            }
        },
    );
}

pub(super) fn rollback(model: &mut Model, id: &str) {
    match with_store(model, |store| store.rollback(id)) {
        Ok(package) => {
            apply_plugins(model);
            model.push_notice(format!("rolled {id} back to {}", package.manifest.version));
        }
        Err(error) => model.push_notice(error),
    }
    reopen(model, Some((id, Scope::User)));
}

pub(super) fn doctor(model: &mut Model) {
    let Some(store) = model.plugins.clone() else {
        return model.push_notice("plugins are unavailable in this session");
    };
    let lines: Vec<String> = extensions::doctor::run(&store)
        .into_iter()
        .map(|check| {
            let mark = match check.level {
                extensions::doctor::Level::Ok => "✓",
                extensions::doctor::Level::Warn => "!",
                extensions::doctor::Level::Fail => "✗",
            };
            format!("  {mark} {}: {}", check.subject, check.message)
        })
        .collect();
    model.push_notice(format!("plugin check:\n{}", lines.join("\n")));
}

pub(super) fn marketplace(model: &mut Model, args: &str, tx: &UnboundedSender<TuiEvent>) {
    let (verb, rest) = args.split_once(' ').unwrap_or((args, ""));
    let rest = rest.trim().to_string();
    match verb {
        "add" if !rest.is_empty() => {
            let Spec::Git(source) = sources::parse(&rest) else {
                return model.push_notice("a marketplace is a repository: owner/repo or a git URL");
            };
            spawn(
                model,
                tx,
                format!("adding marketplace {rest}"),
                move |_, markets| match markets.add(source) {
                    Ok(market) => JobDone {
                        messages: vec![format!(
                            "added marketplace {} with {} plugin(s)",
                            market.name,
                            market.plugins.len()
                        )],
                        then: After::Discover,
                    },
                    Err(error) => JobDone::failed(error),
                },
            );
        }
        "remove" if !rest.is_empty() => {
            match model
                .plugin_markets
                .as_ref()
                .map(|markets| markets.remove(&rest))
            {
                Some(Ok(())) => model.push_notice(format!(
                    "removed marketplace {rest}; its installed plugins stay installed"
                )),
                Some(Err(error)) => model.push_notice(error.to_string()),
                None => model.push_notice("plugins are unavailable in this session"),
            }
        }
        "refresh" => refresh(model, tx),
        _ => open_discover(model, tx),
    }
}

fn refresh(model: &mut Model, tx: &UnboundedSender<TuiEvent>) {
    spawn(model, tx, "refreshing marketplaces".into(), |_, markets| {
        let mut messages = Vec::new();
        for market in markets.list().unwrap_or_default() {
            messages.push(match markets.refresh(&market.name) {
                Ok(market) => format!("{} lists {} plugin(s)", market.name, market.plugins.len()),
                Err(error) => format!("{}: {error}", market.name),
            });
        }
        JobDone {
            messages,
            then: After::Discover,
        }
    });
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoverRow {
    market: String,
    name: String,
    description: String,
    id: String,
    installed: bool,
}

/// The built-in catalogs are fetched the first time Discover opens.
pub(super) fn open_discover(model: &mut Model, tx: &UnboundedSender<TuiEvent>) {
    let pending = model
        .plugin_markets
        .as_ref()
        .is_some_and(|markets| !markets.pending_defaults().is_empty());
    if !pending {
        return show_discover(model, String::new());
    }
    spawn(
        model,
        tx,
        "fetching plugin catalogs".into(),
        |_, markets| {
            let messages = markets
            .add_defaults()
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("a plugin catalog could not be fetched ({error}); Discover retries next time"))
            .collect();
            JobDone {
                messages,
                then: After::Discover,
            }
        },
    );
}

pub(super) fn show_discover(model: &mut Model, query: String) {
    let Some(markets) = model.plugin_markets.clone() else {
        return model.push_notice("plugins are unavailable in this session");
    };
    let store = model.plugins.clone();
    let installed: Vec<String> = model
        .plugins
        .as_ref()
        .and_then(|store| store.discover().ok())
        .map(|discovery| {
            discovery
                .plugins
                .into_iter()
                .map(|plugin| plugin.package.manifest.id)
                .collect()
        })
        .unwrap_or_default();
    let rows = match markets.list() {
        Ok(list) => list
            .into_iter()
            .flat_map(|market| {
                let name = market.name.clone();
                let store = store.clone();
                market.plugins.into_iter().map(move |listing| {
                    let id = store
                        .as_ref()
                        .and_then(|store| store.installed_from(&listing.source))
                        .unwrap_or_else(|| sources::listed_id(&name, &listing.name));
                    DiscoverRow {
                        market: name.clone(),
                        name: listing.name,
                        description: listing.description,
                        installed: false,
                        id,
                    }
                })
            })
            .map(|mut row| {
                row.installed = installed.contains(&row.id);
                row
            })
            .collect(),
        Err(error) => {
            model.push_notice(error.to_string());
            Vec::new()
        }
    };
    if rows.is_empty() {
        model.push_notice(
            "no marketplace plugins yet — add a marketplace, e.g. owner/repo, from the ➕ row",
        );
    }
    let selected = usize::from(!rows.is_empty());
    model.picker = Some(Picker {
        kind: PickerKind::Plugins(Screen::Discover { rows, query }),
        selected,
    });
}

/// Rows every typed word matches, best first: name before description, and a
/// word may be a fuzzy (in-order letters) or one-typo match of a name part.
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

pub(super) fn enter_discover(
    model: &mut Model,
    all: &[DiscoverRow],
    query: &str,
    selected: usize,
    tx: &UnboundedSender<TuiEvent>,
) {
    let rows = matching(all, query);
    if selected == 0 {
        model.push_notice("start typing to search, e.g. github, security, or database");
        return show_discover(model, query.to_string());
    }
    match rows.get(selected - 1) {
        Some(row) if row.installed => {
            let on = with_store(model, |store| store.inspect(&row.id, Some(Scope::User)))
                .is_ok_and(|plugin| plugin.activation == Activation::Enabled);
            model.push_notice(format!(
                "{} is already installed{}",
                row.id,
                if on { " and on" } else { "" }
            ));
            reopen(model, Some((&row.id, Scope::User)));
        }
        Some(row) => install(model, &format!("{}@{}", row.name, row.market), tx),
        None if selected == rows.len() + 1 => {
            model.input = "/plugins marketplace add ".into();
            model.cursor = model.input.len();
            model.push_notice("type the marketplace repository (owner/repo), then Enter");
        }
        None if selected == rows.len() + 2 => refresh(model, tx),
        None => open(model),
    }
}
