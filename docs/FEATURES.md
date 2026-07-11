# MEDHA — Feature Reference

A complete reference of every shipped feature, with where it lives in the code.
Nothing here is aspirational — if a feature is listed, the code implements it.
Roadmap items are in [`../PROGRESS.md`](../PROGRESS.md), not here.

---

## 1. Tool families

All tools live in [`crates/tools/src/lib.rs`](../crates/tools/src/lib.rs), behind
the `Tool` trait, registered by `ToolRegistry::with_workspace` (22 tools total,
including the background-task `task.output` / `task.kill` / `task.list`).
The registry implements the kernel's `Executor` trait, so the kernel never knows
an individual tool exists — only the `specs()` it exposes and the
`blast_radius()` / `category()` each declares.

Every tool call is wrapped in a **60-second timeout** (`TOOL_TIMEOUT`) and
returns a structured `Observation` — a timeout is data the model reasons about,
never a hang (`tools/lib.rs:31`, `lib.rs:200`).

### File system — `fs.*`

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `fs.read` | Read | Read | Read a UTF-8 file; supports offset/limit slicing for large files. |
| `fs.write` | ReversibleLocal | Write | Write a whole file (snapshots the prior version first). |
| `fs.list` | Read | Read | List immediate directory entries (dirs suffixed `/`). |
| `fs.edit` | ReversibleLocal | Write | Exact-substring replace in one file; fails on ambiguity unless `replace_all`. Returns a unified diff. |
| `multi_edit` | ReversibleLocal | Write | Several exact-substring edits to one file, atomically (all-or-nothing). |

`fs.edit` and `multi_edit` both produce a unified diff (`make_diff`, via the
`similar` crate) and a snapshot id, surfaced to the approval gate and the sink.

### Search & discovery

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `grep` | Read | Search | Regex search across the workspace, gitignore-aware, with context lines. |
| `glob` | Read | Search | Find files by name pattern (recursive, gitignore-aware). |
| `code_outline` | Read | Read | Extract a symbol map (functions/classes/structs + line numbers) from a source file. Heuristic, line-based. |
| `references` | Read | Search | Whole-word symbol occurrences across the workspace. |
| `tree` | Read | Read | Indented, depth-limited directory tree (gitignore-aware). |
| `word_count` | Read | Read | Words, lines, characters in a file. |

### Shell — `shell.exec`

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `shell.exec` | IrreversibleLocal | Shell | Run any shell command via `sh -c`, rooted at the workspace; captures stdout/stderr/exit code. |

Runs through the sandbox's `ExecBackend` (host or OS-native jail). The child
starts from a **cleared environment** with an explicit allowlist (`shell_env`,
`lib.rs:1746`) — `PATH`, `HOME`, toolchain locators (`CARGO_HOME`, etc.) — so
injected API keys never reach an arbitrary command. `kill_on_drop` ensures a
cancelled/timed-out command never leaks an orphan.

### Web — `web.*`

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `web.search` | Read | Web | Query the web. Backend cascade (see below). |
| `web.fetch` | Read | Web | Fetch one URL → Markdown (plain fetch, Tavily Extract fallback). HTML→Markdown, plus PDF text extraction. |
| `web.crawl` | Read | Web | Crawl multiple pages under one root (Tavily; requires `TAVILY_API_KEY`). |

**Search backend cascade** (`web.search`, tried in order): Tavily (if
`TAVILY_API_KEY` set) → Brave (if `BRAVE_API_KEY`) → a configured SearXNG
(`MEDHA_SEARXNG_URL`) → DuckDuckGo (scraped, no key). DuckDuckGo tries the
`lite` endpoint before `html`. All backends normalize to `{title, url, snippet}`
results.

Web output is stamped `TrustLabel::Web` (untrusted content) and taints the
request for trust-flow escalation (see §2). A browser User-Agent is sent because
search engines block non-browser agents.

**SSRF protection** (`validate_public_url`, `lib.rs:1012`): only `http`/`https`
schemes; the host is resolved and every address checked against a blocklist
(loopback, RFC1918, link-local incl. `169.254.169.254`, CGNAT, unspecified,
multicast, IPv6 equivalents incl. IPv4-mapped). Re-checked on every redirect
hop. Response bodies are size-capped and streamed (`read_body_capped`).

### Git — `git`

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `git` | ReversibleLocal | Vcs | Structured git: `status`/`diff`/`log`/`blame`/`show` (reads, free) and `add`/`commit` (approval-gated). |

