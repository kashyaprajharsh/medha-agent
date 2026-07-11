//! `medha` — Phase 0 entrypoint.
//!
//! Config is *resolved*, never hardcoded (see `config.rs`):
//!   CLI flag  >  env override  >  ~/.medha/config.toml  >  first-run wizard
//!
//! Usage:
//!   medha "your task"            run one turn (first run launches setup)
//!   medha --setup                (re)configure the provider interactively
//!   medha --model X "your task"  one-off model override
//!
//! Env overrides: MEDHA_BASE_URL, MEDHA_MODEL, MEDHA_API_KEY

mod acp;
mod config;
mod tui_tea;

use anyhow::Result;
use clap::Parser;
use kernel::{Kernel, Message, Provider, Session};
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
    /// (Re)configure the model provider interactively
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

#[tokio::main]
async fn main() -> Result<()> {
    // Load a .env from the current dir or any ancestor (industry-standard BYOK
    // convenience). Real shell env vars take precedence — dotenvy never
    // overrides values already set in the environment.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // Explicit reconfigure.
    if cli.setup {
        config::run_wizard().await?;
        return Ok(());
    }

    // --sessions only reads the local event log — handle it here, before the
    // provider is resolved, so listing sessions never touches the API key /
    // keychain (it shouldn't need to prompt for a read-only list).
    if cli.sessions {
        let db = std::env::current_dir()?.join(".medha").join("events.db");
        print_sessions(&store::SqliteLog::open(&db)?)?;
        return Ok(());
    }

    // Resolve provider from flags → env → saved config. Only fall back to the
    // first-run wizard if nothing supplies a base URL + model.
    let mut cfg = config::load()?;
    let resolved = match config::resolve(cfg.as_ref(), cli.base_url.clone(), cli.model.clone()) {
        Some(r) => r,
        None => {
            let new_cfg = config::run_wizard().await?;
            let r = config::resolve(Some(&new_cfg), cli.base_url.clone(), cli.model.clone())
                .ok_or_else(|| anyhow::anyhow!("provider unresolved after setup"))?;
            cfg = Some(new_cfg);
            r
        }
    };

    let prompt = cli.prompt.join(" ");
    let use_plain_repl = cli.plain;

    let model_name = resolved.model.clone();

    // Resolve the context window so compaction sizes itself — without the user
    // ever typing a number, and without fabricating one. Precedence:
    //   1. explicit (MEDHA_MAX_CTX / config)        — override, wins
    //   2. /v1/models discovery (server-authoritative — the endpoint itself)
    //   3. models.dev (real, externally maintained model metadata; cached
    //      locally from a real, externally maintained metadata source — NOT a
    //      hardcoded table baked into this binary)
    //   4. otherwise unknown → compaction off, say so (never guess, §4.3)
    let (mut max_ctx, mut ctx_source) = (resolved.max_ctx, "config/env");
    if max_ctx.is_none() {
        if let Ok(models) =
            providers::openai_compat::list_models(&resolved.base_url, &resolved.api_key).await
        {
            if let Some(c) =
                models.iter().find(|m| m.id == model_name).and_then(|m| m.context_length)
            {
                max_ctx = Some(c);
                ctx_source = "discovered from /v1/models";
            }
        }
    }
    if max_ctx.is_none() {
        if let Some(c) = providers::models_dev::context_window(&model_name).await {
            max_ctx = Some(c);
            ctx_source = "models.dev";
        }
    }
    match max_ctx {
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

    let mut provider = OpenAiCompat::new(resolved.base_url, resolved.api_key, resolved.model);
    if let Some(ctx_window) = max_ctx {
        provider = provider.with_max_ctx(ctx_window);
    }
    let provider = Arc::new(provider);

    // medha.lock (§6): the harness artifact. Absent file = built-in defaults
    // (identical to MEDHA's behavior before this existed); env vars below layer
    // on top as session-level overrides. `./medha.lock` in the workspace root.
    let lock = lockfile::MedhaLock::load_default();

    // Reasoning/thinking request-side control (§4.4): config-file default,
    // further adjustable live via /think.
    provider.set_reasoning(lock.reasoning.to_config());

    // Persistent, hash-chained event log at <workspace>/.medha/events.db (§4.2).
    let cwd = std::env::current_dir()?;

    // Structured logging to a file, never stdout — a TUI owns the screen (spec §7).
    // <workspace>/.medha/logs/medha.log; level via RUST_LOG (default info).
    let logs_dir = cwd.join(".medha").join("logs");
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

    let db_path = cwd.join(".medha").join("events.db");
    let log = Arc::new(store::SqliteLog::open(&db_path)?);

    // Verify the tamper-evident hash chain on resume. A break means the log was
    // edited/corrupted since it was written — warn loudly but don't refuse to
    // start (the operator may be intentionally recovering a damaged log).
    if let Err(e) = log.verify() {
        eprintln!("warning: event log integrity check failed: {e}");
    }

    // Content-addressed artifact store at <workspace>/.medha/artifacts (§4.5).
    let artifacts = Arc::new(store::FileArtifactStore::open(cwd.join(".medha").join("artifacts"))?);

    // Human gate (§4.7): the editor's approval card in ACP mode, the TUI's modal
    // in TUI mode, a y/N prompt in the terminal REPL/one-shot, auto-deny headless.
    // Created early so it can be passed to WorkspaceSandbox for permission prompts.
    let has_task = !prompt.trim().is_empty();
    let is_tty = std::io::stdin().is_terminal();
    let use_acp = cli.acp;
    let use_tui = !use_acp && is_tty && !has_task && !use_plain_repl;

    let tui_channel = if use_tui { Some(tui_tea::channel()) } else { None };
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

    // Workspace = current directory; fs/shell tools use permission system for out-of-workspace access (§4.8).
    // Logs live under <workspace>/.medha/logs/ (created above, alongside events.db/
    // artifacts); the trust lockfile stays at the project root for the user to edit.
    let lock_path = cwd.join("medha.lock");
    // Machine-local permission grants live here, NOT in the portable medha.lock
    // (§13.3): absolute per-machine paths must not travel with the harness
    // artifact. One-time migration moves any legacy [permissions] block out.
    let trust_path = cwd.join(".medha").join("trust.lock");
    lockfile::migrate_permissions_to_trust_file(&lock_path, &trust_path).ok();
    // Keep the whole .medha state dir (trust grants, audit log, snapshots, db)
    // out of version control — write a gitignore once if absent.
    let medha_dir = cwd.join(".medha");
    let gitignore = medha_dir.join(".gitignore");
    if medha_dir.exists() && !gitignore.exists() {
        std::fs::write(&gitignore, "# MEDHA local state — never commit\n*\n").ok();
    }
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
    match std::env::var("MEDHA_SANDBOX").ok().as_deref().map(str::trim) {
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
                eprintln!("warning: [sandbox] backend=container needs an `image` — falling back to the native jail.");
                sbx_cfg.backend = sandbox::BackendKind::Native;
            } else if !sandbox::program_on_path(&runtime) {
                eprintln!("warning: container runtime '{runtime}' not found on PATH — falling back to the native jail.");
                sbx_cfg.backend = sandbox::BackendKind::Native;
            }
        }
        sandbox::BackendKind::Ssh => {
            if sbx_cfg.host.as_deref().unwrap_or("").is_empty() {
                eprintln!("warning: [sandbox] backend=ssh needs a `host` — falling back to the native jail.");
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
        for cache in [".cargo", ".rustup", ".npm", ".cache", ".pnpm-store", ".gradle", ".m2", "go/pkg"] {
            extra_writable.push(home.join(cache));
        }
    }
    let exec_backend = sandbox::select_backend(&sbx_cfg, extra_writable);

    let workspace = Arc::new(
        WorkspaceSandbox::new(cwd.clone(), trust_path, audit_path, Some(gate.clone()))?
            .with_exec_backend(exec_backend),
    );
    let executor = Arc::new(ToolRegistry::with_workspace(workspace.clone(), artifacts.clone()));

    // Context engine: budget-aware two-phase compaction (§4.3), tuned from
    // medha.lock's [context] section (or its built-in-matching default).
    // LLM summarizer for Full compaction (falls back to extractive on failure),
    // so a compacted session keeps a real handoff summary instead of a keyword
    // scrape that invites hallucination.
    let context_engine = Arc::new(
        context::PipelineEngine::new(lock.context.to_policy())
            .with_summarizer(Arc::new(context::LlmSummarizer::new(provider.clone()))),
    );

    // Deny-first policy + shell command scanner (§4.6). Approval set comes from
    // medha.lock's [policy] approve list, extended by MEDHA_APPROVE (e.g.
    // "writes", "shell", "all").
    let policy = Arc::new(policy::DefaultPolicy::requiring_approval(approve_list(
        lock.policy.approve.clone(),
    )));

    // Deterministic verifier (§4.7): medha.lock's [verify] command, overridden
    // by MEDHA_VERIFY="cargo check" if set. Empty/absent = no verifier.
    let verify_cmd = std::env::var("MEDHA_VERIFY").ok().filter(|s| !s.trim().is_empty()).or(lock.verify.command.clone());
    let verifier: Arc<dyn kernel::Verifier> = match verify_cmd {
        Some(cmd) => Arc::new(CommandVerifier { command: cmd, dir: cwd.clone() }),
        None => Arc::new(kernel::NoVerify),
    };

    let base_budget = lock.budget.to_budget();
    let ui_config = lock.ui.clone();
    let kernel = Kernel::new(
        provider, log.clone(), executor, context_engine, artifacts, policy, gate, verifier,
    );

    // K1 Identity sheath is assembled by the context compiler, not hardcoded
    // here; config may override the persona (§4.3).
    let persona = cfg.as_ref().and_then(|c| c.agent.identity.as_deref());
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

    // Resume (--continue / --resume <id>): rebuild the prior conversation from
    // the event log and continue the SAME session (new events append onward).
    // Empty `resumed` = a fresh session.
    let (session, resumed) = match resolve_resume(&log, &cli).await {
        Ok(Some((id, msgs))) => {
            eprintln!("resumed session {id} ({} prior messages)", msgs.len());
            (Session { id, done: false }, msgs)
        }
        Ok(None) => (Session::new(), Vec::new()),
        Err(e) => {
            eprintln!("resume failed: {e} — starting a fresh session");
            (Session::new(), Vec::new())
        }
    };
    let mode = if use_acp { "acp" } else if use_tui { "tui" } else if has_task { "headless" } else { "repl" };
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
                apply_budget_env(base_budget),
                ui_config,
                resumed,
                workspace.clone(),
                tx,
                rx,
            )
            .await?;
        } else if is_tty {
            run_repl(&kernel, &session, system, &model_name, max_ctx, apply_budget_env(base_budget), resumed).await?;
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
    match kernel.run_session(&session, messages, apply_budget_env(base_budget), &sink).await {
        Ok((_t, kernel::StopReason::Budget(stop))) => {
            eprintln!("\n(stopped: {} reached — raise the limit to continue)", stop.label());
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
    let parts: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();

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
            summary: format!("`{}` exit {}", self.command, out.status.code().unwrap_or(-1)),
            output,
        })
    }
}

