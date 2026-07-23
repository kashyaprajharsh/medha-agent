//! `medha` — Phase 0 entrypoint.
//!
//! Config is *resolved*, never hardcoded (see `config.rs`):
//!   CLI flag  >  env override  >  ~/.medha/config.toml  >  TUI first-run model setup
//!
//! Usage:
//!   medha                        interactive TUI (first run opens model setup)
//!   medha --setup                open the TUI straight into model setup
//!   medha "your task"            run one turn headless
//!   medha --model X "your task"  one-off model override
//!
//! Env overrides: MEDHA_BASE_URL, MEDHA_MODEL, MEDHA_API_KEY

mod acp;
mod config;
mod skill_judge;
mod tui_tea;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kernel::{EventLog, Kernel, Message, Provider, Session};
use providers::OpenAiCompat;
use sandbox::WorkspaceSandbox;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use tools::ToolRegistry;

/// Env var name → setter, for the small overrides that apply on top of
/// whatever `medha.lock` (or its built-in default) already specified.
fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}
fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}
fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}
fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

/// Apply the per-task budget overrides from the environment on top of a base
/// budget (from `medha.lock`, §4.1/§18.5). Precedence: env > lock > built-in
/// default. Only fields whose env var is actually set are overridden — an
/// absent var must never wipe out a value the lock file specified.
///   MEDHA_MAX_TURNS · MEDHA_MAX_TOKENS · MEDHA_MAX_COST (usd) · MEDHA_MAX_WALL (s)
fn apply_budget_env(mut b: kernel::Budget) -> kernel::Budget {
    if let Some(t) = env_u32("MEDHA_MAX_TURNS") {
        b.max_turns = Some(t);
    }
    if let Some(t) = env_u64("MEDHA_MAX_TOKENS") {
        b.max_tokens = Some(t);
    }
    if let Some(c) = env_f64("MEDHA_MAX_COST") {
        b.max_cost_usd = Some(c);
    }
    if let Some(w) = env_u64("MEDHA_MAX_WALL") {
        b.max_wall_s = Some(w);
    }
    b
}

#[derive(Parser, Debug)]
#[command(
    name = "medha",
    version,
    about = "MEDHA — a verification-first, open-first agent harness"
)]
struct Cli {
    /// Open the TUI straight into model setup (same form as /model add)
    #[arg(long)]
    setup: bool,

    /// Override the model for this run
    #[arg(long)]
    model: Option<String>,

    /// Override the provider base URL for this run
    #[arg(long)]
    base_url: Option<String>,

    /// Use the plain scrolling REPL instead of the full-screen TUI (fallback
    /// for terminals that don't support raw mode / alternate screen well).
    #[arg(long)]
    plain: bool,

    /// Editor bridge: expose the session over line-delimited JSON-RPC on stdio
    /// for an editor extension to embed (Vol 4 §5, Agent Client Protocol).
    #[arg(long)]
    acp: bool,

    /// Disable the OS-native execution sandbox — shell/build commands run
    /// directly on the host (only the scanner + approval gate protect you).
    #[arg(long)]
    no_sandbox: bool,

    /// Resume the most recent session in this workspace.
    #[arg(long = "continue", short = 'c')]
    continue_: bool,

    /// Resume a specific session by id (see --sessions).
    #[arg(long)]
    resume: Option<String>,

    /// List past sessions in this workspace and exit.
    #[arg(long)]
    sessions: bool,

    /// The task / prompt
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

/// `medha gate <scenario>` — the Eval Gate (§4.11–4.12, Vol 5). Runs the real
/// agent against fixture scenarios in isolation and scores each run with
/// deterministic checks over the event log + filesystem. Exit code gates CI.
#[derive(Parser)]
#[command(
    name = "medha gate",
    about = "Run eval scenarios against the agent (deterministic CI for cognition)"
)]
struct GateCli {
    /// A scenario file, a scenario directory, or a directory of scenarios
    path: std::path::PathBuf,
    /// Repeats per scenario — >1 gives a pass-rate + confidence interval
    /// (overrides `[gate] seeds`)
    #[arg(long)]
    seeds: Option<u32>,
    /// Promote threshold, 0.0–1.0 (overrides `[gate] pass_threshold`)
    #[arg(long)]
    threshold: Option<f64>,
    /// Machine-readable JSON output for CI
    #[arg(long)]
    json: bool,
    /// Only load and validate the scenario(s) — no model runs. Cheap CI lint.
    #[arg(long)]
    validate: bool,
}

/// Resolve the operator's model + gate policy, then run the scenarios. The gate
/// injects the provider as env into each isolated child run, so a run reaches a
/// model without seeing the operator's real `~/.medha` (its `MEDHA_HOME` is a
/// throwaway). Exit code: 0 all promote · 1 any reject · 2 any hold.
async fn run_gate_command(args: Vec<String>) -> Result<()> {
    let gc = GateCli::parse_from(std::iter::once("medha-gate".to_string()).chain(args));

    // `--validate` never runs the agent, so it needs no provider — a fast lint
    // for CI (and safe to run without spending API budget).
    if gc.validate {
        let paths = gate::discover(&gc.path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut ok = true;
        for p in &paths {
            match gate::Scenario::load(p) {
                Ok(s) => println!("✔ {} — {} check(s), fixture ok", s.id, s.checks.len()),
                Err(e) => {
                    println!("✗ {}: {e}", p.display());
                    ok = false;
                }
            }
        }
        std::process::exit(if ok { 0 } else { 1 });
    }

    let cfg = config::load()?;
    let resolved = config::resolve(cfg.as_ref(), None, None)?
        .filter(|resolved| !resolved.provider.base_url.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the gate needs a configured model — add one with `medha`, or set \
                 MEDHA_BASE_URL / MEDHA_MODEL / MEDHA_API_KEY"
            )
        })?;

    let lock = lockfile::MedhaLock::load_default();
    let seeds = gc.seeds.unwrap_or(lock.gate.seeds).max(1);
    let threshold = gc.threshold.unwrap_or(lock.gate.pass_threshold);

    let mut provider_env = vec![
        (
            "MEDHA_BASE_URL".to_string(),
            resolved.provider.base_url.clone(),
        ),
        ("MEDHA_MODEL".to_string(), resolved.provider.model.clone()),
        ("MEDHA_API_KEY".to_string(), resolved.credential.clone()),
        (
            "MEDHA_PROTOCOL".to_string(),
            resolved.provider.protocol.as_str().to_string(),
        ),
        (
            "MEDHA_AUTH".to_string(),
            resolved.provider.auth.as_str().to_string(),
        ),
        (
            "MEDHA_REASONING_SUPPORT".to_string(),
            resolved.provider.reasoning.as_str().to_string(),
        ),
    ];
    if !resolved.provider.headers.is_empty() {
        provider_env.push((
            "MEDHA_HEADERS_JSON".to_string(),
            serde_json::to_string(&resolved.provider.headers)
                .context("serializing provider headers for gate child")?,
        ));
    }

    let run = gate::RunConfig {
        binary: std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("locating the medha binary: {e}"))?,
        provider_env,
        default_wall_s: 600,
    };

    let results = gate::run_gate(gate::GateOptions {
        path: gc.path,
        seeds,
        threshold,
        run,
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if gc.json {
        println!("{}", gate::report::json(&results));
    } else {
        print!("{}", gate::report::human(&results, seeds));
    }
    std::process::exit(gate::report::exit_code(&results));
}

/// `medha pulse [--fix]` — the configuration health check (the `/pulse` command
/// outside the TUI). Reports which model/endpoint/credential medha resolves and
/// from where, flags mismatches, and — with `--fix` — applies the safe repairs.
/// Static and offline: no keychain prompt, no network, no API spend.
fn run_pulse_command(args: &[String]) -> Result<()> {
    let fix = args.iter().any(|a| a == "--fix" || a == "fix");
    let cfg = config::load()?;
    let report = config::pulse(cfg.as_ref(), None, None);
    print!("{}", report.render());

    if fix {
        let mut cfg = cfg.unwrap_or_default();
        let applied = config::apply_safe_fixes(&mut cfg);
        if applied.is_empty() {
            println!("\nfix: nothing to repair.");
        } else {
            config::save(&cfg)?;
            println!("\nfix: applied {} repair(s):", applied.len());
            for line in applied {
                println!("  ✔ {line}");
            }
        }
    } else if report.has_fixes() {
        println!("\n(run `medha pulse --fix` to apply the fixable items)");
    }
    Ok(())
}

/// Restore file(s) from a snapshot, headlessly — `/rewind` without the TUI
/// or the conversation fork.
#[derive(Parser)]
#[command(
    name = "medha undo",
    about = "Restore file(s) from a snapshot (no conversation change)"
)]
struct UndoCli {
    /// Undo this event and everything after it, instead of just the last write.
    #[arg(long)]
    event: Option<String>,
    /// List recent write events instead of restoring anything.
    #[arg(long)]
    list: bool,
}

#[derive(Parser)]
#[command(name = "medha memory", about = "Inspect and manage persistent memory")]
struct MemoryCli {
    #[command(subcommand)]
    command: MemoryCommand,
}

