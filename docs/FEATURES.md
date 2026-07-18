# MEDHA — Feature Reference

A complete reference of every shipped feature, with where it lives in the code.
Nothing here is aspirational — if a feature is listed, the code implements it.
Roadmap items are in [`../PROGRESS.md`](../PROGRESS.md), not here.

---

## 1. Tool families

All tools live in [`crates/tools/src/lib.rs`](../crates/tools/src/lib.rs), behind
the `Tool` trait, registered by `ToolRegistry::with_workspace` plus the skill
tools (including background-task `task.output` / `task.kill` / `task.list` and
the human-in-the-loop `clarify`).
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

### Skills — `skill.load`, `skill.list`, `skill.save`

Project skills live at `.medha/skills/<name>/SKILL.md`; user skills live under
`$MEDHA_HOME/skills` (or `~/.medha/skills`). Project skills shadow user skills
with the same name. The system prompt receives a compact manifest and loads a
procedure only on demand with `skill.load`; `skill.list` keeps catalogs larger
than 30 skills discoverable without loading every procedure. Metadata is
validated as bounded, single-line manifest data. `skill.save` always requires a
human approval and previews the complete file before it is written.

### Human-in-the-loop — `clarify`
**Where:** `Clarify` in [`crates/tools/src/lib.rs`](../crates/tools/src/lib.rs); the `Asker` seam in [`crates/kernel/src/clarify.rs`](../crates/kernel/src/clarify.rs); TUI form in [`crates/medha-cli/src/tui_tea/`](../crates/medha-cli/src/tui_tea/).

| Tool | Blast radius | Category | What it does |
|------|-------------|----------|--------------|
| `clarify` | Read | Plan | Ask the user 1–4 structured questions BEFORE proceeding, each with 2–5 options (one may be `recommended`), radio or `multi_select` checkbox, plus a free-text "Other". Returns the choices. |

When a task is materially ambiguous, the agent calls `clarify` and the TUI renders
an inline form: **↑↓** move within options, **←→** switch between questions,
**Space** picks/toggles (radio vs checkbox), **Enter** submits all, **Esc**
dismisses. The recommended option is pre-selected. It's a blocking surface
round-trip — the mirror of the `HumanGate` approval card, but for questions —
carried by a `TuiEvent::Clarify(questions, responder)` + a `oneshot` reply.

Because `clarify` is **Read**-radius, the policy never gates it, so it works the
same at every autonomy level — including `yolo`. That composition is the point:
**in `yolo` the agent clarifies its doubts up front, then runs fully autonomous**
("ask once, then go"). No interactive surface (headless / ACP) → the tool returns
`{skipped:true}` and the agent proceeds on best judgment; it never hangs.

---

## 2. The security model

### Deny-first policy
**Where:** [`crates/policy/src/lib.rs`](../crates/policy/src/lib.rs) (`DefaultPolicy`)

Every tool intent passes `authorize(autonomy, intent, blast_radius)` before
execution. Authorization is driven by the tool's **declared blast radius**, not a
hardcoded name list: `Read` and `ReversibleLocal` are allowed; `IrreversibleLocal`
and `External` route to the human gate; an unregistered tool (radius `None`) is
denied. A configurable `approve` set escalates otherwise-allowed tools (e.g.
`fs.write`, `fs.edit`) to the human gate.

### The autonomy dial (`/mode`) — and its unremovable floor
**Where:** `AutonomyLevel` in [`crates/kernel/src/types.rs`](../crates/kernel/src/types.rs); applied in `DefaultPolicy::authorize` ([`crates/policy/src/lib.rs`](../crates/policy/src/lib.rs)).

A per-session dial controls **how much runs without asking**, switchable live in
the TUI with `/mode` (an arrow-key picker), set at startup by `[policy] autonomy`
in `medha.lock`, or overridden by `MEDHA_MODE`:

| Level | Reversible edits (fs.write/edit) | Safe shell (build/test) |
|-------|----------------------------------|-------------------------|
| `careful` *(default)* | ask | ask |
| `normal` | auto | ask |
| `yolo` | auto | auto |

