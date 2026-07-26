# MEDHA

**A verification-first AI agent harness — one static Rust binary, any OpenAI-compatible model.**

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Edition](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)](#requirements)

> **मेधा (Medha)** — Sanskrit for *sharp intelligence, retentive power, mental fire.*

MEDHA runs an autonomous, general-purpose agent on top of **any** OpenAI-compatible endpoint — vLLM, NVIDIA NIM, Ollama, llama.cpp, or a hosted gateway. The model proposes actions — read and edit files, run commands, search and fetch the web, use git, navigate code semantically, call MCP servers, spawn sub-agents — and the harness validates, polices, sandboxes, and records every one of them behind a deny-first policy and a tamper-evident event log.

Coding is where it's sharpest today, but nothing in the kernel is coding-specific: it's a tool-using agent for whatever tools you give it.

The bet: **the frontier of agent reliability has moved from the model to the harness.** The same model behind a stronger harness is a dramatically more reliable agent. MEDHA is that harness — sandboxed, auditable, and able to accumulate memory and skills across sessions.

---

## Contents

- [Why MEDHA](#why-medha)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [How a turn works](#how-a-turn-works)
- [What the model sees each turn](#what-the-model-sees-each-turn)
- [Features](#features)
- [Configuration](#configuration)
- [TUI commands](#tui-commands)
- [Architecture](#architecture)
- [Documentation](#documentation)
- [Project status](#project-status)
- [Contributing](#contributing)
- [Security](#security)
- [License](#license)

---

## Why MEDHA

- **Runs anywhere OpenAI's API shape is spoken** — strict-compliant wire format, so it works on vLLM, NIM, Together, Fireworks, OpenRouter, Groq, Ollama, and OpenAI itself. Live streaming *or* whole-response mode (`/stream`).
- **Deny-first by construction** — unregistered tools are denied, `shell.exec` is scanned for dangerous patterns, and consequential actions gate for human approval.
- **OS-native sandbox, no Docker required** — filesystem jail via macOS Seatbelt / Linux Landlock by default; Host, Container, and SSH backends available.
- **Semantic code intelligence, automatically** — detects Rust, TypeScript/JavaScript, Python, Go, and C/C++, reuses installed language servers lazily, and feeds post-edit diagnostic deltas back to the agent. No language selection required.
- **Sub-agents that are real sessions** — a child gets its own session id, event log, and narrowed capability set, enforced at runtime rather than by prompt. A child can never widen its own permissions.
- **MCP host** — supervised Model Context Protocol servers, with their output treated as untrusted data rather than instruction.
- **Tamper-evident memory** — every fact the agent learns is an event in a SHA-256 hash-chained log, with kernel-computed trust and provenance the model cannot forge.
- **Time travel** — rewind to any past turn and branch a new session; the original is never destroyed.
- **Skills** — reusable, security-scanned workflows the agent loads on demand and can author with your approval.
- **Eval Gate** — deterministic, CI-style scenarios that score an agent setup pass/reject.

---

## Requirements

| | |
|---|---|
| **Rust** | 1.85 or newer (edition 2024) |
| **Platform** | macOS (Seatbelt sandbox) or Linux (Landlock sandbox) |
| **Model endpoint** | Any OpenAI-compatible Chat Completions API |
| **Optional** | Docker/Podman for the container backend; language servers for semantic navigation |

No runtime dependencies — MEDHA builds to a single binary.

---

## Installation

```sh
git clone <repository-url> medha
cd medha
cargo build --release
```

The binary lands at `./target/release/medha`. Put it on your `PATH`:

```sh
install -m 755 target/release/medha /usr/local/bin/medha
```

---

## Quick start

```sh
medha          # first run walks you through model setup
```

Point it at a model — in the setup form, or via environment:

```sh
export MEDHA_BASE_URL="http://localhost:8000/v1"   # any OpenAI-compatible server
export MEDHA_MODEL="qwen3-coder"
export MEDHA_API_KEY="…"                           # if the endpoint needs one
```

> **Only the `MEDHA_*` namespace configures MEDHA.** As a harness that runs inside repos it doesn't own, MEDHA deliberately **never reads a project's `.env`** and **never reads generic `OPENAI_*` / `GOOGLE_*` names** — those belong to the app that owns the directory, and reading them would let one project's environment silently hijack MEDHA's model or credentials. Config comes only from `MEDHA_*`, `~/.medha/config.toml`, the OS keychain, and `medha.lock [routing]`.
>
> Run **`medha pulse`** (or **`/pulse`** in the TUI) to see exactly which model and key resolved, and from where. **`medha pulse --fix`** auto-repairs safe issues.

Then work interactively or headless:

```sh
medha                                                 # full-screen TUI
medha "fix the failing test in tests/calc.rs"         # one-shot, headless
medha "summarize the open issues and draft a triage plan"
```

In the TUI: type a task and press **Enter**, approve or deny prompted actions, **Esc** to interrupt, **Ctrl-D** to quit.

### Run modes

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

## How a turn works

```
model streams a response
      │
      ▼
 ┌───────────────────────────────────────────────┐
 │  validate → police → (verify) → sandbox → run │   for every tool the model calls
 └───────────────────────────────────────────────┘
      │
      ▼
 observations fed back to the model · every step appended to the event log
```

Nothing the model says *causes* an effect. Only a validated, policy-approved, sandbox-confined tool intent does — and every intent, decision, and result is a durable event that can be replayed, audited, or forked.

---

## What the model sees each turn

The prompt isn't one flat blob — MEDHA assembles it in **five ordered layers**, each with its own token budget. Understanding these makes the rest of MEDHA (memory, context files, compaction) click into place.

```
1. Identity    Who the agent is — your PERSONA.md, the harness rules, the mode.
2. Capability  Which tools it may call, and any skills you've loaded.
3. Knowledge   What it knows — a compact, ranked memory index + project facts.
4. History     The conversation and tool results so far. The biggest layer.
5. Immediate   Your current message and the live progress checklist.
```

Two rules follow from this order:

- **The top layers stay stable across turns.** Identity and capability rarely change mid-session, so the provider's prompt cache keeps working and turns stay fast. Memory is compiled once at session start and frozen — it refreshes only when history is compacted (which breaks the cache anyway).
- **Pressure is absorbed at the bottom.** When the context window fills, MEDHA compacts the *History* layer — summarizing old turns, spilling large tool outputs to the artifact store — and never touches your current message or the progress checklist.

Memory lives in the **Knowledge** layer as a short, ranked list — not every fact ever learned, just the most relevant under a hard token budget. For the full entry the model calls `memory.search`; for older conversations, `sessions.search`.

---

## Features

### Model & providers

Any OpenAI-compatible Chat Completions endpoint. Tool names are sanitized to the strict OpenAI contract on the wire, so endpoints that reject non-standard names (NIM, OpenAI) work out of the box. Reasoning traces (`reasoning_content` or `<think>` tags) stream natively. Toggle streaming with `/stream` — useful for gateways that only expose reasoning in a non-streamed response.

### Deny-first policy & sandbox

- **Policy** — unregistered tools denied; `shell.exec` run through a dangerous-pattern scanner; file writes and other consequential actions gate for approval.
- **Autonomy modes** (`/mode`, or `[policy].autonomy`) — how much runs without asking. The safety floor never moves: dangerous commands, credential reads, and web-tainted external actions gate or deny at *every* level, even `yolo`.

  | Mode | Behavior |
  |---|---|
  | `careful` (default) | Ask before every configured consequential action — file writes, commits, skill saves |
  | `normal` | File edits run freely; other consequential actions still ask |
  | `yolo` | No approval prompts for already-allowed actions; the deny-first floor still bites |

- **Sandbox backends** — chosen in `medha.lock`:

  | Backend | Isolation |
  |---|---|
  | `native` (default) | OS jail — writes confined to workspace, temp, and dev caches; `~/.ssh` and the like blocked. Zero dependencies |
  | `host` | No OS isolation (scanner + approval only) |
  | `container` | Throwaway Docker/Podman container; host env is **not** forwarded, so API keys stay put |
  | `ssh` | Run commands on a remote host |

- **Env hygiene** — `shell.exec` starts from an empty environment with an explicit allowlist, so a leaked key never reaches an arbitrary command.
- **Trust-flow escalation** — an action derived from web-fetched content is escalated to human approval unless the sandbox confines the network.
- **Tool output is data, not instruction** — web pages, MCP results, file contents, and sub-agent reports carry no authority. The operating brief states this explicitly, and trust labels are computed by the kernel.

### Sub-agents

A child agent is an independently managed session, not a prompt trick. It is built ad hoc from an objective — there are no preset agent files. It gets a fresh session id, so the event log already gives it a durable, resumable, independently addressable transcript; the parent receives only a bounded structured result.

Capability narrowing is enforced at runtime in two places: `specs()` decides what the child is *shown*, and `execute()` refuses independently — because a model can name a tool it was never shown. **A child can never widen beyond its parent's set.** Read-only children may share a workspace; writers may not.

```
agent.spawn · agent.list · agent.wait · agent.message · agent.steer
agent.followup · agent.transcript · agent.cancel · agent.apply
```

### Semantic code intelligence (LSP)

Supervised Language Server Protocol support. MEDHA detects the language, lazily reuses installed servers, and exposes definitions, references, hover, symbols, implementations, and call hierarchy — so navigation is semantic rather than grep guesswork. Post-edit diagnostic deltas are fed back to the agent automatically.

```
lsp.definition · lsp.references · lsp.hover · lsp.symbols · lsp.document_symbols
lsp.implementation · lsp.call_hierarchy · lsp.diagnostics · lsp.start · lsp.status
```

### MCP host

Supervised Model Context Protocol servers over local stdio. Servers are health-checked, restarted with backoff, and capped. Their tools appear to the model namespaced as `mcp__*`, and their output crosses a trust boundary — it is treated as untrusted data.

### Typed memory

Memory is event-sourced, not hidden model state:

1. The model calls `memory.write` / `memory.update` / `memory.forget`.
2. The **kernel** computes the entry's trust, confidence, and provenance from the current turn — these are stripped from the model's arguments and can never be self-asserted. A turn that read a web page can only produce web-trust memory.
3. The mutation is appended to the hash-chained event log, then projected into project- and user-scoped SQLite databases with FTS5 search.
4. A compact **memory index** — the Knowledge layer above — is ranked (pinned → trust → recency), fit under a hard token budget, and compiled into the prompt at session start.
5. `memory.search` returns full entries; `sessions.search` returns verbatim exchanges from past sessions — no extra model call.

Because memory writes are events, **time travel applies to memory for free**: fork a session before a bad write and the branch never learned it.

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

Drop project instructions in **`MEDHA.md`**, `AGENTS.md`, or `CLAUDE.md` (first match wins, per directory from cwd to the git root, plus `~/.medha/MEDHA.md` for global rules). Files are guard-scanned before entering the prompt; a subdirectory's file is discovered and attached the first time the agent touches that directory. `~/.medha/PERSONA.md` sets the agent's global identity.

Existing `AGENTS.md` / `CLAUDE.md` files work unchanged — adopting MEDHA on an existing repo is zero-config.

### Skills

A **skill** is a versioned `SKILL.md` plus supporting scripts, loaded on demand. Author them by hand, or say "save this as a skill" and approve the `skill.save`. Every skill passes a security guard — static scan plus an LLM judge for ambiguous cases. `/skill lock` and `/skill sync` pin a team's skill set for reproducibility.

### Time travel & undo

```sh
medha undo                # restore the last file write
medha undo --list         # recent writes
medha undo --event <id>   # undo from an event onward
```

In the TUI, `/rewind` branches a new session from an earlier turn — conversation only, code only, or both. The original session is preserved.

### Eval Gate — CI for cognition

```sh
medha gate scenarios/                 # run every scenario
medha gate scenarios/ --seeds 3       # repeat → pass-rate with a Wilson interval
medha gate scenarios/ --json          # machine-readable for CI
```

A scenario declares a fixture workspace, a task, a budget contract, and deterministic checks — command exit codes, file diffs, tools used. Verdict: **promote / hold / reject**, with matching exit codes.

---

## Configuration

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
k3_budget_tokens = 3000
write_approval = "user-scope" # none · user-scope · all
stale_after_days = 30

[context_files]
enabled = true
max_chars = 20000
progressive_discovery = true

[lsp]
enabled = true                # automatic; set false only to disable semantic code intelligence

[mcp]
enabled = true
request_timeout_ms = 60000

[agents]
# max_turns = 40              # operator ceiling on any one child

[verify]
# command = "cargo check"     # deterministic check after file-modifying turns
```

See [`medha.lock.example`](medha.lock.example) for every option, annotated. Quick env overrides: `MEDHA_MAX_TURNS`, `MEDHA_APPROVE`, `MEDHA_SANDBOX`, `MEDHA_MODE`.

> `medha.lock` is per-user and git-ignored. `medha.lock.example` is the shared template.

---

## TUI commands

Type `/` for the palette (Tab completes, ↑↓ navigate menus).

| Command | Does |
|---|---|
| `/help` · `/status` | Commands · model, context window, pressure |
| `/pulse` | Config health: which model/key resolves & from where; `/pulse fix` auto-repairs |
| `/reasoning` | Thinking mode, effort, visibility |
| `/stream` | Toggle live token streaming |
| `/model` · `/search` | Switch model/profile · web-search provider |
| `/mode` | Autonomy: careful · normal · yolo |
| `/memory` | Browse memory with trust/age chips; jump to provenance |
| `/skill` | Load, add, search, update, lock/sync skills |
| `/agents` | Watch the sub-agent tree; reach finished agents |
| `/rewind` · `/resume` | Branch from an earlier turn · reopen a past session |
| `/detail` · `/tasks` | Full tool I/O · background shell tasks |
| `/clear` · `/exit` | Reset conversation · quit (or Ctrl-D) |

---

## Architecture

Fifteen crates. `kernel` is the only code that calls a model, writes an event, or enforces a budget; everything else is a trait behind it.

| Crate | Role |
|---|---|
| [`kernel`](crates/kernel/) | Agent loop, budgets, trust-flow, interrupts, dispatch, artifact spill |
| [`providers`](crates/providers/) | OpenAI-compatible streaming + non-streaming, model discovery |
| [`context`](crates/context/) | Prompt assembly (the five layers), compaction, identity, context files |
| [`memory`](crates/memory/) | Typed memory: projection, ranked recall, consolidation |
| [`tools`](crates/tools/) | 52 tools: fs, shell, web, git, search, LSP, MCP, sub-agents, skills, memory |
| [`orchestrator`](crates/orchestrator/) | Sub-agent runtime: child sessions, capability narrowing, worktrees |
| [`lsp`](crates/lsp/) | Supervised Language Server Protocol host |
| [`mcp`](crates/mcp/) | Supervised Model Context Protocol host, OAuth, remote transports |
| [`policy`](crates/policy/) | Deny-first authorization, shell scanner, content guard |
| [`sandbox`](crates/sandbox/) | Exec backends: host · Seatbelt/Landlock · container · ssh |
| [`store`](crates/store/) | SQLite (WAL) hash-chained event log + FTS5 + artifact store |
| [`lockfile`](crates/lockfile/) | `medha.lock` parsing, defaults, migration |
| [`permissions`](crates/permissions/) | Ask-then-persist trust for out-of-workspace access |
| [`gate`](crates/gate/) | Eval Gate: scenario runner, deterministic checks |
| [`medha-cli`](crates/medha-cli/) | TUI (ratatui), REPL, headless, ACP bridge |

**Provider config precedence:** CLI flag → `MEDHA_*` env → `~/.medha/config.toml` → first-run setup. Only the `MEDHA_*` namespace is read — never a project `.env` or generic `OPENAI_*` / `GOOGLE_*` names. `medha pulse` shows the resolved provenance.

**API keys:** `MEDHA_API_KEY` env → `~/.medha/credentials.toml` (0600) → OS keychain (optional). Keys are never written to `medha.lock`.

---

## Documentation

| Doc | Covers |
|---|---|
| [**docs/WHAT_IS_MEDHA.md**](docs/WHAT_IS_MEDHA.md) | Complete architecture reference — every feature explained in depth |
| [docs/CODE_STYLE.md](docs/CODE_STYLE.md) | Code conventions for contributors |
| [medha.lock.example](medha.lock.example) | Every configuration option, annotated |

---

## Project status

**Shipped and in daily use:** the kernel loop, OpenAI-compatible provider (streaming + non-streaming), the 52-tool suite, all four sandbox backends, deny-first policy, context compaction, typed memory with provenance, context files and persona, the hash-chained event log, time-travel/undo, skills, the LSP host, the MCP host, sub-agents with runtime capability narrowing, and the deterministic Eval Gate.

**On the roadmap:** cross-vendor adversarial verification, span-level trust taint, a deep-research pipeline, trace→skill distillation with eval-gated promotion, native Anthropic/Gemini adapters, and the WebSocket gateway server.

This is pre-1.0 software (`0.0.1`). Interfaces may change.

---

## Contributing

```sh
cargo build --workspace          # build everything
cargo test --workspace           # run the full suite
cargo clippy --workspace         # lint
cargo fmt --all                  # format
```

Read [docs/CODE_STYLE.md](docs/CODE_STYLE.md) before your first change. In short: comments earn their place, tests live beside what they test, and no file becomes a monolith.

Run `medha gate scenarios/` before proposing behavioral changes — it is the regression suite for cognition, not just for code.

---

## Security

MEDHA is a harness that executes model-proposed actions on your machine. Its defenses — deny-first policy, the sandbox, trust-flow escalation, and the human gate — are described in [docs/WHAT_IS_MEDHA.md](docs/WHAT_IS_MEDHA.md).

If you find a vulnerability, please report it privately rather than opening a public issue.

---

## License

Apache-2.0.

> *मेधा सूक्ताय नमः* — Salutations to the hymn of sharp intelligence.
