# MEDHA

**A verification-first AI agent harness — one static Rust binary, any OpenAI-compatible model.**

> **मेधा (Medha)** — Sanskrit for *sharp intelligence, retentive power, mental fire.*

MEDHA runs an autonomous coding agent on top of **any** OpenAI-compatible endpoint — vLLM, NVIDIA NIM, Ollama, llama.cpp, or a hosted gateway. The model proposes actions (read a file, run a command, search the web, edit code); the harness validates, polices, sandboxes, and records every one of them behind a deny-first policy and a tamper-evident event log.

The bet: **the frontier of agent reliability has moved from the model to the harness.** The same model behind a stronger harness is a dramatically more reliable agent. MEDHA is that harness — sandboxed, auditable, and able to accumulate memory and skills across sessions.

```sh
cargo build --release
./target/release/medha
```

---

## Highlights

- **Runs anywhere OpenAI's API shape is spoken** — strict-compliant wire format, so it works on vLLM, NIM, Together, Fireworks, OpenRouter, Groq, Ollama, and OpenAI itself. Live streaming *or* whole-response mode (`/stream`).
- **Deny-first by construction** — unregistered tools are denied; `shell.exec` is scanned for dangerous patterns; consequential actions gate for human approval.
- **OS-native sandbox, no Docker required** — filesystem jail via macOS Seatbelt / Linux Landlock by default; Host, Container, and SSH backends available.
- **Tamper-evident memory** — every fact the agent learns is an event in a SHA-256 hash-chained log, with kernel-computed trust and provenance the model cannot forge.
- **Time travel** — rewind to any past turn and branch a new session; the original is never destroyed.
- **Skills** — reusable, security-scanned workflows the agent loads on demand and can author with your approval.
- **Eval Gate** — deterministic, CI-style scenarios that score an agent setup pass/reject.

---

## Quick start

```sh
# Build (Rust 1.85+, edition 2024)
cargo build --release
./target/release/medha          # first run walks you through model setup
```

Point it at a model — in the setup form, or via environment:

```sh
export MEDHA_BASE_URL="http://localhost:8000/v1"   # any OpenAI-compatible server
export MEDHA_MODEL="qwen3-coder"
export MEDHA_API_KEY="…"                            # if the endpoint needs one
```

Then work interactively or headless:

```sh
medha                                  # full-screen TUI
medha "fix the failing test in tests/calc.rs"   # one-shot, headless
```

In the TUI: type a task and press **Enter**, approve or deny prompted actions, **Esc** to interrupt, **Ctrl-D** to quit.

---

## How a turn works

```
model streams a response
      │
      ▼
 ┌──────────────────────────────────────────────┐
 │  validate → police → (verify) → sandbox → run │   for every tool the model calls
 └──────────────────────────────────────────────┘
      │
      ▼
 observations fed back to the model · every step appended to the event log
```

Nothing the model says *causes* an effect. Only a validated, policy-approved, sandbox-confined tool intent does — and every intent, decision, and result is a durable event that can be replayed, audited, or forked.

---

## What the model sees each turn

The prompt sent to the model isn't one flat blob — MEDHA assembles it in **five ordered layers**, each with its own token budget. Understanding these makes the rest of MEDHA (memory, context files, compaction) click into place.

```
1. Identity    Who the agent is — your PERSONA.md, the harness rules, the mode.
2. Capability  Which tools it may call, and any skills you've loaded.
3. Knowledge   What it knows — a compact, ranked memory index + project facts.
4. History     The conversation and tool results so far. The biggest layer.
5. Immediate   Your current message and the live progress checklist.
```

Two rules follow from this order:

- **The top layers stay stable across turns.** Identity and capability rarely change mid-session, so the provider's prompt cache keeps working and turns stay fast. Memory is compiled once at session start and frozen — it only refreshes when the history is compacted (which breaks the cache anyway).
- **Pressure is absorbed at the bottom.** When the context window fills up, MEDHA compacts the *History* layer (summarizing old turns, spilling large tool outputs to disk) and never touches your current message or the progress checklist.

Memory lives in the **Knowledge** layer as a short, ranked list — not every fact the agent has ever learned, just the most relevant ones under a hard token budget. When the model needs the full entry, it calls `memory.search`; for older conversations, `sessions.search`.

---

## Features