**The dial can only ever turn `Allow`→`Human` — never the reverse.** It is
structurally incapable of loosening the base decision, so the **safety floor is
identical at every level, including `yolo`**: the dangerous-command scanner
(`rm -rf /`, `mkfs`, `sudo`, `curl|sh`, credential reads) still hard-denies;
`IrreversibleLocal`/`External` actions and `git commit` still route to the human
gate; out-of-workspace/personal-file access is still gated by the permission
manager; web-tainted actions still escalate via trust-flow. A floor action that
resolves to `Human` is **asked** interactively and **denied** headless (AutoDeny) —
so `MEDHA_MODE=yolo` unattended can never execute `rm -rf ~`, it's refused.
(Verification is a *separate* axis — see the Eval Gate §8; `/verify` is future.)

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
| `[memory]` | `enabled`, `k3_budget_tokens`, `write_approval`, `stale_after_days` | Frozen recall budget, write gate, and deterministic staleness. |
| `[context_files]` | `enabled`, `max_chars`, `progressive_discovery` | Guarded startup and progressive project instructions. |
| `[policy]` | `approve`, `autonomy` | `approve`: tool classes requiring human approval. `autonomy`: starting dial `careful`/`normal`/`yolo` (default `careful`; live via `/mode`, override `MEDHA_MODE`). |
| `[sandbox]` | `backend`, `network`, `image`, `runtime`, `memory`, `pids`, `host`, `remote_dir`, `extra_writable` | Execution backend + network posture. Default: `backend = "native"`, `network = "allow"`. |
| `[verify]` | `command` | Deterministic check run after file-modifying turns (e.g. `cargo check`). Empty = none. |
| `[reasoning]` | `enabled`, `effort` | Request-side thinking control for reasoning-capable models. |
| `[ui]` | `show_thinking`, `full_transparency` | TUI presentation defaults. |
| `[gate]` | `scenarios_dir`, `pass_threshold`, `seeds`, `regression_epsilon` | Eval-gate policy for `medha gate` (§8). Defaults: `scenarios`, `1.0`, `1`, `0.0`. |

Machine-local trust grants (out-of-workspace path permissions) live in
`.medha/trust.lock`, **not** in the portable `medha.lock` — absolute per-machine
paths must not travel with the harness artifact. A one-time migration moves any
legacy `[permissions]` block out of `medha.lock`.

Session-level env overrides: `MEDHA_MAX_TURNS`, `MEDHA_MAX_TOKENS`,
`MEDHA_MAX_COST`, `MEDHA_MAX_WALL`, `MEDHA_APPROVE`, `MEDHA_MODE`, `MEDHA_VERIFY`,
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
| `/mode` | — | ✅ | Autonomy dial picker (careful/normal/yolo) — how much runs without asking; floor stays gated |
| `/reasoning` | — | ✅ | Unified mode, visibility, effort, and delivery-status panel |
| `/reasoning on\|off\|show\|hide\|status` | — | ✅ | Configure reasoning explicitly |
| `/reasoning effort auto\|low\|medium\|high` | — | ✅ | Set reasoning depth (`low`/`medium`/`high` also enable it) |
| `/think`, `/effort`, `/thinking` | ✅ | ✅ | Compatibility aliases (hidden from TUI help/autocomplete) |
| `/clear` | ✅ | — | Reset conversation (keep system prompt) |
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
  from the answer.
- **Request-side reasoning control** — `ReasoningConfig {enabled, effort}` maps
  to `chat_template_kwargs` (`enable_thinking`, `reasoning_effort`) and is
  silently omitted for servers that don't support a given knob. Set via
  `[reasoning]` in `medha.lock` or the unified `/reasoning` panel live.
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

Resolution order: **CLI flag → env var → `~/.medha/config.toml` → TUI
first-run model setup** (`medha --setup` opens the same form; the old terminal
wizard is gone — one setup surface). Unconfigured headless runs fail fast with
an actionable error instead of prompting. API keys
never live in the TOML; they resolve through a layered secret store: **env var
→ `~/.medha/credentials.toml` (owner-only 0600, enforced on every write) → OS
keychain (`keyring` crate)**. File-first is deliberate: macOS binds keychain
ACLs to the binary's code signature, so keychain-first storage throws a
password dialog on every rebuild of an ad-hoc-signed dev binary (and headless
Linux has no keychain at all). Legacy keychain keys migrate into the file on
first read. `MEDHA_CRED_STORE=keychain` at runtime — or compiling with
`MEDHA_DEFAULT_CRED_STORE=keychain`, intended for stably-signed release
builds — restores keychain-first. Found keys are cached in-process — at most
one store lookup per endpoint per run. Env names accept `MEDHA_*` and the
common `OPENAI_COMPATIBLE_*` / `OPENAI_*` spellings.

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