Read subcommands are allowed by policy; `add`/`commit` route to the human gate;
everything else (push/reset/rebase) is denied. Paths go after `--` so they can
never be read as flags; revisions are validated by `safe_git_arg`. Branch
operations are intentionally out of scope — use `shell.exec`.

### Diagnostics — `diagnostics`

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `diagnostics` | Read | Diagnostic | Run the project's linter/typechecker and return structured findings. |

Auto-detects the language from project files (`detect_lang`, `lib.rs:2139`):
`Cargo.toml` → Rust (`cargo check --message-format=json`), `tsconfig.json` →
TypeScript (`tsc --noEmit`), `pyproject.toml`/`ruff.toml` → Python
(`ruff check --output-format=json`). Parses each into `{severity, file, line,
column, code, message}`. Can force a language via the `language` arg.

### Planning & artifacts

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `update_plan` | ReversibleLocal | Plan | Maintain a TODO list; the agent's planning tool and the user's live progress view. |
| `read_artifact` | Read | Read | Page through a large/earlier tool output that was spilled to the artifact store (by hash, byte range). |

`update_plan` is the only `Plan`-category tool; the system prompt
(`crates/context/prompts/system.md`) instructs the model to call it for any
3+ step task.

---

## 2. The security model

### Deny-first policy
**Where:** [`crates/policy/src/lib.rs`](../crates/policy/src/lib.rs) (`DefaultPolicy`)

Every tool intent passes `authorize(intent, blast_radius)` before execution.
Authorization is driven by the tool's **declared blast radius**, not a hardcoded
name list: `Read` and `ReversibleLocal` are allowed; `IrreversibleLocal` and
`External` route to the human gate; an unregistered tool (radius `None`) is
denied. A configurable `approve` set escalates otherwise-allowed tools (e.g.
`fs.write`, `fs.edit`) to the human gate.

### Fail-closed shell scanner
**Where:** `crates/policy/src/lib.rs` — `scan_command` / `hard_dangerous` / `needs_review`

`shell.exec` commands are scanned before execution:
- **Hard-deny** (`hard_dangerous`): fork bombs, `mkfs`, raw disk writes,
  `rm -rf` on an absolute/home path, credential reads (`/etc/shadow`, `id_rsa`,
  `.aws/credentials`, `.kube/config`, …), `sudo`, decode-then-pipe-to-shell
  (`curl … | sh`, `base64 -d | sh`).
- **Escalate to human** (`needs_review`): command substitution (`$()`, backticks,
  `<()`), backslash escaping, network upload/exfiltration (`curl -d`, `scp`,
  `rsync`, `nc`), raw sockets (`/dev/tcp/`), environment dumps (`printenv`).
- **Allow**: everything else.

The principle is **fail-closed on ambiguity** — anything the static scan can't
see through routes to the human gate, never silently allowed.

### OS-native sandbox
**Where:** [`crates/sandbox/src/exec.rs`](../crates/sandbox/src/exec.rs) — the `ExecBackend` trait and four backends.

| Backend | Platform | Containment | Notes |
|---------|----------|-------------|-------|
| `HostBackend` | all | `None` | Runs directly on the host. Fallback; `--no-sandbox` / `MEDHA_SANDBOX=host`. |
| `SeatbeltBackend` | macOS | `OsFsJail` / `OsFsJailNoNet` | `/usr/bin/sandbox-exec` with a generated SBPL profile: allow-default, deny all writes, re-allow under workspace + temp + `/dev`. Zero external deps. |
| `LandlockBackend` | Linux ≥5.13 | `OsFsJail` | Landlock LSM ruleset in a `pre_exec` hook; read+exec everywhere, write only under jailed roots. Best-effort (degrades to host if unsupported). |
| `ContainerBackend` | all (needs docker/podman) | `OsFsJail` / `OsFsJailNoNet` | Throwaway container; workspace bind-mounted at `/workspace`, caps dropped, `no-new-privileges`. **Host env is not forwarded** (API keys stay put). |
| `SshBackend` | all (needs ssh) | `None` | Run each command on a remote `host` over ssh. Remote execution, not local isolation. |

`select_backend()` picks from config; misconfigured container/ssh falls back to
the native jail with a warning. `native_backend_available()` probes live Landlock
support so the CLI warns honestly when the jail degrades to host.