#[derive(Subcommand)]
enum MemoryCommand {
    List {
        #[arg(long)]
        scope: Option<String>,
    },
    Show {
        name: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Search {
        query: Vec<String>,
    },
    Edit {
        name: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Forget {
        name: String,
        #[arg(long)]
        scope: Option<String>,
    },
    Pin {
        name: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        off: bool,
    },
    Pending,
    Approve {
        id: String,
    },
}

fn memory_scope(raw: Option<&str>) -> Result<memory::Scope> {
    match raw {
        None | Some("project") => Ok(memory::Scope::Project),
        Some("user") => Ok(memory::Scope::User),
        Some(other) => anyhow::bail!("unknown memory scope '{other}' (project|user)"),
    }
}

fn memory_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

async fn append_cli_memory_op(
    log: &store::SqliteLog,
    projection: &memory::MemoryProjection,
    session: &Session,
    op: memory::MemoryOp,
) -> Result<()> {
    log.append(kernel::Event::memory_write(
        session,
        serde_json::to_value(&op)?,
    ))
    .await?;
    projection.apply(&op)?;
    Ok(())
}

fn find_memory(
    projection: &memory::MemoryProjection,
    scope: Option<&str>,
    name: &str,
) -> Result<Option<memory::MemoryEntry>> {
    if let Some(scope) = scope {
        return Ok(projection.get(memory_scope(Some(scope))?, name)?);
    }
    Ok(projection
        .get(memory::Scope::Project, name)?
        .or(projection.get(memory::Scope::User, name)?))
}

async fn run_memory_command(args: Vec<String>) -> Result<()> {
    let cli = MemoryCli::parse_from(std::iter::once("medha-memory".to_string()).chain(args));
    let cwd = std::env::current_dir()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let state = config::state_dir(&cwd)?;
    let log = store::SqliteLog::open(state.join("events.db"))?;
    let projection = memory::MemoryProjection::open(
        state.join("memory.db"),
        config::medha_home()?.join("memory.db"),
    )?;
    projection.rebuild_project(
        log.all_events()?
            .into_iter()
            .filter(|event| event.provenance.source != "fork"),
    )?;

    match cli.command {
        MemoryCommand::List { scope } => {
            let scope = scope
                .as_deref()
                .map(|value| memory_scope(Some(value)))
                .transpose()?;
            let now = memory_now();
            let entries = projection
                .list()?
                .into_iter()
                .filter(|entry| scope.is_none_or(|scope| entry.scope == scope))
                .collect::<Vec<_>>();
            if entries.is_empty() {
                println!("No memories found.");
            } else {
                for entry in entries {
                    let age = ((now - entry.updated).max(0.0) / 86_400.0).floor() as u64;
                    println!(
                        "{}  [{} · {} · {}d{}]  {}",
                        entry.name,
                        entry.scope.as_str(),
                        entry.trust.as_str(),
                        age,
                        if entry.pinned { " · pinned" } else { "" },
                        entry.description
                    );
                }
            }
        }
        MemoryCommand::Show { name, scope } => {
            let entry = find_memory(&projection, scope.as_deref(), &name)?
                .ok_or_else(|| anyhow::anyhow!("memory '{name}' not found"))?;
            println!("{} [{}]", entry.name, entry.scope.as_str());
            println!("kind: {}", entry.kind.as_str());
            println!("trust: {}", entry.trust.as_str());
            println!("confidence: {}", entry.confidence.as_str());
            println!("version: {}", entry.version);
            println!("pinned: {}", entry.pinned);
            println!("description: {}", entry.description);
            println!("claim:\n{}", entry.claim);
            println!("provenance:");
            let all_events = log.all_events()?;
            for event_id in &entry.provenance {
                if let Some(event) = all_events.iter().find(|event| event.id == *event_id) {
                    println!(
                        "  {}  session={}  kind={}",
                        event.id,
                        event.session_id,
                        event.kind.as_str()
                    );
                } else {
                    println!("  {event_id}  (event not found locally)");
                }
            }
        }
        MemoryCommand::Search { query } => {
            let query = query.join(" ");
            if query.trim().is_empty() {
                anyhow::bail!("memory search needs a non-empty query");
            }
            for entry in projection.search(&query, 20)? {
                println!(
                    "{}  [{} · {}]  {}\n  {}",
                    entry.name,
                    entry.scope.as_str(),
                    entry.trust.as_str(),
                    entry.description,
                    entry.claim.replace('\n', " ")
                );
            }
        }
        MemoryCommand::Edit { name, scope } => {
            let mut entry = find_memory(&projection, scope.as_deref(), &name)?
                .ok_or_else(|| anyhow::anyhow!("memory '{name}' not found"))?;
            let path =
                std::env::temp_dir().join(format!("medha-memory-edit-{}.md", ulid::Ulid::new()));
            std::fs::write(&path, &entry.claim)?;
            let editor =
                std::env::var("EDITOR").map_err(|_| anyhow::anyhow!("$EDITOR is not set"))?;
            let mut parts = editor.split_whitespace();
            let program = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("$EDITOR is empty"))?;
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(&path)
                .status()?;
            if !status.success() {
                anyhow::bail!("editor exited with {status}");
            }
            let edited = std::fs::read_to_string(&path)?;
            std::fs::remove_file(&path).ok();
            if edited.trim().is_empty() {
                anyhow::bail!("edited memory claim is empty");
            }
            let session = Session::new();
            let evidence = log
                .append(kernel::Event::user_message(
                    &session,
                    &format!("CLI memory edit: {name}"),
                ))
                .await?;
            entry.claim = edited.trim_end().to_string();
            entry.trust = kernel::TrustLabel::User;
            entry.confidence = memory::ConfidenceRung::UserStated;
            entry.provenance.push(evidence.id);
            entry.sessions.push(session.id);
            entry.version += 1;
            entry.updated = memory_now();
            append_cli_memory_op(
                &log,
                &projection,
                &session,
                memory::MemoryOp::Update { entry },
            )
            .await?;
            println!("Updated '{name}' through the event log.");
        }
        MemoryCommand::Forget { name, scope } => {
            let scope = memory_scope(scope.as_deref())?;
            if projection.get(scope, &name)?.is_none() {
                anyhow::bail!("memory '{name}' not found in {} scope", scope.as_str());
            }
            append_cli_memory_op(
                &log,
                &projection,
                &Session::new(),
                memory::MemoryOp::Forget {
                    scope,
                    name: name.clone(),
                },
            )
            .await?;
            println!("Forgot '{name}'.");
        }
        MemoryCommand::Pin { name, scope, off } => {
            let scope = memory_scope(scope.as_deref())?;
            if projection.get(scope, &name)?.is_none() {
                anyhow::bail!("memory '{name}' not found in {} scope", scope.as_str());
            }
            append_cli_memory_op(
                &log,
                &projection,
                &Session::new(),
                memory::MemoryOp::Pin {
                    scope,
                    name: name.clone(),
                    pinned: !off,
                },
            )
            .await?;
            println!("{} '{name}'.", if off { "Unpinned" } else { "Pinned" });
        }
        MemoryCommand::Pending => {
            let dir = state.join("memory-pending");
            let mut paths = std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect()
                })
                .unwrap_or_else(|_| Vec::<std::path::PathBuf>::new());
            paths.sort();
            if paths.is_empty() {
                println!("No pending memory writes.");
            }
            for path in paths {
                if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                    println!("{}", path.file_stem().unwrap_or_default().to_string_lossy());
                }
            }
        }
        MemoryCommand::Approve { id } => {
            let pending_id = ulid::Ulid::from_string(&id)
                .map_err(|_| anyhow::anyhow!("pending id must be a ULID"))?;
            let path = state
                .join("memory-pending")
                .join(format!("{pending_id}.json"));
            let op: memory::MemoryOp = serde_json::from_slice(&std::fs::read(&path)?)?;
            append_cli_memory_op(&log, &projection, &Session::new(), op).await?;
            std::fs::remove_file(path)?;
            println!("Approved pending memory {pending_id}.");
        }
    }
    Ok(())
}

struct WriteEvent {
    id: ulid::Ulid,
    session: ulid::Ulid,
    path: String,
    ts: f64,
}