### Typed memory and context files

**Where:** `crates/memory/`, `crates/context/src/ctxfiles.rs`,
`crates/tools/src/memory_tools.rs`, and `crates/store/src/lib.rs`.

#### Architecture

```text
visible turn
    │
    ├─ memory.write/update/forget
    │       └─ kernel computes trust + confidence + provenance
    │
    └─ hash-chained EventKind::MemoryWrite       single source of truth
            ├─ project/user SQLite + FTS5         rebuildable projection
            │      ├─ frozen K3 index             prompt recall
            │      └─ memory.search               full-entry recall
            └─ event-text FTS5 ─ sessions.search  verbatim episodic recall
```

Workspace logs own memory history. Project memory is rebuilt from the current
workspace log; user memory is incrementally maintained in the global projection,
while its provenance event remains in the workspace where it was written.
Recall merges both scopes, with project scope winning a name collision. Forking
reconstructs branch-specific project memory; user-global memory remains visible
until it is explicitly forgotten in user scope.

K3 is compiled once at session start. Entries rank by pinned status, trust, and
recency, and the rendered block cannot exceed `[memory].k3_budget_tokens`.
Mid-session writes are immediately searchable but do not alter K3 until a new
session or Full compaction. Stale candidates leave K3 without being deleted;
pinned entries remain and stale results carry a verification warning.

Trust is a kernel boundary. Tool arguments contain the claim and classification,
but never trust, confidence, or provenance. The kernel derives them from turn
taint, so web/tool evidence cannot promote itself to user trust. Contradictions
append a new version and produce reconciliation choices instead of silently
overwriting the prior claim.

#### What M1–M7 added

| Milestone | Shipped behavior |
|---|---|
| M1 | Typed entries and memory events; SQLite/FTS5 projection; deterministic log rebuild and fork-aware state. |
| M2 | `memory.write`, `memory.update`, and `memory.forget`; kernel-owned trust/provenance; duplicate and contradiction handling; user-scope approval gate. |
| M3 | Token-bounded frozen K3 recall, stable `## Memory` replacement, staleness stamps, Full-compaction refresh, and `memory.search`. |
| M4 | Structured consolidation pressure, terminal success responses, three-failure cap, and stale-candidate decay. |
| M5 | FTS5 mirror of text-bearing events and verbatim `sessions.search` discover/scroll/browse modes with automation demotion. |
| M6 | Guarded startup context chain, bounded progressive discovery, load events, and global `PERSONA.md` through K1. |
| M7 | Memory CLI, TUI `/memory`, provenance jumps, reconciliation card, lockfile settings, and fork-aware end-to-end coverage. |

#### Prompt composition

No base prompt edit is required for memory or context files. Startup assembles
the effective system message in this order:

1. built-in identity, or `$MEDHA_HOME/PERSONA.md` through the K1 override;
2. guarded `## Project context` from context files;
3. the skills manifest;
4. the frozen `## Memory` K3 block.

Do not paste memory entries, `MEDHA.md`, or `PERSONA.md` into
`crates/context/prompts/system.md`. Runtime assembly keeps data current,
preserves event provenance, and keeps the K3 prefix byte-stable. Change the base
prompt only when MEDHA's universal behavior should change for every user and
workspace, not when adding memory or project instructions.

Context files are instructions, not memories. Startup selects the first existing
file per directory in `MEDHA.md` → `AGENTS.md` → `CLAUDE.md` order while walking
cwd to git root, then adds `$MEDHA_HOME/MEDHA.md`. Files are guard-scanned and
limited to 20K characters using a 70/20 head/tail split. Progressive discovery
checks at most five ancestors once per directory and appends at most 8K
characters to a tool observation with workspace trust. Blocked files produce a
visible notice. `$MEDHA_HOME/PERSONA.md` is global and never sourced from cwd.

#### Files and storage locations