/// Terminal human gate: print the action + preview, prompt y/N (§4.7).
struct TerminalGate;

#[async_trait::async_trait]
impl kernel::HumanGate for TerminalGate {
    async fn confirm(&self, action: &str, detail: Option<&str>, escalated: bool) -> kernel::Approval {
        println!("\n\x1b[33m⚠ approve {action}?\x1b[0m");
        if escalated {
            println!("\x1b[31m  ⚠ web-tainted action — reviewed every time; 'always' is not offered\x1b[0m");
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
            let err = payload.get("error").and_then(|v| v.as_str()).unwrap_or("error");
            println!("  ⎿ \x1b[31m✗ {err}\x1b[0m");
        } else {
            println!("  ⎿ {}", result_summary(tool, payload));
        }
    }
    fn compaction(&self, before: u32, after: u32, summarized: bool) {
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
        _ => "",
    };
    // Preferred key, else the first string argument as a fallback.
    let val = args
        .get(key)
        .and_then(|v| v.as_str())
        .or_else(|| args.as_object().and_then(|o| o.values().find_map(|v| v.as_str())));
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
    let chars = |k: &str| p.get(k).and_then(|v| v.as_str()).map(|x| x.len()).unwrap_or(0);
    let arr = |k: &str| p.get(k).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    match tool {
        "web.search" => format!("{} results", u("count")),
        "grep" => {
            let t = p.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false);
            format!("{} matches{}", u("count"), if t { " (truncated)" } else { "" })
        }
        "fs.read" => format!("{} chars", chars("content")),
        "fs.list" => format!("{} entries", arr("entries")),
        "fs.write" => format!("wrote {}", s("path")),
        "web.fetch" => {
            let title = s("title");
            let len = chars("content");
            if title.is_empty() { format!("{len} chars") } else { format!("{title} ({len} chars)") }
        }
        "shell.exec" => {
            // Show exit code + a peek at the first non-empty stdout line.
            let first = s("stdout").lines().find(|l| !l.trim().is_empty()).unwrap_or("");
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

/// Interactive session (Vol 7 Stage 2): persistent multi-turn conversation with
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
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
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
async fn resolve_resume(log: &store::SqliteLog, cli: &Cli) -> Result<Option<(ulid::Ulid, Vec<Message>)>> {
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
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

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
                        "status" => print_status(model, max_ctx, usage.load(std::sync::atomic::Ordering::Relaxed)),
                        other => println!("unknown command: /{other}   (try /help)"),
                    }
                    continue;
                }