### Model & providers
Any OpenAI-compatible Chat Completions endpoint. Tool names are sanitized to the strict OpenAI contract on the wire, so endpoints that reject non-standard names (NIM, OpenAI) work out of the box. Reasoning traces (`reasoning_content` or `<think>` tags) stream natively. Toggle streaming with `/stream` — useful for gateways that only expose reasoning in a non-streamed response.

### Deny-first policy & sandbox
- **Policy** — unregistered tools denied; `shell.exec` run through a dangerous-pattern scanner; file writes and other consequential actions gate for approval. Autonomy dial: `careful · normal · yolo`.
- **Sandbox** — pick your isolation in `medha.lock`:
  - `native` (default) — OS jail: writes confined to the workspace, temp, and dev caches; `~/.ssh` and the like are blocked. Zero dependencies.
  - `host` — no OS isolation (scanner + approval only).
  - `container` — throwaway Docker/Podman container; host env is **not** forwarded, so API keys stay put.
  - `ssh` — run commands on a remote host.
- **Env hygiene** — `shell.exec` starts from an empty environment with an explicit allowlist, so a leaked key never reaches an arbitrary command.
- **Trust-flow escalation** — an action derived from web-fetched content is escalated to human approval unless the sandbox confines the network.

### Typed memory
Memory is event-sourced, not hidden model state:

1. The model calls `memory.write` / `memory.update` / `memory.forget`.
2. The **kernel** computes the entry's trust, confidence, and provenance from the current turn — these are stripped from the model's arguments and can never be self-asserted. A turn that read a web page can only produce web-trust memory.
3. The mutation is appended to the hash-chained event log, then projected into project- and user-scoped SQLite databases with FTS5 search.
4. A compact **memory index** — the Knowledge layer above — is ranked (pinned → trust → recency), fit under a hard token budget, and compiled into the prompt at session start. It's frozen for the session and refreshes only after a full compaction, so the prompt stays cache-stable.
5. `memory.search` returns full entries; `sessions.search` returns verbatim exchanges from past sessions — no extra model call.

Because memory writes are events, **time-travel applies to memory for free**: fork a session before a bad write and the branch never learned it.

```sh
medha memory list
medha memory show <name>          # includes the provenance events it was born from
medha memory search <words>
medha memory edit <name>          # $EDITOR; re-enters through the log as a user-trust update
medha memory pin <name> [--off]
medha memory forget <name>
medha memory pending              # writes staged for approval
medha memory approve <id>
```

| Scope | Location |
|---|---|
| Project memory | `$MEDHA_HOME/projects/<workspace>/memory.db` |
| User (global) memory | `$MEDHA_HOME/memory.db` |
| Provenance events | `$MEDHA_HOME/projects/<workspace>/events.db` |

### Context files & persona
Drop project instructions in **`MEDHA.md`**, `AGENTS.md`, or `CLAUDE.md` (first match wins, per directory from cwd to the git root, plus `~/.medha/MEDHA.md` for global rules). Files are guard-scanned before they enter the prompt; a subdirectory's file is discovered and attached the first time the agent touches that directory. `~/.medha/PERSONA.md` sets the agent's global identity. Existing `AGENTS.md`/`CLAUDE.md` files work with no changes — adopting MEDHA on an existing repo is zero-config.

### Skills
A **skill** is a versioned `SKILL.md` + supporting scripts the agent loads on demand. Author them by hand, or say "save this as a skill" and approve the `skill.save`. Every skill passes a security guard (static scan + an LLM judge for ambiguous cases). `/skill lock` and `/skill sync` pin a team's skill set for reproducibility.

### Time travel & undo
```sh
medha undo                # restore the last file write
medha undo --list         # recent writes
medha undo --event <id>   # undo from an event onward
```
In the TUI, `/rewind` branches a new session from an earlier turn (conversation only, code only, or both) — the original session is preserved.

### Eval Gate — CI for cognition
```sh
medha gate scenarios/                 # run every scenario
medha gate scenarios/ --seeds 3       # repeat → pass-rate with a Wilson interval
medha gate scenarios/ --json          # machine-readable for CI
```
A scenario declares a fixture workspace, a task, a budget contract, and deterministic checks (command exit codes, file diffs, tools used). Verdict: **promote / hold / reject**, with matching exit codes.

---

## Configuration — `medha.lock`

Commit a `medha.lock` (TOML) to version your harness config. Absent file = built-in defaults; nothing changes for a bare checkout. Precedence: **env var > `medha.lock` > default.**