`$MEDHA_HOME` means the `MEDHA_HOME` environment variable when set, otherwise
`~/.medha`. Runtime state stays outside the repository. For example, workspace
`/Users/me/code/app` maps to
`~/.medha/projects/-Users-me-code-app/`.

| File or directory | Who creates it | Meaning |
|---|---|---|
| `<repo>/MEDHA.md` | You, optional and recommended | Project instructions. Commit it when the team should share it. |
| `<repo>/AGENTS.md` | You or an existing tool, optional | Compatibility fallback when that directory has no `MEDHA.md`. |
| `<repo>/CLAUDE.md` | You or an existing tool, optional | Compatibility fallback when that directory has neither `MEDHA.md` nor `AGENTS.md`. |
| `<repo>/<subdir>/MEDHA.md` | You, optional | More specific instructions, discovered when MEDHA works in that subtree. |
| `$MEDHA_HOME/MEDHA.md` | You, optional | User-global instructions appended in every workspace. |
| `$MEDHA_HOME/PERSONA.md` | MEDHA seeds it; you customize it | Global K1 identity and communication style. Comment-only seed keeps the built-in persona. |
| `<repo>/medha.lock` | You, optional | Versioned memory/context limits and policy; defaults apply when absent. |
| `$MEDHA_HOME/projects/<encoded-workspace>/events.db` | MEDHA | Workspace hash-chained log and provenance source. Never edit directly. |
| `$MEDHA_HOME/projects/<encoded-workspace>/memory.db` | MEDHA | Rebuildable project-memory SQLite/FTS5 projection. Never edit directly. |
| `$MEDHA_HOME/memory.db` | MEDHA | User-global SQLite/FTS5 memory projection shared across workspaces. Never edit directly. |
| `$MEDHA_HOME/projects/<encoded-workspace>/memory-pending/` | MEDHA when approval is staged | Pending operations consumed by `medha memory approve <id>`. |

Only one context filename wins in each directory. If all three exist beside one
another, MEDHA loads `MEDHA.md` and ignores `AGENTS.md` and `CLAUDE.md` there.
It can still load the winning file from each ancestor directory up to the git
root. You do not need to copy the same instructions into all three files.

There is no user-authored `MEMORY.md`. To save a project fact, call
`memory.write` with `scope: "project"` (the default). MEDHA appends the operation
to that workspace's `events.db` and updates its project `memory.db`. A project
fork rebuilt before that event will not contain the fact.

To save a preference across repositories, call `memory.write` with
`scope: "user"`. The operation is still recorded in the originating workspace's
`events.db` for provenance, but its projection is `$MEDHA_HOME/memory.db` and it
appears in K3/search for every workspace. The default `write_approval =
"user-scope"` applies the human gate. Rewinding one project does not remove a
global user preference; use `medha memory forget <name> --scope user`.

Useful scope checks:

```sh
medha memory list --scope project
medha memory list --scope user
medha memory show <name> --scope project
medha memory show <name> --scope user
medha memory search <words> # merged; project wins a same-name collision
```

#### Configuration and surfaces

```toml
[memory]
enabled = true
k3_budget_tokens = 1200
write_approval = "user-scope" # "none" | "user-scope" | "all"
stale_after_days = 30

[context_files]
enabled = true
max_chars = 20000
progressive_discovery = true
```

`medha memory list|show|search|edit|forget|pin|pending|approve` operates directly
on the event-backed memory state. `edit` opens `$EDITOR` and appends a user-trust
update event; it never writes the projection directly. TUI `/memory` shows
trust/age chips and opens the provenance event.

#### Test M1–M7 exactly

Run milestone-focused tests from the repository root:

