# MEDHA

**A self-evolving AI agent harness — verification-first, open-first, one static binary.**

> **मेधा (Medha)** — Sanskrit for *sharp intelligence, retentive power, mental fire*. From the Vedic *Medha Suktam*.

MEDHA is an autonomous agent kernel written in Rust. Point it at any OpenAI-compatible model (vLLM, Ollama, llama.cpp, or hosted) and it runs an autonomous loop: read files, run shell commands, fetch the web, edit code — all behind a deny-first policy, flexible sandbox (Host · Native · Docker/Podman · SSH), and tamper-evident audit log. The model proposes; the harness disposes.

```sh
cargo build --release
./target/release/medha
```

**The thesis:** The frontier of agent capability has moved from the model to the harness. The same model behind different harnesses produces wildly different reliability. MEDHA is the harness that learns — accumulating skills, evolving from traces, and gating every change with deterministic evals.

---

## ⚡ Quick Start (30 seconds)

**1. Install & Run**
```sh
cargo install --path .    # or: cargo build --release
medha                     # First run opens TUI setup
```

**2. Configure Your Model**
The TUI guides you through:
- Enter base URL (e.g., `http://localhost:8000` for vLLM, or hosted provider)
- Paste API key (stored encrypted, never in config)
- Select model ID

**3. Start Working**
```sh
medha "Fix the failing test in tests/calc.rs"
```

Or interactive TUI:
- Type task, press **Enter**
- Watch live streaming output
- Approve/deny actions when prompted
- **Esc** to interrupt, **Ctrl-D** to quit

**That's it.** Sandboxed, auditable, self-improving.

---

## 🖥️ TUI Commands

Type `/` + command. **Tab-complete** available. **↑↓ arrows** in menus.

| Command | What It Does |
|---------|--------------|
| `/help` | Show all commands |
| `/status` | Model, context window, token estimate |
| `/reasoning` | Configure thinking mode (on/off/effort/visibility) |
| `/model` | Switch models or add/edit saved profiles |
| `/search` | Change web search provider (Tavily/Brave/SearXNG/DuckDuckGo) |
| `/mode` | Autonomy: `careful` · `normal` · `yolo` |
| `/detail` | Toggle full tool I/O vs summarized |
| `/resume` | Load past session from event log |
| `/rewind` | Time-travel: branch from earlier turn (undoes later edits) |
| `/tasks` | List background shell tasks (running/finished) |
| `/skill` | Skill hub: load, add, search, update, lock/sync |
| `/clear` | Reset conversation (keeps system prompt) |
| `/exit` or `/quit` | Quit TUI (or Ctrl-D) |

**Keyboard Shortcuts:**
- **Esc** — Interrupt running turn
- **Ctrl-D** — Quit
- **↑/↓** — Scroll history (empty input) or cycle input history (typing)
- **Tab** — Auto-complete commands

---

## 📦 Core Capabilities

### 🔒 Verification-First Architecture

Every tool call: **stream → validate → police → execute**

**Verification tiers:**
- **Deterministic** (default) — `cargo check`, `pytest`, typecheck after edits (free)
- **Human gate** — Approval required for consequential actions (file writes, git commits, external actions)

Configured in `medha.lock` `[verify]` section. Multi-judge consensus is Phase 4 roadmap.

### 🧠 Self-Evolving via Skills

A **skill** is a reusable workflow (`SKILL.md` + scripts) that MEDHA can load on demand:

**Authoring:**
- Manual: Write `SKILL.md` by hand in `.medha/skills/` or `~/.medha/skills/`
- Agent-assisted: "save this as a skill" → `skill.save` tool (human approval required)
- Proactive: MEDHA offers to save when you repeat instructions

**Features (Phase A - shipped):**
- Install/load/save with Skills Guard security scan
- Content-hashed updates (your edits protected)
- `/skill lock` / `/skill sync` for team reproducibility
- Search catalog, add from GitHub URLs

**Roadmap (Phase D):** Auto-distill skills from traces, eval-gated promotion, canary deployment, win-rate tracking

### ⚡ Parallel Execution

**Parallel tool calls:** Kernel executes independent tools concurrently
- Dependency-aware DAG (not queue)
- Write-safety: same-path writes serialized with snapshot barriers
- Governed: `max_parallel_tools` (default 8 in `medha.lock`)
- Per-tool-family semaphores (e.g., `web.*` ≤ 4 for rate limits)

### ⏪ Time-Travel & Undo