/// Write-family observations (same `snapshot`-key check as `rollback_plan`),
/// newest first across all sessions.
async fn recent_writes(log: &store::SqliteLog, limit: usize) -> Vec<WriteEvent> {
    let mut out = Vec::new();
    for meta in log.list_sessions().unwrap_or_default() {
        let events = kernel::EventLog::events(log, meta.id).await;
        for e in events.iter().rev() {
            if e.kind != kernel::EventKind::ToolObs {
                continue;
            }
            let Some(result) = e.payload.get("payload").and_then(|p| p.as_object()) else {
                continue;
            };
            if !result.contains_key("snapshot") {
                continue;
            }
            let Some(path) = result.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            out.push(WriteEvent {
                id: e.id,
                session: meta.id,
                path: path.to_string(),
                ts: e.ts,
            });
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

async fn run_undo_command(args: Vec<String>) -> Result<()> {
    let uc = UndoCli::parse_from(std::iter::once("medha-undo".to_string()).chain(args));

    let cwd = std::env::current_dir()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let state = config::state_dir(&cwd)?;
    let log = store::SqliteLog::open(state.join("events.db"))?;

    if uc.list {
        let writes = recent_writes(&log, 10).await;
        if writes.is_empty() {
            println!("No file writes recorded in this workspace yet.");
            return Ok(());
        }
        println!("Recent writes in this workspace (newest first):\n");
        for w in &writes {
            let when = chrono::DateTime::from_timestamp(w.ts as i64, 0)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            println!("  {}  {when}  {}", w.id, w.path);
        }
        println!("\nUndo one:  medha undo --event <id>");
        return Ok(());
    }

    let (session_id, target) = if let Some(idstr) = &uc.event {
        let target = ulid::Ulid::from_string(idstr.trim())
            .map_err(|_| anyhow::anyhow!("invalid event id '{idstr}'"))?;
        let mut found = None;
        for meta in log.list_sessions().unwrap_or_default() {
            let events = kernel::EventLog::events(&log, meta.id).await;
            if events.iter().any(|e| e.id == target) {
                found = Some(meta.id);
                break;
            }
        }
        let session_id = found
            .ok_or_else(|| anyhow::anyhow!("event {target} not found in this workspace's log"))?;
        (session_id, target)
    } else {
        let writes = recent_writes(&log, 1).await;
        let Some(last) = writes.into_iter().next() else {
            println!("Nothing to undo — no file writes recorded in this workspace yet.");
            return Ok(());
        };
        (last.session, last.id)
    };

    let events = kernel::EventLog::events(&log, session_id).await;
    let plan = kernel::events::rollback_plan(&events, target);
    if plan.is_empty() {
        println!(
            "Nothing to undo at event {target} (not a write, or already at the workspace's HEAD state)."
        );
        return Ok(());
    }

    let trust_path = state.join("trust.lock");
    let audit_path = state.join("logs").join("audit.log");
    let sandbox = WorkspaceSandbox::new(
        cwd.clone(),
        trust_path,
        audit_path,
        Some(Arc::new(kernel::AutoDeny)),
    )?
    .with_snapshots_dir(state.join("snapshots"));

    println!(
        "Restoring {} file(s) to their state at event {target}:",
        plan.len()
    );
    for f in &plan {
        match &f.snapshot {
            Some(_) => println!("  {} — reverted to the snapshot before that write", f.path),
            None => println!(
                "  {} — removed (it was created at/after this point)",
                f.path
            ),
        }
        sandbox.restore(&f.path, f.snapshot.as_deref()).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // medha deliberately does NOT auto-load a project `.env`. As a roaming
    // harness it runs inside repos it doesn't own, and dotenv walks up ancestors
    // — so a project's own `.env` (its `OPENAI_*` / `OPENAI_COMPATIBLE_*` LLM
    // settings) silently hijacked medha's model and credentials, and could send
    // one program's secrets through another. medha's configuration is a function
    // of medha's own state only: `~/.medha/config.toml`, the OS keychain /
    // credentials file, `medha.lock [routing]`, and the `MEDHA_*` env namespace.
    // The workspace's `.env` still reaches child tool processes through normal
    // inheritance — medha simply never reads it for its own config. `medha nadi`
    // (or `/nadi` in the TUI) shows exactly where each value resolved from.

    // `medha gate <scenario>` is an operator subcommand (the Eval Gate, §4.11–4.12).
    // It's dispatched by hand *before* the main clap parse: the interactive/headless
    // CLI uses a trailing free-form prompt positional, which cannot coexist with a
    // clap subcommand — so we peel `gate` off argv and parse it with its own parser
    // (real --help/validation), leaving the existing `medha "task"` UX untouched.
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(|s| s == "gate").unwrap_or(false) {
        return run_gate_command(raw[2..].to_vec()).await;
    }
    if raw.get(1).map(|s| s == "undo").unwrap_or(false) {
        return run_undo_command(raw[2..].to_vec()).await;
    }
    if raw.get(1).map(|s| s == "memory").unwrap_or(false) {
        return run_memory_command(raw[2..].to_vec()).await;
    }
    if raw.get(1).map(|s| s == "pulse").unwrap_or(false) {
        return run_pulse_command(&raw[2..]);
    }

    let cli = Cli::parse();

    // `--setup` is only a doorway: it opens the normal TUI directly in the
    // model-setup form (the exact surface `/model add` uses — one
    // implementation, never a second wizard). Handled below with use_tui.
    if cli.setup && !std::io::stdin().is_terminal() {
        anyhow::bail!("--setup opens the interactive TUI and needs a terminal");
    }

    // --sessions only reads the local event log — handle it here, before the
    // provider is resolved, so listing sessions never touches the API key /
    // keychain (it shouldn't need to prompt for a read-only list).
    if cli.sessions {
        let cwd = std::env::current_dir()?;
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let state = config::state_dir(&cwd)?;
        migrate_legacy_state(&cwd, &state);
        print_sessions(&store::SqliteLog::open(state.join("events.db"))?)?;
        return Ok(());
    }

    // Resolve provider from flags → env → saved config. Nothing resolvable +
    // interactive TUI = first-run: start unconfigured and open the model-setup
    // form inside the TUI. Headless callers get an actionable error instead —
    // scripts must never hang on interactive prompts.
    let cfg = config::load()?;
    let is_tty_early = std::io::stdin().is_terminal();
    let tui_possible =
        !cli.acp && !cli.plain && cli.prompt.join(" ").trim().is_empty() && is_tty_early;
    let resolved = match config::resolve(cfg.as_ref(), cli.base_url.clone(), cli.model.clone())? {
        Some(r) => r,
        None if tui_possible || cli.setup => config::Resolved {
            name: String::new(),
            provider: providers::ProviderProfile::openai_chat(
                String::new(),
                String::new(),
                providers::AuthKind::None,
            ),
            credential: String::new(),
            model_source: config::Source::Config,
            base_url_source: config::Source::Config,
            credential_source: config::CredSource::KeychainOrNone,
        },
        None => anyhow::bail!(
            "no model configured. Run `medha` to add one in the TUI, \
             or set MEDHA_BASE_URL / MEDHA_MODEL / MEDHA_API_KEY."
        ),
    };
    // Open the TUI in model setup on explicit --setup or an unconfigured start.
    let open_setup = cli.setup || resolved.provider.base_url.is_empty();

    let prompt = cli.prompt.join(" ");
    let use_plain_repl = cli.plain;

    let model_name = resolved.provider.model.clone();
    // Keep a mutable, persisted profile registry available to the TUI. A
    // session started purely from flags/environment (or still unconfigured)
    // begins with an empty registry; `/model add` writes the first profile.
    let model_profiles = Arc::new(std::sync::Mutex::new(cfg.unwrap_or_default()));
    let active_profile = resolved.name.clone();

    // Resolve the context window so compaction sizes itself — without the user
    // ever typing a number, and without fabricating one. Precedence:
    //   1. explicit (MEDHA_MAX_CTX / config)        — override, wins
    //   2. /v1/models discovery (server-authoritative — the endpoint itself)
    //   3. models.dev (real, externally maintained model metadata; cached
    //      locally from a real, externally maintained metadata source — NOT a
    //      hardcoded table baked into this binary)
    //   4. otherwise unknown → compaction off, say so (never guess, §4.3)
    let (mut max_ctx, mut ctx_source) = (resolved.provider.max_ctx, "config/env");
    // Unconfigured first-run start: no endpoint to ask yet — the TUI's model
    // setup captures the context window when the first profile is saved.
    if max_ctx.is_none()
        && !resolved.provider.base_url.is_empty()
        && matches!(
            resolved.provider.protocol,
            kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
        )
    {
        if let Ok(models) = providers::openai_compat::list_models_for_profile(
            &resolved.provider,
            &resolved.credential,
        )
        .await
        {
            if let Some(c) = models
                .iter()
                .find(|m| m.id == model_name)
                .and_then(|m| m.context_length)
            {
                max_ctx = Some(c);
                ctx_source = "discovered from /v1/models";
            }
        }
    }
    if max_ctx.is_none() && !model_name.is_empty() {
        if let Some(c) = providers::models_dev::context_window(&model_name).await {
            max_ctx = Some(c);
            ctx_source = "models.dev";
        }
    }
    match max_ctx {
        _ if model_name.is_empty() => {} // nothing configured yet — stay quiet
        Some(n) => {
            eprintln!("context window: {n} tokens ({ctx_source}) — compaction enabled");
            if ctx_source == "models.dev" {
                eprintln!(
                    "  note: that's {model_name}'s spec maximum from models.dev — your deployment \
                     may serve less (e.g. a reduced KV-cache limit). If requests get rejected for \
                     context length, set MEDHA_MAX_CTX to the real value your endpoint serves."
                );
            }
        }
        None => eprintln!(
            "note: context window unknown for '{model_name}' (not reported by the server \
             or found on models.dev) — compaction disabled. Set MEDHA_MAX_CTX=<tokens> to enable it."
        ),
    }

    if !resolved.provider.base_url.is_empty()
        && !matches!(
            resolved.provider.protocol,
            kernel::Protocol::OpenAiChat | kernel::Protocol::GeminiInteractions
        )
    {
        anyhow::bail!(
            "protocol '{}' is configured but its native adapter is not implemented yet",
            resolved.provider.protocol.as_str()
        );
    }
    let mut runtime_profile = resolved.provider;
    runtime_profile.max_ctx = max_ctx;
    let provider = if open_setup && runtime_profile.base_url.is_empty() {
        // The first-run TUI needs a provider handle so it can atomically switch
        // to the profile saved by setup. Keep this explicit inert state out of
        // `from_profile`, where accepting an empty endpoint would weaken
        // validation for every real profile.
        OpenAiCompat::unconfigured()
    } else {
        OpenAiCompat::from_profile(runtime_profile, resolved.credential)
            .map_err(|error| anyhow::anyhow!(error))?
    };
    let provider = Arc::new(provider);

    // medha.lock (§6): the harness artifact. Absent file = built-in defaults
    // (identical to MEDHA's behavior before this existed); env vars below layer
    // on top as session-level overrides. `./medha.lock` in the workspace root.
    let lock = lockfile::MedhaLock::load_default();

    // Reasoning/thinking request-side control (§4.4): config-file default,
    // further adjustable live via /think.
    let reasoning = lock.reasoning.to_config();
    if let Err(error) = provider.set_reasoning(reasoning.clone()) {
        eprintln!("note: saved reasoning setting was not applied: {error}");
    } else if reasoning != kernel::ReasoningConfig::default()
        && provider.reasoning_support() == kernel::ReasoningSupport::Unknown
    {
        eprintln!(
            "note: reasoning effort was requested, but this profile marks model support as unverified"
        );
    }
    // Streaming default from the lock; live-toggle via /stream. Absent → on.
    if let Some(stream) = lock.reasoning.stream {
        provider.set_streaming(stream);
    }

    // Runtime state lives OUT of the working tree, under
    // ~/.medha/projects/<encoded-cwd>/ (Claude Code style) — event log,
    // artifacts, snapshots, logs. Only committed config (.medha/skills,
    // medha.lock) stays in the workspace. See config::state_dir.
    let cwd = std::env::current_dir()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let state = config::state_dir(&cwd)?;
    // One-time move of any pre-relocation state from <workspace>/.medha.
    migrate_legacy_state(&cwd, &state);

    // Structured logging to a file, never stdout — a TUI owns the screen (spec §7).
    // state/logs/medha.log; level via RUST_LOG (default info).
    let logs_dir = state.join("logs");
    std::fs::create_dir_all(&logs_dir).ok();
    let (log_writer, _log_guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&logs_dir, "medha.log"));
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let db_path = state.join("events.db");
    let log = Arc::new(store::SqliteLog::open(&db_path)?);

    // Verify the tamper-evident hash chain on resume. A break means the log was
    // edited/corrupted since it was written — warn loudly but don't refuse to
    // start (the operator may be intentionally recovering a damaged log).
    if let Err(e) = log.verify() {
        eprintln!("warning: event log integrity check failed: {e}");
    }

    // Content-addressed artifact store at state/artifacts (§4.5).
    let artifacts = Arc::new(store::FileArtifactStore::open(state.join("artifacts"))?);

    // Human gate (§4.7): the editor's approval card in ACP mode, the TUI's modal
    // in TUI mode, a y/N prompt in the terminal REPL/one-shot, auto-deny headless.
    // Created early so it can be passed to WorkspaceSandbox for permission prompts.
    // --setup always lands in the TUI's model-setup form; it outranks a stray
    // trailing task string (there is nothing to run against mid-setup).
    let has_task = !cli.setup && !prompt.trim().is_empty();
    let is_tty = std::io::stdin().is_terminal();
    let use_acp = cli.acp;
    let use_tui = cli.setup || (!use_acp && is_tty && !has_task && !use_plain_repl);

    let tui_channel = if use_tui {
        Some(tui_tea::channel())
    } else {
        None
    };
    let acp_bridge = if use_acp { Some(acp::bridge()) } else { None };

    let gate: Arc<dyn kernel::HumanGate> = if let Some((writer, pending)) = &acp_bridge {
        Arc::new(acp::AcpGate::new(writer.clone(), pending.clone()))
    } else if let Some((tx, _)) = &tui_channel {
        Arc::new(tui_tea::TuiGate { tx: tx.clone() })
    } else if is_tty {
        Arc::new(TerminalGate)
    } else {
        Arc::new(kernel::AutoDeny)
    };

    // The `clarify` tool's question-asker: only the full TUI renders the form;
    // every other surface has no interactive question UI, so `clarify` reports
    // "skipped" and the agent proceeds on best judgment (never blocks).
    let asker: Arc<dyn kernel::Asker> = if let Some((tx, _)) = &tui_channel {
        Arc::new(tui_tea::TuiAsker { tx: tx.clone() })
    } else {
        Arc::new(kernel::NoAsker)
    };

    // Workspace = current directory; fs/shell tools use permission system for out-of-workspace access (§4.8).
    // medha.lock stays at the project root (committed, user-editable); runtime
    // state (logs/db/artifacts/trust) lives in the per-workspace state dir.
    let lock_path = cwd.join("medha.lock");
    // Machine-local permission grants live in the per-workspace state dir, NOT in
    // the portable medha.lock (§13.3): absolute per-machine paths must not travel
    // with the harness artifact. One-time migration moves any legacy
    // [permissions] block out.
    let trust_path = state.join("trust.lock");
    lockfile::migrate_permissions_to_trust_file(&lock_path, &trust_path).ok();
    // No workspace .gitignore is written any more: runtime state now lives under
    // ~/.medha/projects/, so the only thing left in <workspace>/.medha is
    // committed config (skills), which the user *wants* in version control.
    let audit_path = logs_dir.join("audit.log");
    // Migrate an audit log written by an older build at the project root.
    let legacy_audit = cwd.join("medha_audit.log");
    if legacy_audit.exists() && !audit_path.exists() {
        std::fs::rename(&legacy_audit, &audit_path).ok();
    }
    // Execution sandbox (§4.8): pick the backend from medha.lock's [sandbox],
    // with `--no-sandbox` / MEDHA_SANDBOX=host|native|off as session overrides.
    // Default is the OS-native jail where available.
    let mut sbx_cfg = lock.sandbox.to_config();
    match std::env::var("MEDHA_SANDBOX")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("host") | Some("off") | Some("none") => sbx_cfg.backend = sandbox::BackendKind::Host,
        Some("native") | Some("on") => sbx_cfg.backend = sandbox::BackendKind::Native,
        _ => {}
    }
    if cli.no_sandbox {
        sbx_cfg.backend = sandbox::BackendKind::Host;
    }
    // Validate the opt-in heavy tiers; if misconfigured or unavailable, fall
    // back to the native jail with a warning rather than break the user.
    match sbx_cfg.backend {
        sandbox::BackendKind::Container => {
            let runtime = sbx_cfg.runtime.clone().unwrap_or_else(|| "docker".into());
            if sbx_cfg.image.as_deref().unwrap_or("").is_empty() {
                eprintln!(
                    "warning: [sandbox] backend=container needs an `image` — falling back to the native jail."
                );
                sbx_cfg.backend = sandbox::BackendKind::Native;
            } else if !sandbox::program_on_path(&runtime) {
                eprintln!(
                    "warning: container runtime '{runtime}' not found on PATH — falling back to the native jail."
                );
                sbx_cfg.backend = sandbox::BackendKind::Native;
            }
        }
        sandbox::BackendKind::Ssh => {
            if sbx_cfg.host.as_deref().unwrap_or("").is_empty() {
                eprintln!(
                    "warning: [sandbox] backend=ssh needs a `host` — falling back to the native jail."
                );
                sbx_cfg.backend = sandbox::BackendKind::Native;
            } else if !sandbox::program_on_path("ssh") {
                eprintln!("warning: `ssh` not found on PATH — falling back to the native jail.");
                sbx_cfg.backend = sandbox::BackendKind::Native;
            }
        }
        _ => {}
    }
    if sbx_cfg.backend == sandbox::BackendKind::Native && !sandbox::native_backend_available() {
        eprintln!(
            "warning: OS-native sandbox unavailable here (needs macOS Seatbelt, or Linux \
             Landlock on kernel ≥5.13) — shell commands run on the host; the scanner + \
             approval gate still apply. Set [sandbox] backend = \"host\" to silence."
        );
    }
    // Built-in writable dev caches so ordinary builds work inside the jail
    // (these are non-secret; ~/.ssh, ~/.aws, etc. stay denied by omission).
    let mut extra_writable = lock.sandbox.extra_writable_paths();
    if let Some(home) = dirs::home_dir() {
        for cache in [
            ".cargo",
            ".rustup",
            ".npm",
            ".cache",
            ".pnpm-store",
            ".gradle",
            ".m2",
            "go/pkg",
        ] {
            extra_writable.push(home.join(cache));
        }
    }
    let exec_backend = sandbox::select_backend(&sbx_cfg, extra_writable);

    let workspace = Arc::new(
        WorkspaceSandbox::new(cwd.clone(), trust_path, audit_path, Some(gate.clone()))?
            .with_exec_backend(exec_backend)
            // Skills bundle reference files the model reads on demand; the
            // user skills root lives outside the workspace, so without this
            // every bundled-file read would raise a permission card.
            .with_readable_roots(&[config::user_skills_dir()?])
            .with_snapshots_dir(state.join("snapshots")),
    );
    // Skills (Phase A, §4.11 consumption side): discover project + user skills
    // and register `skill.load`/`skill.save`. The store reads the harness's own
    // `.medha/skills` config dirs directly (not via the sandbox), so scanning
    // never prompts for permission.
    // The skill store gets the two-tier guard: the deterministic regex scanner
    // (in the store) plus an LLM judge (MEDHA's own model) that reviews the
    // ambiguous Caution cases. See `skill_judge`.
    let security_judge = Arc::new(skill_judge::LlmJudge::new(provider.clone()));
    let context_file_loader = context::ctxfiles::ContextFileLoader::new()
        .with_judge(security_judge.clone())
        .with_limits(
            lock.context_files.max_chars,
            lock.context_files
                .max_chars
                .min(context::ctxfiles::PROGRESSIVE_MAX_CHARS),
        );
    let medha_home = config::medha_home()?;
    let startup_context = if lock.context_files.enabled {
        context_file_loader
            .discover_startup(&cwd, &medha_home)
            .await
    } else {
        Vec::new()
    };
    let persona_file = context_file_loader.load_persona(&medha_home).await?;
    let progressive_context =
        (lock.context_files.enabled && lock.context_files.progressive_discovery).then(|| {
            Arc::new(context::ctxfiles::ProgressiveContextFiles::new(
                context_file_loader,
                cwd.clone(),
            ))
        });
    let skill_store = Arc::new(
        tools::SkillStore::new(
            workspace.root().join(".medha").join("skills"),
            Some(config::user_skills_dir()?),
        )
        .with_judge(security_judge),
    );
    let mut registry = ToolRegistry::with_workspace(workspace.clone(), artifacts.clone());
    registry.register_skills(skill_store.clone());
    // Typed memory (D9): project entries in the workspace state dir, user
    // entries in the user-global store — recall merges both.
    let memory_store = Arc::new(memory::MemoryProjection::open(
        state.join("memory.db"),
        config::medha_home()?.join("memory.db"),
    )?);
    let k3_budget_tokens = lock.memory.k3_budget_tokens;
    let stale_after_days = lock.memory.stale_after_days;
    if lock.memory.enabled {
        registry.register_memory_configured(
            memory_store.clone(),
            k3_budget_tokens,
            stale_after_days,
        );
    }
    registry.register_session_search(log.clone(), artifacts.clone());
    let known_tools = registry.tool_names();
    // Live web-search settings, shared with the `web.*` tools. Seed from the
    // saved config (provider choice + stored keys, env fallback); the TUI's
    // `/search` writes this same handle so a change applies without a restart.
    let search_handle = registry.search_handle();
    if let Ok(cfg_guard) = model_profiles.lock() {
        *search_handle.lock().expect("search settings lock") = config::resolve_search(&cfg_guard);
    }
    // Hand the `clarify` tool the surface's question-asker (TUI form, or NoAsker).
    if let Ok(mut slot) = registry.clarify_handle().lock() {
        *slot = Some(asker);
    }
    let executor = Arc::new(registry);

    // Context engine: budget-aware two-phase compaction (§4.3), tuned from
    // medha.lock's [context] section (or its built-in-matching default).
    // LLM summarizer for Full compaction (falls back to extractive on failure),
    // so a compacted session keeps a real handoff summary instead of a keyword
    // scrape that invites hallucination.
    let recall_store = memory_store.clone();
    let memory_enabled = lock.memory.enabled;
    let context_engine = Arc::new(
        context::PipelineEngine::new(lock.context.to_policy())
            .with_summarizer(Arc::new(context::LlmSummarizer::new(provider.clone())))
            .with_artifacts(artifacts.clone())
            .with_full_compaction_refresh(Arc::new(move |system| {
                if !memory_enabled {
                    return system.to_string();
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);
                match memory::recall::compile_k3_configured(
                    &recall_store,
                    k3_budget_tokens,
                    now,
                    stale_after_days,
                ) {
                    Ok(block) => memory::recall::replace_k3(system, &block),
                    Err(_) => system.to_string(),
                }
            })),
    );

    // Deny-first policy + shell command scanner (§4.6). Approval set comes from
    // medha.lock's [policy] approve list, extended by MEDHA_APPROVE (e.g.
    // "writes", "shell", "all").
    let policy = Arc::new(
        policy::DefaultPolicy::requiring_approval(approve_list(lock.policy.approve.clone()))
            .with_memory_write_approval(&lock.memory.write_approval),
    );

    // Deterministic verifier (§4.7): medha.lock's [verify] command, overridden
    // by MEDHA_VERIFY="cargo check" if set. Empty/absent = no verifier.
    let verify_cmd = std::env::var("MEDHA_VERIFY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or(lock.verify.command.clone());
    let verifier: Arc<dyn kernel::Verifier> = match verify_cmd {
        Some(cmd) => Arc::new(CommandVerifier {
            command: cmd,
            dir: cwd.clone(),
        }),
        None => Arc::new(kernel::NoVerify),
    };

    let base_budget = lock.budget.to_budget();
    let ui_config = lock.ui.clone();

    // Cost meter (P1-12): the operator's configured rate wins; else the model's
    // models.dev list price as an *indicative* figure (self-hosted routes don't
    // bill list price); else the meter stays off — never a silent $0.00.
    let pricing = match (lock.pricing.input_per_mtok, lock.pricing.output_per_mtok) {
        (Some(i), Some(o)) => Some(kernel::Pricing {
            input_per_mtok: i,
            output_per_mtok: o,
            indicative: false,
        }),
        _ => providers::models_dev::pricing(&model_name)
            .await
            .map(|(i, o)| kernel::Pricing {
                input_per_mtok: i,
                output_per_mtok: o,
                indicative: true,
            }),
    };
    match &pricing {
        Some(p) if p.indicative => eprintln!(
            "cost meter: {model_name} list price from models.dev (${:.2}/M in, ${:.2}/M out) — \
             indicative only; set [pricing] in medha.lock for your real rate",
            p.input_per_mtok, p.output_per_mtok
        ),
        Some(_) => {}
        None => {
            if base_budget.max_cost_usd.is_some() {
                eprintln!(
                    "warning: max_cost_usd is set but no pricing is known for '{model_name}' — \
                     the cost budget cannot be enforced. Set [pricing] input_per_mtok / \
                     output_per_mtok in medha.lock."
                );
            }
        }
    }

    let max_parallel_tools = env_usize("MEDHA_MAX_PARALLEL_TOOLS")
        .or(lock.budget.max_parallel_tools)
        .unwrap_or(kernel::DEFAULT_MAX_PARALLEL_TOOLS);
    let mut kernel = Kernel::new(
        provider,
        log.clone(),
        executor,
        context_engine,
        artifacts,
        policy,
        gate,
        verifier,
    )
    .with_pricing(pricing)
    .with_max_parallel_tools(max_parallel_tools);
    if let Some(progressive_context) = progressive_context {
        kernel = kernel.with_progressive_context(progressive_context);
    }

    // K1 Identity sheath is assembled by the context compiler, not hardcoded
    // here; config may override the persona (§4.3).
    let configured_persona = model_profiles
        .lock()
        .ok()
        .and_then(|c| c.agent.identity.clone());
    let persona = persona_file
        .as_ref()
        .filter(|file| !file.blocked())
        .map(|file| file.content.as_str())
        .or(configured_persona.as_deref());
    if let Some(file) = persona_file.as_ref().filter(|file| file.blocked()) {
        eprintln!("{}", file.content);
    }
    let mut system = context::identity::system_prompt(persona);
    // Ground the model in the real current date + workspace — without this it
    // guesses a stale year for time-sensitive queries ("latest news" → 2024).
    let today = chrono::Local::now().format("%A, %-d %B %Y").to_string();
    system.push_str(&format!(
        "\n\nEnvironment:\n- Today's date: {today}\n- Workspace: {}\n\nFor anything \
         time-sensitive (news, prices, \"latest\"/\"recent\"/\"today\"), use the current \
         date above — do not assume an older year in your searches or answers.",
        cwd.display()
    ));
    let project_context = context::ctxfiles::render_startup(&startup_context);
    if !project_context.is_empty() {
        system.push_str("\n\n");
        system.push_str(&project_context);
    }
    // K2 skills manifest: one compact line per installed skill so the model knows
    // what it can `skill.load`. Empty (no section) when no skills exist — zero
    // behaviour change for workspaces without skills. In headless mode the task
    // narrows the list when there are many; the interactive session lists all.
    let skills_manifest = skill_store.manifest(
        &known_tools,
        if has_task {
            Some(prompt.as_str())
        } else {
            None
        },
    );
    if !skills_manifest.is_empty() {
        system.push_str("\n\n");
        system.push_str(&skills_manifest);
    }
    // Resume (--continue / --resume <id>): rebuild the prior conversation from
    // the event log and continue the SAME session (new events append onward).
    // Empty `resumed` = a fresh session.
    let (mut session, resumed) = match resolve_resume(&log, &cli).await {
        Ok(Some((id, msgs))) => {
            eprintln!("resumed session {id} ({} prior messages)", msgs.len());
            (
                Session {
                    id,
                    done: false,
                    autonomy: kernel::AutonomyLevel::Careful,
                },
                msgs,
            )
        }
        Ok(None) => (Session::new(), Vec::new()),
        Err(e) => {
            eprintln!("resume failed: {e} — starting a fresh session");
            (Session::new(), Vec::new())
        }
    };
    if lock.memory.enabled {
        let session_events = log.events(session.id).await;
        let forked = session_events
            .first()
            .is_some_and(|event| event.provenance.source == "fork");
        if forked {
            memory_store.rebuild_project(session_events.into_iter())?;
        } else {
            memory_store.rebuild_project(
                log.all_events()?
                    .into_iter()
                    .filter(|event| event.provenance.source != "fork"),
            )?;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0);
        let k3 = memory::recall::compile_k3_configured(
            &memory_store,
            k3_budget_tokens,
            now,
            stale_after_days,
        )?;
        system = memory::recall::replace_k3(&system, &k3);
    }
    // Starting autonomy dial: medha.lock's [policy] autonomy, overridable by
    // MEDHA_MODE. The TUI can change it live via /mode; headless keeps this.
    session.autonomy = kernel::AutonomyLevel::from_id(
        &std::env::var("MEDHA_MODE").unwrap_or_else(|_| lock.policy.autonomy.clone()),
    );
    for file in startup_context.iter().chain(persona_file.iter()) {
        log.append(kernel::Event::context_file(
            &session,
            &file.path.display().to_string(),
            &file.content,
            file.blocked(),
        ))
        .await?;
    }

    let mode = if use_acp {
        "acp"
    } else if use_tui {
        "tui"
    } else if has_task {
        "headless"
    } else {
        "repl"
    };
    tracing::info!(model = %model_name, mode, "medha session start");

    // Editor bridge mode: hand the whole session to the ACP loop over stdio and
    // return when the editor disconnects. Takes priority over TUI/headless.
    if let Some((writer, pending)) = acp_bridge {
        acp::run(
            Arc::new(kernel),
            session,
            system,
            model_name,
            apply_budget_env(base_budget),
            writer,
            pending,
        )
        .await?;
        return Ok(());
    }

    // No task on the command line → interactive session (full TUI by default,
    // --plain for the scrolling REPL, usage if there's no terminal at all).
    // A task always runs headless (scripting/CI).
    if !has_task {
        if let Some((tx, rx)) = tui_channel {
            tui_tea::run_tea(
                Arc::new(kernel),
                session,
                system,
                model_name,
                max_ctx,
                model_profiles,
                active_profile,
                open_setup,
                apply_budget_env(base_budget),
                ui_config,
                resumed,
                workspace.clone(),
                logs_dir.join("stray-stdout.log"),
                skill_store.clone(),
                memory_store.clone(),
                lock.memory.enabled,
                k3_budget_tokens,
                stale_after_days,
                known_tools.clone(),
                search_handle.clone(),
                tx,
                rx,
            )
            .await?;
        } else if is_tty {
            run_repl(
                &kernel,
                &session,
                system,
                &model_name,
                max_ctx,
                apply_budget_env(base_budget),
                resumed,
            )
            .await?;
        } else {
            eprintln!("usage: medha \"<task>\"   (run `medha --setup` to reconfigure)");
        }
        return Ok(());
    }

    // Headless one-shot: stream the run live via the sink, then a trailing
    // newline. (Structured NDJSON output for CI is a separate `--json` mode.)
    let mut messages = vec![Message::system(system)];
    messages.extend(resumed);
    messages.push(Message::user(prompt));
    let sink = PrintSink::plain();
    match kernel
        .run_session(
            &session,
            messages,
            apply_budget_env(base_budget),
            &sink,
            None,
        )
        .await
    {
        Ok((_t, kernel::StopReason::Budget(stop))) => {
            eprintln!(
                "\n(stopped: {} reached — raise the limit to continue)",
                stop.label()
            );
        }
        Ok(_) => println!(),
        Err(e) => eprintln!("error: {e}"),
    }
    Ok(())
}

/// Tool classes that require human approval, from `medha.lock`'s `[policy]
/// approve` list, extended by MEDHA_APPROVE (comma list, with shortcuts:
/// `all`, `writes`, `shell`, and `none`).
///
/// Secure-by-default per spec §4.7 (P5, blast-radius → verification): `shell.exec`
/// is IRREVERSIBLE_LOCAL, so it is gated by the human/verifier by default — the
/// deterministic scanner still hard-denies dangerous commands before that. Set
/// `MEDHA_APPROVE=none` (or `[policy] autonomous = true` intent) to opt out for
/// CI/headless autonomy, where the gate is `AutoDeny` and shell would otherwise
/// be blocked entirely.
fn approve_list(base: Vec<String>) -> Vec<String> {
    let raw = std::env::var("MEDHA_APPROVE").unwrap_or_default();
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    // Explicit autonomous escape hatch: no gating at all.
    if parts.contains(&"none") {
        return Vec::new();
    }

    // Default: gate the one IRREVERSIBLE_LOCAL surface (shell), plus the lock file's list.
    let mut out = base;
    out.push("shell.exec".into());
    for part in parts {
        match part {
            "all" => out.extend(["fs.write", "fs.edit", "shell.exec"].map(String::from)),
            "writes" => out.extend(["fs.write", "fs.edit"].map(String::from)),
            "shell" => out.push("shell.exec".into()),
            other => out.push(other.to_string()),
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Deterministic verifier: runs a shell check (e.g. `cargo check`) in the
/// workspace after edits and reports pass/fail (§4.7).
struct CommandVerifier {
    command: String,
    dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl kernel::Verifier for CommandVerifier {
    async fn check(&self) -> Option<kernel::VerifyReport> {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(&self.dir)
            .kill_on_drop(true)
            .output()
            .await
            .ok()?;
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Some(kernel::VerifyReport {
            ok: out.status.success(),
            summary: format!(
                "`{}` exit {}",
                self.command,
                out.status.code().unwrap_or(-1)
            ),
            output,
        })
    }
}

/// Terminal human gate: print the action + preview, prompt y/N (§4.7).
struct TerminalGate;

#[async_trait::async_trait]
impl kernel::HumanGate for TerminalGate {
    async fn confirm(
        &self,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::Approval {
        println!("\n\x1b[33m⚠ approve {action}?\x1b[0m");
        if escalated {
            println!(
                "\x1b[31m  ⚠ web-tainted action — reviewed every time; 'always' is not offered\x1b[0m"
            );
        }
        if let Some(d) = detail {
            for line in d.lines() {
                if line.starts_with('+') && !line.starts_with("+++") {
                    println!("\x1b[32m{line}\x1b[0m");
                } else if line.starts_with('-') && !line.starts_with("---") {
                    println!("\x1b[31m{line}\x1b[0m");
                } else {
                    println!("{line}");
                }
            }
        }
        // A trust-flow-escalated action is never remembered: offer only once/no.
        if escalated {
            print!("  [1] once  [3] no (default): ");
        } else {
            print!("  [1] once  [2] always  [3] no (default): ");
        }
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        match line.trim().to_lowercase().as_str() {
            "1" | "y" | "yes" | "once" => kernel::Approval::Once,
            "2" | "a" | "always" if !escalated => kernel::Approval::Always,
            _ => kernel::Approval::Deny,
        }
    }
}

/// Clean live-output sink: streams text token-by-token, shows each tool call as
/// one concise line (salient arg only, never a raw JSON dump), notes compaction,
/// and records the provider's *real* token usage. Designed to read well during
/// long runs (§4.13).
struct PrintSink {
    /// Updated with the real prompt-token count from the provider (for the
    /// REPL's live pressure meter). `None` in headless mode.
    usage: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl PrintSink {
    fn plain() -> Self {
        Self { usage: None }
    }
    fn tracking(cell: Arc<std::sync::atomic::AtomicU32>) -> Self {
        Self { usage: Some(cell) }
    }
}

impl kernel::StreamSink for PrintSink {
    fn text(&self, delta: &str) {
        print!("{delta}");
        let _ = std::io::stdout().flush();
    }
    fn reasoning(&self, delta: &str) {
        // Dim italic so thinking is visually distinct from the final answer.
        print!("\x1b[2;3m{delta}\x1b[0m");
        let _ = std::io::stdout().flush();
    }
    fn tool_call(&self, tool: &str, args: &serde_json::Value) {
        println!("\n⏺ {tool}{}", salient_arg(tool, args));
    }
    fn tool_result(&self, tool: &str, ok: bool, payload: &serde_json::Value) {
        // Edit → render the +/- diff; error → show it; otherwise a concise
        // one-line output summary (in/out visibility, like the good agents).
        if let Some(diff) = payload.get("diff").and_then(|v| v.as_str()) {
            for line in diff.lines() {
                if line.starts_with('+') && !line.starts_with("+++") {
                    println!("\x1b[32m{line}\x1b[0m");
                } else if line.starts_with('-') && !line.starts_with("---") {
                    println!("\x1b[31m{line}\x1b[0m");
                } else {
                    println!("{line}");
                }
            }
        } else if !ok {
            let err = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            println!("  ⎿ \x1b[31m✗ {err}\x1b[0m");
        } else {
            println!("  ⎿ {}", result_summary(tool, payload));
        }
    }
    fn compaction(&self, before: u32, after: u32, summarized: bool, _summary: Option<&str>) {
        let how = if summarized { "summarized" } else { "pruned" };
        println!("\n↯ {how} context {before}→{after} tokens");
    }
    fn usage(&self, prompt_tokens: u32, _total_tokens: u32) {
        if let Some(c) = &self.usage {
            c.store(prompt_tokens, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn verify(&self, ok: bool, summary: &str) {
        if ok {
            println!("\n\x1b[32m✔ verify: {summary}\x1b[0m");
        } else {
            println!("\n\x1b[31m✗ verify: {summary}\x1b[0m");
        }
    }
}

/// The one input argument worth showing per tool (the "in"), so the surface
/// reads clearly without dumping the whole args object.
fn salient_arg(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        t if t.starts_with("fs.") => "path",
        "shell.exec" => "command",
        "web.search" => "query",
        "web.fetch" => "url",
        "read_artifact" => "hash",
        "grep" => "pattern",
        // Skill rows read by their name; the description is prose and makes
        // an unreadable label ("Save(Guidance for distinctive, intent…)").
        t if t.starts_with("skill.") => "name",
        _ => "",
    };
    // Preferred key, else the first string argument as a fallback.
    let val = args.get(key).and_then(|v| v.as_str()).or_else(|| {
        args.as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str()))
    });
    match val {
        Some(v) => {
            let short: String = v.chars().take(60).collect();
            let ell = if v.chars().count() > 60 { "…" } else { "" };
            format!("({short}{ell})")
        }
        None => String::new(),
    }
}

/// A concise one-line summary of a tool's output (the "out").
fn result_summary(tool: &str, p: &serde_json::Value) -> String {
    let u = |k: &str| p.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let chars = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .map(|x| x.len())
            .unwrap_or(0)
    };
    let arr = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    match tool {
        "web.search" => format!("{} results", u("count")),
        "grep" => {
            let t = p
                .get("truncated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            format!(
                "{} matches{}",
                u("count"),
                if t { " (truncated)" } else { "" }
            )
        }
        "fs.read" => format!("{} chars", chars("content")),
        "fs.list" => format!("{} entries", arr("entries")),
        "fs.write" => format!("wrote {}", s("path")),
        "web.fetch" => {
            let title = s("title");
            let len = chars("content");
            if title.is_empty() {
                format!("{len} chars")
            } else {
                format!("{title} ({len} chars)")
            }
        }
        "shell.exec" => {
            // Show exit code + a peek at the first non-empty stdout line.
            let first = s("stdout")
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            let peek: String = first.chars().take(80).collect();
            match p.get("exit_code").and_then(|v| v.as_i64()) {
                Some(c) if peek.is_empty() => format!("exit {c}"),
                Some(c) => format!("exit {c} · {peek}"),
                None => "ran".into(),
            }
        }
        "read_artifact" => format!("{} of {} bytes", u("length"), u("total_size")),
        _ => "ok".into(),
    }
}

/// One-time move of pre-relocation runtime state from `<workspace>/.medha` into
/// the new per-workspace state dir (`~/.medha/projects/<enc>`). Non-destructive:
/// each item moves only if the destination does not already exist, so a fresh
/// state dir wins and re-running never clobbers. Committed config (`skills`,
/// `agents`, …) is deliberately NOT moved — it belongs in the workspace.
fn migrate_legacy_state(cwd: &std::path::Path, state: &std::path::Path) {
    let legacy = cwd.join(".medha");
    if !legacy.exists() {
        return;
    }
    // events.db carries WAL/SHM sidecars — move them together or the log breaks.
    for name in [
        "events.db",
        "events.db-wal",
        "events.db-shm",
        "artifacts",
        "snapshots",
        "logs",
        "trust.lock",
    ] {
        let (from, to) = (legacy.join(name), state.join(name));
        if from.exists() && !to.exists() {
            if let Err(e) = move_path(&from, &to) {
                // Don't silently swallow — surface it so a failed move never
                // looks like lost history (the source is left in place).
                eprintln!(
                    "warning: could not migrate {} → {} ({e}). Move it manually to keep the old history.",
                    from.display(),
                    to.display()
                );
            }
        }
    }
    // Remove the blanket ".medha/.gitignore" older builds wrote (content "*"): it
    // would now wrongly ignore committed project skills. Only delete our own file.
    let gi = legacy.join(".gitignore");
    if let Ok(text) = std::fs::read_to_string(&gi) {
        if text.contains("MEDHA local state") {
            std::fs::remove_file(&gi).ok();
        }
    }
    // If nothing committed is left (no skills/agents/etc.), remove the empty dir
    // so a migrated workspace carries no stray .medha at all.
    if std::fs::read_dir(&legacy)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        std::fs::remove_dir(&legacy).ok();
    }
}

/// Move a file or directory, bulletproof across filesystems: try the atomic
/// `rename` first (same volume), and on ANY failure — cross-device `EXDEV` is the
/// common one when `~/.medha` and the workspace sit on different drives — fall
/// back to a recursive copy-then-delete so the migration never gives up silently.
fn move_path(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    move_by_copy(from, to)
}

/// The cross-device fallback for [`move_path`]: copy the tree, then delete the
/// source. Only leaves the source behind if the *delete* fails (data is already
/// safe at `to` by then). Factored out so this branch is unit-testable without
/// actually needing two filesystems.
fn move_by_copy(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if from.is_dir() {
        copy_dir_all(from, to)?;
        std::fs::remove_dir_all(from)?;
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

/// Recursively copy a directory tree (regular files + nested dirs). Symlinks are
/// followed (their target content is copied) — MEDHA's state dirs hold regular
/// files, so this is safe and keeps the copy self-contained on the destination.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Print the workspace's past sessions (newest first) for `--sessions`.
fn print_sessions(log: &store::SqliteLog) -> Result<()> {
    let sessions = log.list_sessions()?;
    if sessions.is_empty() {
        println!("No sessions yet in this workspace.");
        return Ok(());
    }
    println!("Sessions in this workspace (newest first):\n");
    for s in &sessions {
        let when = chrono::DateTime::from_timestamp(s.last_ts as i64, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        println!("  {}", s.id);
        println!("    {when} · {} events · {}", s.events, s.title);
    }
    println!("\nResume:  medha --resume <id>   (or  medha --continue  for the most recent)");
    Ok(())
}

/// Resolve `--continue` / `--resume <id>` into the session to reopen and its
/// reconstructed conversation (projected from the event log). `None` when
/// neither flag is set — a fresh session.
async fn resolve_resume(
    log: &store::SqliteLog,
    cli: &Cli,
) -> Result<Option<(ulid::Ulid, Vec<Message>)>> {
    let id = if let Some(idstr) = &cli.resume {
        ulid::Ulid::from_string(idstr.trim())
            .map_err(|_| anyhow::anyhow!("invalid session id '{idstr}'"))?
    } else if cli.continue_ {
        match log.list_sessions()?.into_iter().next() {
            Some(s) => s.id,
            None => {
                eprintln!("no prior sessions to continue — starting fresh");
                return Ok(None);
            }
        }
    } else {
        return Ok(None);
    };
    // `events()` is the EventLog trait method; call fully-qualified so the trait
    // needn't be imported here.
    let events = kernel::EventLog::events(log, id).await;
    if events.is_empty() {
        anyhow::bail!("session {id} has no events (not found)");
    }
    Ok(Some((id, kernel::project_messages(&events))))
}

/// readline editing, history, and slash commands. The transcript accrues across
/// turns, so compaction engages naturally on long sessions.
async fn run_repl<P, L>(
    kernel: &Kernel<P, L>,
    session: &Session,
    system: String,
    model: &str,
    max_ctx: Option<u32>,
    budget: kernel::Budget,
    resumed: Vec<Message>,
) -> Result<()>
where
    P: kernel::Provider,
    L: kernel::EventLog,
{
    use rustyline::DefaultEditor;
    use rustyline::error::ReadlineError;

    println!("MEDHA — interactive session. /help for commands, /exit to quit.\n");
    let mut rl = DefaultEditor::new()?;
    let mut transcript = vec![Message::system(system)];
    transcript.extend(resumed); // prior conversation when resuming (else empty)

    // Real prompt-token count from the provider's last response (0 until the
    // first turn). The pressure meter reflects this — actual tokens, not a guess.
    let usage = Arc::new(std::sync::atomic::AtomicU32::new(0));

    loop {
        // Live context-pressure meter in the prompt, from real token usage.
        let actual = usage.load(std::sync::atomic::Ordering::Relaxed);
        let prompt_str = pressure_prompt(actual, max_ctx);
        match rl.readline(&prompt_str) {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line.as_str());

                if let Some(cmd) = line.strip_prefix('/') {
                    let cmd = cmd.trim();
                    if let Some(rest) = cmd.strip_prefix("think") {
                        println!("{}", apply_think_command(kernel.provider.as_ref(), rest));
                        continue;
                    }
                    if let Some(rest) = cmd.strip_prefix("effort") {
                        println!("{}", apply_effort_command(kernel.provider.as_ref(), rest));
                        continue;
                    }
                    match cmd {
                        "exit" | "quit" => break,
                        "help" => print_help(),
                        "clear" => {
                            transcript.truncate(1); // keep the system message
                            println!("(conversation cleared)");
                        }
                        "status" => print_status(
                            model,
                            max_ctx,
                            usage.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        other => println!("unknown command: /{other}   (try /help)"),
                    }
                    continue;
                }

                transcript.push(Message::user(line));
                // Output (streamed text, ⏺ tool lines, ↯ compaction) renders live
                // through the sink, which also records real token usage.
                let sink = PrintSink::tracking(usage.clone());
                // Each user message is a fresh task → fresh budget contract.
                match kernel
                    .run_session(session, transcript.clone(), budget.clone(), &sink, None)
                    .await
                {
                    Ok((updated, kernel::StopReason::Budget(stop))) => {
                        transcript = updated;
                        println!(
                            "\n(stopped: {} reached — say \"continue\" or raise the limit)",
                            stop.label()
                        );
                    }
                    Ok((updated, _)) => {
                        transcript = updated;
                        println!(); // separate the answer from the next prompt
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted) => println!("(^C — type /exit to quit)"),
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }
    }
    println!("bye.");
    Ok(())
}

fn print_help() {
    println!(
        "commands:\n  \
         /help                        show this\n  \
         /status                      model, context window, current pressure\n  \
         /think [on|off|status]       enable/disable reasoning (§4.4)\n  \
         /effort [minimal|low|medium|high]\n\
                                      set reasoning depth (turns thinking on)\n  \
         /clear                       reset the conversation (keep system prompt)\n  \
         /exit                        quit (also Ctrl-D)\n\
         anything else is sent to the agent."
    );
}

/// Apply a `/think` command against the live provider; returns the notice to
/// show. Shared by the plain REPL and the full TUI. Unsupported controls are
/// returned visibly by the provider rather than being silently ignored.
pub(crate) fn effort_label(e: Option<kernel::ReasoningEffort>) -> &'static str {
    match e {
        Some(kernel::ReasoningEffort::Minimal) => "minimal",
        Some(kernel::ReasoningEffort::Low) => "low",
        Some(kernel::ReasoningEffort::Medium) => "medium",
        Some(kernel::ReasoningEffort::High) => "high",
        None => "default",
    }
}

/// `/think [on|off|status]` — enable/disable reasoning only. Effort level is
/// a separate concern (`/effort`), since "on vs off" and "how hard" are
/// different knobs some servers only partially support.
fn apply_think_command<P: kernel::Provider>(provider: &P, args: &str) -> String {
    match args.trim() {
        "" | "status" => think_status(provider),
        "on" => {
            let effort = provider.reasoning().effort;
            match provider.set_reasoning(kernel::ReasoningConfig {
                enabled: Some(true),
                effort,
            }) {
                Ok(()) => think_status(provider),
                Err(error) => format!("thinking unchanged: {error}"),
            }
        }
        "off" => {
            match provider.set_reasoning(kernel::ReasoningConfig {
                enabled: Some(false),
                effort: None,
            }) {
                Ok(()) => think_status(provider),
                Err(error) => format!("thinking unchanged: {error}"),
            }
        }
        other => format!(
            "usage: /think [on|off|status]  (got '{other}') — use /effort for reasoning level"
        ),
    }
}

fn think_status<P: kernel::Provider>(provider: &P) -> String {
    let cfg = provider.reasoning();
    let enabled = match cfg.enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "server default",
    };
    format!(
        "thinking: {enabled}  |  effort: {}",
        effort_label(cfg.effort)
    )
}

/// `/effort [minimal|low|medium|high]` — set reasoning depth; also turns thinking on
/// (an effort level only means anything once thinking is enabled). In the
/// full TUI, calling this with no args opens an arrow-key picker instead of
/// requiring the name to be typed.
pub(crate) fn apply_effort_command<P: kernel::Provider>(provider: &P, args: &str) -> String {
    match args.trim() {
        "minimal" | "low" | "medium" | "high" => {
            let level = args.trim();
            let effort = match level {
                "minimal" => kernel::ReasoningEffort::Minimal,
                "low" => kernel::ReasoningEffort::Low,
                "medium" => kernel::ReasoningEffort::Medium,
                _ => kernel::ReasoningEffort::High,
            };
            match provider.set_reasoning(kernel::ReasoningConfig {
                enabled: Some(true),
                effort: Some(effort),
            }) {
                Ok(()) if provider.reasoning_support() == kernel::ReasoningSupport::Unknown => {
                    format!("effort: {level} — requested; profile support is unverified")
                }
                Ok(()) => format!("effort: {level}"),
                Err(error) => format!("effort unchanged: {error}"),
            }
        }
        "" => "usage: /effort [minimal|low|medium|high]".to_string(),
        other => format!("usage: /effort [minimal|low|medium|high]  (got '{other}')"),
    }
}

/// Prompt string with a context-pressure gauge from the provider's *real* last
/// prompt-token count, e.g. `medha [23% ctx]› `. Plain prompt until the first
/// response reports usage (no fabricated number).
fn pressure_prompt(actual_tokens: u32, max_ctx: Option<u32>) -> String {
    match max_ctx {
        Some(mc) if actual_tokens > 0 => {
            let usable = context::ContextBudget::from_max_ctx(mc).usable().max(1);
            let pct = (actual_tokens as f32 / usable as f32 * 100.0).round() as u32;
            format!("medha [{pct}% ctx]› ")
        }
        _ => "medha› ".to_string(),
    }
}

fn print_status(model: &str, max_ctx: Option<u32>, actual_tokens: u32) {
    println!("model: {model}");
    match max_ctx {
        Some(mc) => {
            let usable = context::ContextBudget::from_max_ctx(mc).usable().max(1);
            let trigger = context::CompactionPolicy::default().trigger_ratio;
            let pct = (actual_tokens as f32 / usable as f32 * 100.0).round() as u32;
            let used = if actual_tokens > 0 {
                format!("{actual_tokens} tokens used ({pct}%, real)")
            } else {
                "no usage reported yet".to_string()
            };
            println!(
                "context: {mc} window, {usable} usable — {used}; compacts at {}%",
                (trigger * 100.0) as u32
            );
        }
        None => println!("context: window unknown — compaction off (set MEDHA_MAX_CTX)"),
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("medha-mig-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Legacy <workspace>/.medha state moves to the new state dir, the stale
    /// blanket gitignore is dropped, and an emptied legacy dir is removed —
    /// while committed config (skills) is left untouched.
    #[test]
    fn migrates_state_out_of_workspace_and_keeps_committed_config() {
        let root = tmp();
        let (cwd, state) = (root.join("ws"), root.join("state"));
        let legacy = cwd.join(".medha");
        std::fs::create_dir_all(legacy.join("logs")).unwrap();
        std::fs::create_dir_all(legacy.join("skills").join("mine")).unwrap();
        std::fs::write(legacy.join("events.db"), b"DB").unwrap();
        std::fs::write(legacy.join("logs").join("medha.log"), b"log").unwrap();
        std::fs::write(
            legacy.join(".gitignore"),
            "# MEDHA local state — never commit\n*\n",
        )
        .unwrap();
        std::fs::write(legacy.join("skills").join("mine").join("SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(&state).unwrap();

        migrate_legacy_state(&cwd, &state);

        // State moved out.
        assert_eq!(std::fs::read(state.join("events.db")).unwrap(), b"DB");
        assert!(state.join("logs").join("medha.log").exists());
        assert!(!legacy.join("events.db").exists());
        // Stale auto-gitignore dropped; committed skills kept in the workspace.
        assert!(!legacy.join(".gitignore").exists());
        assert!(
            legacy.join("skills").join("mine").join("SKILL.md").exists(),
            "committed config must stay"
        );
        // Legacy dir NOT removed because skills/ remains.
        assert!(legacy.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Non-destructive: a file already present in the new state dir is never
    /// clobbered by an older legacy copy.
    #[test]
    fn does_not_clobber_existing_state() {
        let root = tmp();
        let (cwd, state) = (root.join("ws"), root.join("state"));
        std::fs::create_dir_all(cwd.join(".medha")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(cwd.join(".medha").join("events.db"), b"OLD").unwrap();
        std::fs::write(state.join("events.db"), b"NEW").unwrap();

        migrate_legacy_state(&cwd, &state);

        assert_eq!(
            std::fs::read(state.join("events.db")).unwrap(),
            b"NEW",
            "existing state wins"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The cross-device fallback (used when rename hits EXDEV) must faithfully
    /// copy a nested directory tree and then remove the source. We call it
    /// directly so the branch is covered without needing two real filesystems.
    #[test]
    fn move_by_copy_relocates_nested_tree_and_removes_source() {
        let root = tmp();
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("a.txt"), b"A").unwrap();
        std::fs::write(from.join("sub").join("b.txt"), b"B").unwrap();

        move_by_copy(&from, &to).unwrap();

        assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"A");
        assert_eq!(std::fs::read(to.join("sub").join("b.txt")).unwrap(), b"B");
        assert!(
            !from.exists(),
            "source is removed after a successful copy-move"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A single file goes through the same fallback (creating the parent).
    #[test]
    fn move_by_copy_relocates_single_file() {
        let root = tmp();
        let from = root.join("db");
        std::fs::write(&from, b"DB").unwrap();
        let to = root.join("nested").join("db");

        move_by_copy(&from, &to).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"DB");
        assert!(!from.exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
