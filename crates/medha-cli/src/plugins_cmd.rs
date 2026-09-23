//! `medha plugins`: the operator surface over the extension store.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "medha plugins",
    about = "Inspect and manage local extension packages"
)]
struct PluginsCli {
    #[command(subcommand)]
    command: Option<PluginsCommand>,
}

#[derive(Subcommand)]
enum PluginsCommand {
    /// List discovered packages and their activation state.
    List,
    /// Show a package manifest, requested and granted access, and components.
    Inspect {
        id: String,
        #[arg(long, value_enum)]
        scope: Option<PluginScopeArg>,
    },
    /// Install from a folder, owner/repo[@ref], a git URL, or plugin@marketplace.
    /// It stays off until you enable it.
    Install { source: String },
    /// Fetch the newest commit of an installed plugin; the old files are kept.
    Update {
        id: String,
        /// Approve changed access without an interactive prompt.
        #[arg(long)]
        grant: bool,
    },
    /// Go back to the version before the last update.
    Rollback { id: String },
    /// Check that enabled plugins can actually run, with fixes.
    Doctor,
    /// Add, list, refresh, or remove plugin marketplaces.
    Marketplace {
        #[command(subcommand)]
        command: MarketplaceCommand,
    },
    /// List plugins from added marketplaces.
    Discover,
    /// Enable the exact currently discovered package hash.
    Enable {
        id: String,
        #[arg(long, value_enum)]
        scope: Option<PluginScopeArg>,
        /// Approve the access the package requests without an interactive prompt.
        #[arg(long)]
        grant: bool,
    },
    /// Disable a package without removing its files or data.
    Disable {
        id: String,
        #[arg(long, value_enum)]
        scope: Option<PluginScopeArg>,
    },
    /// Remove a package installed in the managed user plugin store.
    Remove { id: String },
    /// List enabled operator actions. These are not model tools.
    Actions,
}