**`medha undo`** — CLI snapshot restore:
```sh
medha undo                    # Undo last write
medha undo --event <id>       # Undo from event <id> onward
medha undo --list             # List recent writes
```

**`/rewind`** (TUI) — Non-destructive branching:
1. Pick previous prompt
2. Choose scope: conversation only, code only, or both
3. Forks session (original preserved), prefills prompt to re-run

**Hash-chained event log:** SHA-256 linked events in SQLite (WAL)
- Tamper-evident: any modification breaks chain
- Replay from any point: exact reconstruction of session state
- Fork at any event: new chain, shared prefix

### 🧪 Eval Gate: CI for Cognition

Test agent setups with deterministic scenarios:

```sh
medha gate scenarios/fix-failing-test            # One scenario
medha gate scenarios/                            # All in directory
medha gate scenarios/ --seeds 3                  # 3 repeats → pass-rate + CI
medha gate scenarios/ --json                     # Machine-readable for CI
```

**Scenario anatomy:**
```yaml
id: fix-failing-test-007
fixture: sha256:...            # Content-addressed workspace
task: "The test suite fails. Diagnose and fix."
contract: { max_cost_usd: 1.50, max_turns: 40 }
checks:                        # Deterministic first
  - kind: command; run: "pytest -q"; expect: { exit: 0 }
  - kind: file_diff; path: "tests/**"; expect: { unchanged: true }
trajectory:                    # Soft-scored, multiple valid paths
  must_use_any: [["fs.read", "code.outline"]]
  must_not: ["web.fetch"]
```

**Verdicts:** promote / hold / reject
- Pass-rate ± Wilson interval (multi-seed)
- Exit codes: `0` = all pass, `1` = reject, `2` = hold

**Roadmap:** LLM-as-judge with calibration, ablation studies, trace→eval flywheel

### 🛡️ Security Model

- **Deny-first policy** — Unregistered tools denied; `shell.exec` scanned for dangerous patterns ([`crates/policy/src/lib.rs`](crates/policy/src/lib.rs))
- **Flexible sandbox backends** — Choose your isolation level:
  - **Host** — Run directly (scanner + approval only)
  - **Native** — macOS Seatbelt / Linux Landlock (filesystem jail, zero dependencies)
  - **Container** — Docker/Podman with bind-mount, cap-drop, no host env forwarding
  - **SSH** — Remote execution on `user@host`
  ([`crates/sandbox/src/exec.rs`](crates/sandbox/src/exec.rs))
- **Environment clearing** — `shell.exec` starts from empty env with explicit allowlist, so injected API keys never reach arbitrary commands
- **Trust-flow escalation** — Web-tainted consequential actions require human approval unless network confined ([`crates/kernel/src/loop_.rs`](crates/kernel/src/loop_.rs))
- **Tamper-evident log** — SHA-256 hash-chained events; SQLite verifies chain on resume ([`crates/store/src/lib.rs`](crates/store/src/lib.rs))

---

## 📁 Configuration: `medha.lock`

Drop `medha.lock` (TOML) in project root to version harness config. Absent = defaults.

**Example:**
```toml
[routing]
executor = "openai-compat://localhost:8000/qwen3-coder"
# verifier = "openai-compat://together/llama-3.3-70b"  # Phase 3

[budget]
max_turns = 200
max_cost_usd = 5.0
max_parallel_tools = 8

[policy]
approve = ["fs.write", "fs.edit", "skill.save"]
autonomy = "careful"   # careful · normal · yolo

[sandbox]
backend = "native"     # Seatbelt/Landlock
network = "allow"      # or "deny" for stronger containment

[verify]
default_mode = "deterministic"   # off · deterministic · single · multi · human

[context]
trigger_ratio = 0.99
protect_first_n = 3
protect_last_n = 20
```

**Precedence:** env vars > `medha.lock` > built-in defaults

**Env overrides:** `MEDHA_MAX_TURNS`, `MEDHA_APPROVE`, `MEDHA_VERIFY`, `MEDHA_SANDBOX`, `MEDHA_MODE`

See [`medha.lock.example`](medha.lock.example) for all options.

---

## 🏗️ Architecture