                transcript.push(Message::user(line));
                // Output (streamed text, ⏺ tool lines, ↯ compaction) renders live
                // through the sink, which also records real token usage.
                let sink = PrintSink::tracking(usage.clone());
                // Each user message is a fresh task → fresh budget contract.
                match kernel.run_session(session, transcript.clone(), budget.clone(), &sink).await {
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
         /effort [low|medium|high]    set reasoning depth (turns thinking on)\n  \
         /clear                       reset the conversation (keep system prompt)\n  \
         /exit                        quit (also Ctrl-D)\n\
         anything else is sent to the agent."
    );
}

/// Apply a `/think` command against the live provider; returns the notice to
/// show. Shared by the plain REPL and the full TUI. Not every model/server
/// has all three effort tiers — an unsupported one is simply not sent
/// downstream (see `ReasoningLockConfig`/`OpenAiCompat::chat_template_kwargs`).
pub(crate) fn effort_label(e: Option<kernel::ReasoningEffort>) -> &'static str {
    match e {
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
            provider.set_reasoning(kernel::ReasoningConfig { enabled: Some(true), effort });
            "thinking: on".to_string()
        }
        "off" => {
            provider.set_reasoning(kernel::ReasoningConfig { enabled: Some(false), effort: None });
            "thinking: off".to_string()
        }
        other => format!("usage: /think [on|off|status]  (got '{other}') — use /effort for reasoning level"),
    }
}

fn think_status<P: kernel::Provider>(provider: &P) -> String {
    let cfg = provider.reasoning();
    let enabled = match cfg.enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "server default",
    };
    format!("thinking: {enabled}  |  effort: {}", effort_label(cfg.effort))
}

/// `/effort [low|medium|high]` — set reasoning depth; also turns thinking on
/// (an effort level only means anything once thinking is enabled). In the
/// full TUI, calling this with no args opens an arrow-key picker instead of
/// requiring the name to be typed.
pub(crate) fn apply_effort_command<P: kernel::Provider>(provider: &P, args: &str) -> String {
    match args.trim() {
        "low" | "medium" | "high" => {
            let level = args.trim();
            let effort = match level {
                "low" => kernel::ReasoningEffort::Low,
                "medium" => kernel::ReasoningEffort::Medium,
                _ => kernel::ReasoningEffort::High,
            };
            provider.set_reasoning(kernel::ReasoningConfig { enabled: Some(true), effort: Some(effort) });
            format!(
                "effort: {level} (thinking: on) — sent if this server supports it, \
                 otherwise silently ignored"
            )
        }
        "" => "usage: /effort [low|medium|high]".to_string(),
        other => format!("usage: /effort [low|medium|high]  (got '{other}')"),
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