The filesystem jail (path resolution) lives in
[`crates/sandbox/src/lib.rs`](../crates/sandbox/src/lib.rs): `canonicalize_within_root`
resolves symlinks and re-checks under the canonical root after resolution, so an
in-workspace symlink pointing out cannot escape (tested).

### Environment clearing
**Where:** `crates/tools/src/lib.rs` — `shell_env` (`lib.rs:1746`); `crates/sandbox/src/exec.rs` — `ExecRequest.clear_env`.

`shell.exec` sets `clear_env: true`, so the child starts from an empty
environment with only an allowlist of non-secret vars (`PATH`, `HOME`,
`CARGO_HOME`, `RUSTUP_HOME`, …). Fixed-program tools (git, diagnostics) inherit
the env instead. This means `printenv` or `echo $KEY` from a model-run command
cannot exfiltrate the provider API key.

### Containment-coupled trust-flow
**Where:** [`crates/kernel/src/loop_.rs`](../crates/kernel/src/loop_.rs) — `escalate_for_trust_flow` (with tests).

When a `web.*` tool returns, its observation is stamped `TrustLabel::Web` and a
per-request `web_tainted` flag flips true. For the rest of that request, any
**consequential** action (`IrreversibleLocal` or `External` blast radius) is
auto-escalated from `Allow` to `Human` **unless** the sandbox's containment
blocks network exfiltration (`confines_network()`). The escalation only ever
*tightens* — it never relaxes a denial or an existing gate.

### Tamper-evident event log
**Where:** [`crates/kernel/src/events.rs`](../crates/kernel/src/events.rs) + [`crates/store/src/lib.rs`](../crates/store/src/lib.rs)

Every event carries a `prev_hash`; `chain_hash` is SHA-256 over
`(prev_hash ‖ kind ‖ session ‖ payload ‖ ts)`. The `SqliteLog` (WAL mode) stores
both `prev_hash` and the computed `hash` per row; `verify()` recomputes the
chain over the entire log and detects any direct row edit — including the last
row. The CLI calls `verify()` on resume and warns loudly (but doesn't refuse to
start) if the chain is broken.

---

## 3. `medha.lock` — the harness config surface
**Where:** [`crates/lockfile/src/lib.rs`](../crates/lockfile/src/lib.rs); example at [`medha.lock.example`](../medha.lock.example).

A TOML file at the project root that versions the cognitive configuration with
the code. **Absent file = built-in defaults** (identical to pre-lock behavior).
Precedence: **env var > medha.lock > built-in default**.

| Section | Fields | What it controls |
|---------|--------|------------------|
| `[routing]` | `executor`, `verifier` | Provider seats. Only `executor` is consulted today; `verifier` is a placeholder for cross-vendor verification (not yet built). |
| `[budget]` | `max_turns`, `max_tokens`, `max_cost_usd`, `max_wall_s` | Per-task ceilings enforced by the `Governor` before each turn. Default: `max_turns = 200`. |
| `[context]` | `trigger_ratio`, `microcompact_ratio`, `tail_ratio`, `protect_first_n`, `protect_last_n`, `prune_min_tool_tokens`, `emergency_ratio` | Compaction tuning. |
| `[policy]` | `approve` | Tool classes requiring human approval (default: `["fs.write", "fs.edit"]`). |
| `[sandbox]` | `backend`, `network`, `image`, `runtime`, `memory`, `pids`, `host`, `remote_dir`, `extra_writable` | Execution backend + network posture. Default: `backend = "native"`, `network = "allow"`. |
| `[verify]` | `command` | Deterministic check run after file-modifying turns (e.g. `cargo check`). Empty = none. |
| `[reasoning]` | `enabled`, `effort` | Request-side thinking control for reasoning-capable models. |
| `[ui]` | `show_thinking`, `full_transparency` | TUI presentation defaults. |

Machine-local trust grants (out-of-workspace path permissions) live in
`.medha/trust.lock`, **not** in the portable `medha.lock` — absolute per-machine
paths must not travel with the harness artifact. A one-time migration moves any
legacy `[permissions]` block out of `medha.lock`.

Session-level env overrides: `MEDHA_MAX_TURNS`, `MEDHA_MAX_TOKENS`,
`MEDHA_MAX_COST`, `MEDHA_MAX_WALL`, `MEDHA_APPROVE`, `MEDHA_VERIFY`,
`MEDHA_SANDBOX`, `MEDHA_MAX_CTX`.

---