```
                 ┌─────────────────────────────────────────┐
   OpenAI-compat │                                         │
   endpoint  ───▶│  Kernel loop                            │
   (streaming)   │  stream → validate → police → verify    │
                 │           → execute → feed back         │
                 └────────┬───────────────────────┬────────┘
                          │                       │
              ┌───────────▼──────────┐  ┌─────────▼──────────┐
              │  Executor / Tools    │  │  Event log (SQLite) │
              │  fs · shell · web ·  │  │  hash-chained,      │
              │  git · grep · diag   │  │  tamper-evident     │
              └───────────┬──────────┘  └────────────────────┘
                          │
              ┌───────────▼──────────┐
              │  Sandbox (ExecBackend)│
              │  host · Seatbelt ·    │
              │  Landlock · container │
              │  · ssh                │
              └──────────────────────┘
```

**11 Crates:**

| Crate | Role | Source |
|-------|------|--------|
| `kernel` | Agent loop, budget governor, trust-flow, interrupts | [`crates/kernel/`](crates/kernel/) |
| `providers` | OpenAI-compatible streaming, model discovery | [`crates/providers/`](crates/providers/) |
| `tools` | 22+ tools: fs, shell, web, git, search, diagnostics, skills, sub-agents | [`crates/tools/`](crates/tools/) |
| `sandbox` | Flexible backends: Host · Seatbelt/Landlock · Docker/Podman · SSH | [`crates/sandbox/`](crates/sandbox/) |
| `policy` | Deny-first authorization + shell scanner + Skills Guard | [`crates/policy/`](crates/policy/) |
| `context` | Budget-aware two-phase compaction, identity, K1-K5 sheaths | [`crates/context/`](crates/context/) |
| `lockfile` | `medha.lock` parser, defaults, migration | [`crates/lockfile/`](crates/lockfile/) |
| `store` | SQLite event log (WAL, FTS5) + content-addressed artifact store | [`crates/store/`](crates/store/) |
| `permissions` | Out-of-workspace access: ask-then-persist trust flow | [`crates/permissions/`](crates/permissions/) |
| `gate` | Eval Gate: scenario runner, deterministic checks, judge calibration | [`crates/gate/`](crates/gate/) |
| `medha-cli` | TUI (ratatui), REPL, headless, ACP bridge, gateway server | [`crates/medha-cli/`](crates/medha-cli/) |

---

## 🚧 What's Built vs. Roadmap

### ✅ Shipped Today (Phase 0-2)

| Component | Status | Notes |
|-----------|--------|-------|
| **Kernel loop** | ✅ | Stream → validate → police → execute (verify stub) |
| **Provider** | ✅ | OpenAI-compatible (vLLM, Ollama, hosted) |
| **Tools (22)** | ✅ | fs, shell, web, git, grep, glob, diagnostics, skills, tasks |
| **Sandbox** | ✅ | Host, Seatbelt, Landlock; container/ssh backends |
| **Policy** | ✅ | Deny-first, shell scanner, autonomy dial |
| **Context compaction** | ✅ | Two-phase (prune + LLM summarize) |
| **Event log** | ✅ | SQLite WAL, hash-chained, FTS5 |
| **Permissions** | ✅ | Ask-then-persist for out-of-workspace |
| **`medha undo`** | ✅ | CLI snapshot restore |
| **TUI `/rewind`** | ✅ | Time-travel branching |
| **Skills (Phase A)** | ✅ | Install/load/save, Skills Guard, lock/sync |
| **Eval Gate (deterministic)** | ✅ | Scenario runner, command/file/event checks |
| **Gateway protocol** | ⬜ Phase 3 | WebSocket JSON-RPC, ACP bridge |

### 🔜 Roadmap (from Spec Volumes 1-7)

| Feature | Phase | Status | Why It Matters |
|---------|-------|--------|----------------|
| **Adversarial verifier** | Phase 3 | ⬜ Not started | Cross-vendor model reviews proposals before execution |
| **Span-level trust taint** | Phase 3 | ⬜ Not started | Track which context spans influenced a tool call |
| **Guided decoding** | Phase 3 | ⬜ Not started | Force schema-valid tool intents from weak models |
| **Multi-judge consensus** | Phase 4 | ⬜ Not started | 3+ judges, position debiasing, κ ≥ 0.75 |
| **Sub-agent swarms** | Phase 4 | ⬜ Not started | Dynamic spawn, parallel harvest (10-100 agents) |
| **Deep Research pipeline** | Phase 5 | ⬜ Not started | Evidence store, claim graph, parallel writers, assembler |
| **Trace → skill distillation** | Phase D | ⬜ Not started | Auto-extract skills from successful traces |
| **Eval-gated evolution** | Phase D | ⬜ Not started | Canary, win-rate tracking, promote/rollback |
| **Native Anthropic/Gemini** | Phase 2 | ⬜ Not started | Multi-provider routing beyond OpenAI-compatible |
| **Long-term memory** | Phase 4 | ⬜ Not started | Vector store, cross-session retrieval |
| **Gateway server** | Phase 3 | ⬜ Not started | WebSocket + HTTP+SSE fallback, thin clients |