```sh
# M1: event model, projection, replay, FTS5, and fork semantics
cargo test -p memory projection::tests
cargo test -p store memory_write_kind_round_trips_through_persistence

# M2: kernel trust boundary, poisoning resistance, updates, and contradictions
cargo test -p tools --test memory_poisoning_e2e
cargo test -p tools write_stores_kernel_trust_not_model_trust
cargo test -p tools contradictory_update_returns_reconciliation_actions

# M3: ranked/budgeted/frozen K3, cross-session recall, and compaction refresh
cargo test -p memory recall::tests
cargo test -p context frozen_system_refresh_runs_only_at_full_compaction

# M4: consolidation payload, retry cap, in-turn recovery, and stale decay
cargo test -p memory consolidate::tests
cargo test -p tools over_budget_write_lists_pressure_and_caps_retries
cargo test -p tools scripted_consolidation_then_retry_lands_in_one_turn

# M5: verbatim event search plus discover/scroll/browse behavior
cargo test -p store search_window_and_bookends_return_verbatim_prior_session_events
cargo test -p tools sessions_search_discovers_scrolls_and_browses_verbatim

# M6: precedence, guards, truncation, progressive context, and persona
cargo test -p context ctxfiles::tests

# M7: CLI lifecycle, provenance, editing, pinning, approval, and fork exclusion
cargo test -p medha-cli --test memory_e2e
cargo test -p medha-cli memory_picker_shows_trust_age_and_provenance_action
```

Then run the release gate:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

The M7 end-to-end test is the deterministic no-provider proof of the main
lifecycle: write → fresh projection recalls → CLI provenance resolves → fork
before the write excludes it. The poisoning and `sessions.search` tests also use
scripted/local components, so they make zero external model calls.

For a manual smoke test with a configured provider, use one unique project-scope
name so it can be removed afterward:

```sh
cargo build -p medha-cli

target/debug/medha \
  "Call memory.write exactly once with name m7-smoke-quoted-hyphen, claim 'The quoted-hyphen cache key is alpha-beta.', description 'Smoke-test cache decision', kind decision, scope project, and no links."
target/debug/medha memory show m7-smoke-quoted-hyphen
target/debug/medha memory search quoted-hyphen

# This is a fresh session; ask from K3 without explicitly searching.
target/debug/medha \
  "Without calling any search tool, what is the quoted-hyphen cache key?"

# Deep memory and prior-session retrieval through their tools.
target/debug/medha \
  "Call memory.search for quoted-hyphen and report the exact stored claim."
target/debug/medha \
  "Call sessions.search with query quoted-hyphen and return the verbatim exchange."

target/debug/medha memory pin m7-smoke-quoted-hyphen
target/debug/medha memory pin m7-smoke-quoted-hyphen --off
target/debug/medha memory forget m7-smoke-quoted-hyphen
target/debug/medha memory list
```

Expected observations: `show` includes trust, confidence, version, claim, event
ID, and session ID; the fresh session answers from K3; searches return the exact
claim/exchange; and the final list no longer contains the forgotten name. A
user-scope write should additionally exercise the configured human approval
gate. In the TUI, run `/memory`, select an entry, and use the provenance action
to jump to the event that created it.

#### Remaining work

M1–M7 and their normal test coverage are complete. The intentionally deferred
work is: golden Eval Gate memory scenarios; embedding/hybrid retrieval if FTS5
evals justify it; post-session distillation; sleep-time consolidation with
staged approval; external memory providers; and cross-agent shared memory.
These are later phases, not missing M1–M7 exit criteria.

One operational recovery gap remains: startup automatically rebuilds the
project projection from its workspace log, but there is not yet a production
command that scans every workspace log to reconstruct a deleted
`$MEDHA_HOME/memory.db`. User-global writes persist and work across projects in
normal operation; fully automatic global-projection disaster recovery still
needs that scanner/command.

---

## 7. Persistence & state
**Where:** [`crates/store/src/lib.rs`](../crates/store/src/lib.rs), [`crates/permissions/src/lib.rs`](../crates/permissions/src/lib.rs).

Runtime state is under `$MEDHA_HOME/projects/<encoded-workspace>/` (default:
`~/.medha/projects/<encoded-workspace>/`), outside the repository. The only
workspace `.medha/` content is committed configuration such as skills:

| Path | What |
|------|------|
| `events.db` | SQLite (WAL) hash-chained event log — the single source of truth. |
| `artifacts/` | Content-addressed blob store (SHA-256 named); large tool outputs spill here. Path-traversal-safe (`safe_path` rejects non-hex hashes). |
| `snapshots/` | Pre-write file snapshots (ULID-named) — the basis for a future `medha undo`. |
| `trust.lock` | Machine-local out-of-workspace path permission grants (never committed). |
| `logs/medha.log` | Structured `tracing` log (file, never stdout — the TUI owns the screen). |
| `logs/audit.log` | Audit log of out-of-workspace access attempts. |