```toml
[budget]
max_turns = 200
max_cost_usd = 5.0
max_parallel_tools = 8

[policy]
approve = ["fs.write", "fs.edit", "skill.save"]
autonomy = "careful"          # careful · normal · yolo

[sandbox]
backend = "native"            # native · host · container · ssh
network = "allow"             # or "deny" for stronger containment

[memory]
enabled = true
k3_budget_tokens = 1200
write_approval = "user-scope" # none · user-scope · all
stale_after_days = 30

[context_files]
enabled = true
max_chars = 20000
progressive_discovery = true

[reasoning]
# stream = false              # whole reply at once; surfaces reasoning on some gateways

[verify]
# command = "cargo check"     # deterministic check after file-modifying turns
```

See [`medha.lock.example`](medha.lock.example) for every option. Quick env overrides: `MEDHA_MAX_TURNS`, `MEDHA_APPROVE`, `MEDHA_SANDBOX`, `MEDHA_MODE`.

---

## TUI commands

Type `/` for the palette (Tab completes, ↑↓ navigate menus).

| Command | Does |
|---|---|
| `/help` · `/status` | Commands · model, context window, pressure |
| `/reasoning` | Thinking mode, effort, visibility |
| `/stream` | Toggle live token streaming |
| `/model` · `/search` | Switch model/profile · web-search provider |
| `/mode` | Autonomy: careful · normal · yolo |
| `/memory` | Browse memory with trust/age chips; jump to provenance |
| `/skill` | Load, add, search, update, lock/sync skills |
| `/rewind` · `/resume` | Branch from an earlier turn · reopen a past session |
| `/detail` · `/tasks` | Full tool I/O · background shell tasks |
| `/clear` · `/exit` | Reset conversation · quit (or Ctrl-D) |

---

## Run modes

```sh
medha                          # interactive TUI
medha "…task…"                 # headless one-shot
medha --plain                  # scrolling REPL
medha --acp                    # editor bridge (JSON-RPC over stdio)
medha gate scenarios/          # run eval scenarios
medha memory <cmd>             # inspect/manage memory
medha undo                     # restore the last write
```

---

## Architecture

Twelve crates. `kernel` is the only code that calls a model, writes an event, or enforces a budget; everything else is a trait behind it.

| Crate | Role |
|---|---|
| [`kernel`](crates/kernel/) | Agent loop, budgets, trust-flow, interrupts, dispatch |
| [`providers`](crates/providers/) | OpenAI-compatible streaming + non-streaming, model discovery |
| [`context`](crates/context/) | Prompt assembly (the five context layers), compaction, identity, context files |
| [`memory`](crates/memory/) | Typed memory: projection, ranked recall, consolidation |
| [`tools`](crates/tools/) | 23 tools: fs, shell, web, git, search, diagnostics, skills, memory |
| [`policy`](crates/policy/) | Deny-first authorization, shell scanner, content guard |
| [`sandbox`](crates/sandbox/) | Exec backends: host · Seatbelt/Landlock · container · ssh |
| [`store`](crates/store/) | SQLite (WAL) hash-chained event log + FTS5 + artifact store |
| [`lockfile`](crates/lockfile/) | `medha.lock` parsing, defaults, migration |
| [`permissions`](crates/permissions/) | Ask-then-persist trust for out-of-workspace access |
| [`gate`](crates/gate/) | Eval Gate: scenario runner, deterministic checks |
| [`medha-cli`](crates/medha-cli/) | TUI (ratatui), REPL, headless, ACP bridge |

**Provider config precedence:** CLI flag → env var → `~/.medha/config.toml` → first-run setup.
**API keys:** env var → `~/.medha/credentials.toml` (0600) → OS keychain (optional). Keys are never written to `medha.lock`.

---

## Documentation

| Doc | Covers |
|---|---|
| [docs/FEATURES.md](docs/FEATURES.md) | Full feature reference with code locations |
| [medha.lock.example](medha.lock.example) | Every configuration option, annotated |

---

## Status

Shipped and in daily use: the kernel loop, OpenAI-compatible provider (streaming + non-streaming), the tool suite, all four sandbox backends, deny-first policy, context compaction, **typed memory with provenance**, context files and persona, the hash-chained event log, time-travel/undo, skills, and the deterministic Eval Gate.

On the roadmap: cross-vendor adversarial verification, span-level trust taint, sub-agent swarms, a deep-research pipeline, trace→skill distillation with eval-gated promotion, native Anthropic/Gemini adapters, and the WebSocket gateway server.

---

## License

Apache-2.0.

> *मेधा सूक्ताय नमः* — Salutations to the hymn of sharp intelligence.