> **Note on the spec:** Code comments reference a multi-volume design spec ("Vol 1-7", "§4.x") that is **not in this repository**. The roadmap here is reconstructed from the code's own phase language and the spec documents (stored separately). See `MEDHA_01_MASTER_SPEC.md` through `MEDHA_07_TERMINAL_UX.md` for the complete blueprint.

---

## ⚠️ Honest Assessment

**MEDHA is 7/10 production-ready** with a clear path to 9/10.

**What works well today:**
- ✅ Kernel loop, tools, sandbox, permissions
- ✅ Event log, time-travel, `medha undo`
- ✅ TUI, skills (Phase A), deterministic Eval Gate
- ✅ Trust-flow escalation (session-wide)

**What's partial or missing:**
- ⚠️ **Verifier is a stub** — `CommandVerifier` runs `cargo check`, but adversarial model review is Phase 3. The `[routing].verifier` config exists but is not consulted.
- ⚠️ **Trust-flow taint is coarse** — `web_tainted` is session-wide boolean. Once any web content enters, all subsequent consequential actions are escalated. Span-level provenance is Phase 3.
- ⚠️ **Single provider** — Only OpenAI-compatible endpoints. Native Anthropic/Gemini adapters are Phase 2.
- ⚠️ **No sub-agents yet** — Parallel tool execution works, but dynamic sub-agent spawning is Phase 4.
- ⚠️ **No deep research pipeline** — Evidence store, claim graph, parallel writers are Phase 5.
- ⚠️ **Skills don't auto-evolve** — Phase A supports manual/agent-assisted authoring. Trace distillation + eval-gated promotion is Phase D.

**When to use MEDHA today:**
- ✅ You want a sandboxed, auditable agent for local development
- ✅ You value time-travel and undo capabilities
- ✅ You're comfortable with OpenAI-compatible endpoints
- ✅ You want deterministic eval for your agent setup
- ✅ You want to author skills manually or with agent assistance

**When to wait (Phase 3-5):**
- ⏳ You need adversarial verification before execution
- ⏳ You need fine-grained trust tracking (span-level taint)
- ⏳ You want multi-judge consensus for high-stakes actions
- ⏳ You need sub-agent swarms for parallel research
- ⏳ You want auto-evolving skills from traces with win-rate tracking
- ⏳ You need native Anthropic/Gemini support

---

## 📚 Documentation

| Document | What It Covers |
|----------|----------------|
| **[FEATURES.md](docs/FEATURES.md)** | Complete feature reference with code locations |
| **[PROGRESS.md](PROGRESS.md)** | Phase-by-phase status of what's built |
| **[medha.lock.example](medha.lock.example)** | All configuration options with inline comments |
| **Spec Volumes (internal)** | `MEDHA_01_MASTER_SPEC.md` through `MEDHA_07_TERMINAL_UX.md` — the complete blueprint |

---

## 🛠️ Build & Run

**Requirements:** Rust 1.85+ (edition 2024)

```sh
cargo build --release       # → target/release/medha
./target/release/medha      # First run opens TUI setup
```

**Modes:**
```sh
medha                        # Interactive full-screen TUI
medha "fix the failing test" # Headless one-shot
medha --plain                # Scrolling REPL
medha --acp                  # Editor bridge (JSON-RPC over stdio)
medha gate scenarios/        # Run eval scenarios
medha undo                   # Restore last file write
medha serve                  # Start gateway server (Phase 3)
```

**Provider config precedence:** CLI flag → env var → `~/.medha/config.toml` → TUI first-run setup

**API key storage:** env var → `~/.medha/credentials.toml` (0600) → OS keychain (optional)

**Env overrides:** `MEDHA_BASE_URL`, `MEDHA_MODEL`, `MEDHA_API_KEY` (also accepts `OPENAI_*`)

---

## 📄 License

Apache-2.0

---

**Built with ❤️ in Rust.** No runtime, no Docker, no vendor lock-in.

> *मेधा सूक्ताय नमः* — Salutations to the hymn of sharp intelligence.