A second cache is **global, not per-project**:
`$MEDHA_HOME/models_dev_cache.json`
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

---

## 8. The Eval Gate — `medha gate`
**Where:** [`crates/gate/`](../crates/gate/) — `scenario.rs`, `run.rs`, `checks.rs`, `verdict.rs`, `report.rs`, `lib.rs`; CLI dispatch in [`crates/medha-cli/src/main.rs`](../crates/medha-cli/src/main.rs) (`run_gate_command`).

"CI for cognition" (spec §4.11–4.12): run the *real* agent against fixture
scenarios in isolation, then score each run with **deterministic checks** over
the event log + filesystem — **no LLM-as-judge** — and return a
**promote / hold / reject** verdict. Exit code gates CI: `0` all promote · `1`
any reject · `2` any hold.

```sh
medha gate scenarios/fix-failing-test              # one scenario
medha gate scenarios/                              # every scenario under a dir
medha gate scenarios/ --seeds 3                    # 3 repeats → pass-rate + Wilson CI
medha gate scenarios/ --json                       # machine-readable (CI)
medha gate scenarios/fix-failing-test --validate   # load/lint only — no model run, free
```

### Scenario format
A scenario is `scenarios/<id>/scenario.yaml` plus a `fixture/` directory:

| Field | Meaning |
|-------|---------|
| `id` | Stable identifier (report key). |
| `task` | The instruction handed to the agent verbatim. |
| `fixture` | Directory copied into a throwaway workspace per run (default `fixture`). |
| `contract` | `max_turns` / `max_tokens` / `max_cost_usd` / `max_wall_s` → the run's budget env. |
| `checks` | The deterministic checks — all must pass for the run to pass. |
| `labels` | Free-form tags (`coding`, `golden`, `adversarial`, …). |

A scenario with **zero checks is rejected at load** — a check that can't fail
would rubber-stamp any run (Vol 5 §2).

### Check kinds (all deterministic, no model call)
| Check | Passes when |
|-------|-------------|
| `command: { run, expect_exit, contains? }` | The command exits `expect_exit` (and stdout contains `contains`, if given). |
| `unchanged: <glob>` / `changed: <glob>` | Files matching the glob are byte-identical to / differ from the pristine fixture. |
| `exists: <path>` / `absent: <path>` | The path exists / does not in the post-run workspace. |
| `tool_used: <tool>` / `tool_not_used: <tool>` | The agent did / did not issue that tool intent (scans `model.tool_intent` events). |
| `event_absent: { kind, contains }` / `event_present` | No / at least one event of `kind` carries `contains` in its payload (e.g. no `policy` event mentioning `dangerous_pattern`). |

### Isolation & autonomy
Each run is **hermetic** (`run.rs`): a fresh temp workspace (the fixture,
copied) and a throwaway `MEDHA_HOME`, so the run's `events.db` is isolated and a
run never touches the operator's real code or `~/.medha`. The gate spawns the
**real `medha` binary** (`std::env::current_exe`) — a true black-box, not an
in-process re-assembly of the kernel — with the provider injected as env
(`MEDHA_BASE_URL`/`MEDHA_MODEL`/`MEDHA_API_KEY`) and `kill_on_drop` + a wall
backstop. Because a gate run is **unattended**, it runs autonomously
(`MEDHA_APPROVE=none`); safety comes from the disposable workspace + OS sandbox +
the deny-first scanner (which still hard-denies dangerous commands), not a human
gate.

### Verdict & statistics (`verdict.rs`)
The *agent run* is stochastic; the *scoring* is a pure function of the finished
run, so the same run always yields the same verdict. `--seeds N` runs a scenario
`N` times → a pass-rate with a **Wilson 95% confidence interval** (a single seed
prints a noise caveat). A seed passes only if the run **completed** (not timed
out) and every check passed. Verdict: **promote** if pass-rate ≥
`[gate] pass_threshold`, **reject** if nothing passed, else **hold**.

### Deferred (roadmap — see `PROGRESS.md`)
LLM-as-judge with calibration (Vol 5 §4), canary / win-rate / promotion /
rollback (§4.11), microVM isolation, `--ablate`, and the trace→eval flywheel.
This crate ships the deterministic core those build on.