## 4. The TUI & surfaces
**Where:** [`crates/medha-cli/src/tui_tea.rs`](../crates/medha-cli/src/tui_tea.rs) (full-screen TUI, `ratatui` + `crossterm`); [`crates/medha-cli/src/main.rs`](../crates/medha-cli/src/main.rs) (`PrintSink`, plain REPL, headless); [`crates/medha-cli/src/acp.rs`](../crates/medha-cli/src/acp.rs) (editor bridge).

Four run modes, chosen automatically:
- **Full-screen TUI** — default when there's a terminal and no task arg. A
  `ratatui` app with streaming text, tool-call lines (salient arg only, never
  raw JSON), rendered diffs, an approval modal, a live context-pressure gauge
  in the status bar, and an activity label ("writing medha.html…").
- **Plain REPL** — `--plain`; a scrolling `rustyline` session with a
  `[NN% ctx]›` pressure prompt.
- **Headless one-shot** — `medha "task"`; streams output to stdout, no UI.
- **ACP editor bridge** — `--acp`; line-delimited JSON-RPC 2.0 over stdio so an
  editor extension (VS Code, Zed, JetBrains) can embed MEDHA. Same kernel, only
  the surface differs.

### TUI keybindings (as implemented)
| Key | Action |
|-----|--------|
| `Enter` | Send message (or run slash command) |
| `Shift`/`Alt`+`Enter`, `Ctrl`+`J` | Newline |
| `Ctrl`+`C` | Clear input (when not running) |
| `Ctrl`+`D` | Quit |
| `Esc` | Interrupt the running turn |
| `↑`/`↓` | History navigation (or scroll when input empty) |
| `PgUp`/`PgDn` | Scroll by 5 |
| `Home`/`End` | Scroll to top/bottom |
| `1`/`y`, `2`/`a`, `3`/`n`, `Enter` | Approval modal: once / always / deny |

### Slash commands
| Command | Plain REPL | TUI | Action |
|---------|:---:|:---:|--------|
| `/help` | ✅ | ✅ | Show commands |
| `/status` | ✅ | ✅ | Model, context window, pressure, thinking state |
| `/think [on\|off\|status]` | ✅ | ✅ | Enable/disable reasoning |
| `/effort [low\|medium\|high]` | ✅ | ✅ | Set reasoning depth (bare = arrow-key picker in TUI) |
| `/clear` | ✅ | — | Reset conversation (keep system prompt) |
| `/thinking` | — | ✅ | Show/hide the model's live reasoning |
| `/detail` | — | ✅ | Expand/collapse full tool input & output |
| `/exit` `/quit` | ✅ | ✅ | Quit (also `Ctrl`+`D`) |

---

## 5. The provider layer
**Where:** [`crates/providers/src/openai_compat.rs`](../crates/providers/src/openai_compat.rs) (adapter), [`crates/providers/src/models_dev.rs`](../crates/providers/src/models_dev.rs) (discovery).

### OpenAI-compatible adapter (`OpenAiCompat`)
One adapter, parametrized by `base_url`, covers the open ecosystem: local
(vLLM, SGLang, Ollama, llama.cpp, LM Studio) and hosted (OpenRouter, Together,
Groq, OpenAI). Uses `rustls` (no OpenSSL) so the binary is self-contained.

- **Streaming** — real SSE parsing of `POST /v1/chat/completions` with
  `stream: true`; yields canonical `Block`s (Text, Reasoning, ToolIntent,
  ToolStarted, Usage) token-by-token, not buffered.
- **Reasoning support** — handles all three shapes of reasoning delivery
  (separate `reasoning_content`/`reasoning` delta field, inline `‹think›` tags
  in content, and none), normalized to `Block::Reasoning` on a separate channel
  from the answer. See [`docs/REASONING_STREAMING.md`](REASONING_STREAMING.md).
- **Request-side reasoning control** — `ReasoningConfig {enabled, effort}` maps
  to `chat_template_kwargs` (`enable_thinking`, `reasoning_effort`) and is
  silently omitted for servers that don't support a given knob. Set via
  `[reasoning]` in `medha.lock` or `/think` `/effort` live.
- **Exact token counting** — discovers the host's tokenization route on first
  use (vLLM `/tokenize` or Anthropic-style `/messages/count_tokens`), caches it,
  and returns `None` when none exists (falls back to a local estimate). The
  post-turn `usage` is always authoritative.