#[derive(Subcommand)]
enum MarketplaceCommand {
    /// Add a marketplace repository, e.g. owner/repo.
    Add {
        source: String,
    },
    List,
    Refresh {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PluginScopeArg {
    User,
    Project,
}

impl From<PluginScopeArg> for extensions::Scope {
    fn from(value: PluginScopeArg) -> Self {
        match value {
            PluginScopeArg::User => Self::User,
            PluginScopeArg::Project => Self::Project,
        }
    }
}

/// One construction for every surface, so CLI management and session startup
/// always read the same packages and pins.
pub fn store(medha_home: &Path, workspace: &Path, state: &Path) -> extensions::Store {
    extensions::Store::new(
        medha_home.join("plugins"),
        workspace.join(".medha").join("plugins"),
        medha_home.join("plugins.toml"),
        state.join("plugins.toml"),
        env!("CARGO_PKG_VERSION"),
    )
    .with_hook_sources(extensions::HookSource::standard(
        workspace,
        medha_home,
        dirs::home_dir().as_deref(),
    ))
}

pub fn marketplaces(medha_home: &Path) -> extensions::sources::Marketplaces {
    extensions::sources::Marketplaces::new(medha_home.join("plugin-marketplaces.toml"))
        .with_standard_defaults()
}

pub fn expand_home(spec: &str) -> String {
    match (spec.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => spec.to_string(),
    }
}

pub fn short(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

pub fn run(args: Vec<String>) -> Result<()> {
    let cli = PluginsCli::parse_from(std::iter::once("medha-plugins".to_string()).chain(args));
    let workspace = std::env::current_dir()?;
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let medha_home = crate::config::medha_home()?;
    let markets = marketplaces(&medha_home);
    let store = store(
        &medha_home,
        &workspace,
        &crate::config::state_dir(&workspace)?,
    );
    match cli.command.unwrap_or(PluginsCommand::List) {
        PluginsCommand::List => {
            let discovery = store.discover()?;
            if discovery.plugins.is_empty() && discovery.errors.is_empty() {
                println!("No plugins discovered.");
            }
            for plugin in &discovery.plugins {
                println!(
                    "{}  {}  [{} · {}]\n  {}",
                    plugin.package.manifest.id,
                    plugin.package.manifest.version,
                    plugin.scope.as_str(),
                    plugin.activation.as_str(),
                    plugin.package.content_hash
                );
            }
            for notice in discovery.notices() {
                eprintln!("warning: {notice}");
            }
        }
        PluginsCommand::Inspect { id, scope } => {
            inspect(&store.inspect(&id, scope.map(Into::into))?);
        }
        PluginsCommand::Install { source } => {
            let package = store.install_from(&expand_home(&source), &markets)?;
            println!(
                "Installed {} {} (off).\n  {}\nTurn it on with `medha plugins enable {}`.",
                package.manifest.id,
                package.manifest.version,
                package.content_hash,
                package.manifest.id
            );
        }
        PluginsCommand::Update { id, grant } => {
            let plan = store.plan_update(&id)?;
            if plan.up_to_date() {
                println!("{id} is up to date ({}).", short(&plan.to_commit));
                return Ok(());
            }
            println!(
                "{id}: {} ({}) → {} ({})",
                plan.from_version,
                short(&plan.from_commit),
                plan.to_version,
                short(&plan.to_commit)
            );
            let approved = match &plan.access_after {
                Err(error) => {
                    println!("  the new version cannot be granted access: {error}");
                    false
                }
                Ok(_) if !plan.access_changed() => true,
                Ok(after) => {
                    println!("  access changes to:");
                    print_access(&extensions::RequestedPermissions::default(), after);
                    grant || confirm("Allow the new access and keep it on?")?
                }
            };
            let was_enabled = plan.was_enabled;
            let package = store.apply_update(plan, approved)?;
            let on = store.inspect(&id, None)?.activation == extensions::Activation::Enabled;
            println!(
                "Updated {} to {}.{} `medha plugins rollback {id}` restores the previous version.",
                package.manifest.id,
                package.manifest.version,
                if was_enabled && !on {
                    " It is off until you approve its new access."
                } else {
                    ""
                }
            );
        }
        PluginsCommand::Rollback { id } => {
            let package = store.rollback(&id)?;
            println!("Rolled {id} back to {}.", package.manifest.version);
        }
        PluginsCommand::Doctor => {
            let checks = extensions::doctor::run(&store);
            for check in &checks {
                let mark = match check.level {
                    extensions::doctor::Level::Ok => "✓",
                    extensions::doctor::Level::Warn => "!",
                    extensions::doctor::Level::Fail => "✗",
                };
                println!("{mark} {}: {}", check.subject, check.message);
            }
            if checks
                .iter()
                .any(|check| check.level == extensions::doctor::Level::Fail)
            {
                anyhow::bail!("some plugins cannot run; see the ✗ lines above");
            }
        }
        PluginsCommand::Marketplace { command } => match command {
            MarketplaceCommand::Add { source } => {
                let extensions::sources::Spec::Git(source) = extensions::sources::parse(&source)
                else {
                    anyhow::bail!("a marketplace is a repository, e.g. owner/repo or a git URL");
                };
                let market = markets.add(source)?;
                println!(
                    "Added marketplace {} with {} plugin(s). Install one with \
                     `medha plugins install <plugin>@{}`.",
                    market.name,
                    market.plugins.len(),
                    market.name
                );
            }
            MarketplaceCommand::List => {
                for market in markets.list()? {
                    println!(
                        "{}  {} plugin(s)  {}",
                        market.name,
                        market.plugins.len(),
                        market.source.describe()
                    );
                }
            }
            MarketplaceCommand::Refresh { name } => {
                let market = markets.refresh(&name)?;
                println!(
                    "{} now lists {} plugin(s).",
                    market.name,
                    market.plugins.len()
                );
            }
            MarketplaceCommand::Remove { name } => {
                markets.remove(&name)?;
                println!("Removed marketplace {name}; installed plugins stay installed.");
            }
        },
        PluginsCommand::Discover => {
            for failed in markets.add_defaults().into_iter().filter_map(Result::err) {
                eprintln!("warning: {failed}");
            }
            let markets = markets.list()?;
            if markets.is_empty() {
                println!(
                    "No marketplaces yet. Add one with `medha plugins marketplace add owner/repo`."
                );
            }
            for market in markets {
                for listing in market.plugins {
                    println!("{}@{}  {}", listing.name, market.name, listing.description);
                }
            }
        }
        PluginsCommand::Enable { id, scope, grant } => {
            let scope = scope.map(Into::into);
            let plugin = store.inspect(&id, scope)?;
            let requested = plugin.requested_grant()?;
            if !requested.is_empty() {
                println!("{id} requests:");
                print_access(&plugin.package.manifest.permissions, &requested);
                if !grant && !confirm("Grant this access?")? {
                    anyhow::bail!(
                        "{id} was not enabled. Rerun with --grant to approve its access \
                         without a prompt."
                    );
                }
            }
            let package = store.enable(&id, scope, &requested)?;
            println!(
                "Enabled {} at {}. It loads when Medha next starts; disabling or editing it \
                 stops its hooks at once.",
                package.manifest.id, package.content_hash
            );
        }
        PluginsCommand::Disable { id, scope } => {
            let package = store.disable(&id, scope.map(Into::into))?;
            println!("Disabled {}.", package.manifest.id);
        }
        PluginsCommand::Remove { id } => {
            store.remove(&id)?;
            println!("Removed user plugin {id}.");
        }
        PluginsCommand::Actions => {
            let actions = store.actions()?;
            if actions.is_empty() {
                println!("No enabled plugin actions.");
            }
            for action in actions {
                println!("{}  {}\n  {}", action.id, action.title, action.description);
            }
        }
    }
    Ok(())
}

fn inspect(plugin: &extensions::ListedPlugin) {
    let manifest = &plugin.package.manifest;
    println!("{} ({})", manifest.name, manifest.id);
    println!("version: {}", manifest.version);
    println!("requires Medha: {}", manifest.medha);
    println!("scope: {}", plugin.scope.as_str());
    println!("state: {}", plugin.activation.as_str());
    println!("hash: {}", plugin.package.content_hash);
    println!(
        "package: {} file(s), {} bytes",
        plugin.package.files, plugin.package.bytes
    );
    if let Some(description) = &manifest.description {
        println!("description: {description}");
    }
    println!("access requested:");
    match plugin.requested_grant() {
        Ok(requested) if requested.is_empty() => println!("  none"),
        Ok(requested) => print_access(&manifest.permissions, &requested),
        Err(error) => println!("  cannot be granted: {error}"),
    }
    match &plugin.grant {
        Some(grant) if grant.is_empty() => println!("access granted: none"),
        Some(_) => println!("access granted: as requested"),
        None => println!("access granted: nothing (not enabled)"),
    }
    println!("components:");
    for component in &manifest.components {
        println!(
            "  {}  {}",
            manifest.namespaced(component),
            component_kind(component)
        );
    }
    println!("model tools: none automatically exposed");
}

fn print_access(permissions: &extensions::RequestedPermissions, grant: &extensions::Grant) {
    if grant.network {
        println!(
            "  network: allowed for this plugin's processes (asked for {}; \
             Medha cannot limit access to specific hosts)",
            permissions.network_hosts.join(", ")
        );
    }
    if !grant.read_paths.is_empty() {
        println!("  read in workspace: {}", grant.read_paths.join(", "));
    }
    if !grant.write_paths.is_empty() {
        println!("  write in workspace: {}", grant.write_paths.join(", "));
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn component_kind(component: &extensions::ExtensionComponent) -> &'static str {
    match component {
        extensions::ExtensionComponent::Skill { .. } => "skill",
        extensions::ExtensionComponent::Action { .. } => "operator action (not a model tool)",
        extensions::ExtensionComponent::Mcp { .. } => "MCP server definition",
        extensions::ExtensionComponent::Hook { .. } => "hook",
    }
}