- **Tool calls** — K2 specs exposed as OpenAI tool definitions. Native tool
  calls only today (`ToolCallStrategy::Native`); guided/constrained decoding is
  declared but not implemented.

### Context-window discovery
Resolved without the user typing a number, never fabricated (precedence):
1. explicit `MEDHA_MAX_CTX` / config override
2. `/v1/models` discovery (server-authoritative)
3. models.dev (a real, externally maintained metadata database; cached to
   `~/.medha/models_dev_cache.json` with a 7-day TTL)
4. unknown → compaction disabled, stated honestly

### Provider configuration
**Where:** [`crates/medha-cli/src/config.rs`](../crates/medha-cli/src/config.rs)

Resolution order: **CLI flag → env var → `~/.medha/config.toml` → first-run
wizard.** The wizard queries `/v1/models` and offers a model picker. API keys
are stored in the **OS keychain** (`keyring` crate), never in the TOML. Env
names accept `MEDHA_*` and the common `OPENAI_COMPATIBLE_*` / `OPENAI_*`
spellings.

---

## 6. Context & compaction
**Where:** [`crates/context/`](../crates/context/) — `compactor.rs`, `engine.rs`, `policy.rs`, `tokens.rs`, `identity.rs`, `prompts/`.

### Two-phase compaction
**Where:** `crates/context/src/compactor.rs`

Graduated escalation by pressure (`decide`): under `microcompact_ratio` → none;
at `microcompact_ratio` → **Phase 1 prune** (deterministic, lossless); at
`trigger_ratio` → **Phase 2 summarize** (LLM, with an `ExtractiveSummarizer`
offline fallback).

- **Lossless pruning** — pruned tool outputs are replaced with a placeholder
  pointing at their artifact hash; the full content is re-fetchable via
  `read_artifact`. Only the live window shrinks, not the truth.
- **Protected head/tail** — the first N items and a token-budgeted tail are
  never compacted; only the middle is touched.
- **Lineage** — every summary carries the `source_events` it covers.
- **Iterative re-summary** — a previous summary is passed in and updated, not
  restarted.

The `PipelineEngine` compiles a budget-fitted view each turn; the kernel carries
the compacted view forward so history stays bounded and isn't recompacted every
turn. The full originals remain in the durable hash-chained log.

### Token counting
**Where:** `crates/context/src/tokens.rs`

`HeuristicCounter` (local estimate) and `BpeCounter`. The engine prefers the
provider's exact count when available (fed via `update_usage`), else the
heuristic.

### Identity (K1)
**Where:** `crates/context/src/identity.rs` + `crates/context/prompts/system.md`

The system prompt is assembled from a config persona override or the built-in
`system.md`, then grounded with the real current date and workspace path (so the
model doesn't guess a stale year for time-sensitive queries).

---

## 7. Persistence & state
**Where:** [`crates/store/src/lib.rs`](../crates/store/src/lib.rs), [`crates/permissions/src/lib.rs`](../crates/permissions/src/lib.rs).

Under `<workspace>/.medha/` (gitignored by default — a `.gitignore` is written on
first run):

| Path | What |
|------|------|
| `events.db` | SQLite (WAL) hash-chained event log — the single source of truth. |
| `artifacts/` | Content-addressed blob store (SHA-256 named); large tool outputs spill here. Path-traversal-safe (`safe_path` rejects non-hex hashes). |
| `snapshots/` | Pre-write file snapshots (ULID-named) — the basis for a future `medha undo`. |
| `trust.lock` | Machine-local out-of-workspace path permission grants (never committed). |
| `logs/medha.log` | Structured `tracing` log (file, never stdout — the TUI owns the screen). |
| `logs/audit.log` | Audit log of out-of-workspace access attempts. |

A second cache is **global, not per-project**: `~/.medha/models_dev_cache.json`
holds the models.dev context-window metadata (shared across workspaces, since
it's model metadata, not project state — `models_dev.rs:45`).

### Out-of-workspace permissions
**Where:** `crates/permissions/src/lib.rs` — `PermissionManager`

A live ask-then-persist flow for files outside the workspace jail:
1. resolve the target path fully;
2. allow immediately if inside the workspace root;
3. check `trust.lock` for pre-granted paths;
4. if not trusted, prompt via the `HumanGate` (once / always / deny);
5. "always" grants persist to `trust.lock`;
6. read and write permissions are separate;
7. every out-of-workspace access is audited.

Headless / `AutoDeny` mode denies all out-of-workspace access (fail-closed).
