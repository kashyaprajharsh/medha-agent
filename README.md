# MEDHA

**A verification-first, open-first agent harness — one static binary, no runtime, no Docker.**

MEDHA is a coding agent kernel written in Rust. Point it at any OpenAI-compatible
model endpoint (a local vLLM, Ollama, llama.cpp, or any hosted provider) and it
runs an autonomous loop: read files, run shell commands, fetch the web, edit
code — all behind a deny-first policy, an OS-native sandbox, and a tamper-evident
event log. The model proposes; the harness disposes.

The design is **open-first** (the OpenAI-compatible adapter is the baseline, not
an afterthought) and **verification-first** (every consequential action is gated
by a policy, a human-approval step, and a deterministic post-edit check).

## What's here

This repository builds one binary, `medha`, from a Cargo workspace of ten crates:

| Crate | Role |
|-------|------|
| `kernel` | The agent loop: stream → validate → police → verify → execute. Hash-chained event log, budget governor, trust-flow escalation. |
| `providers` | The OpenAI-compatible streaming adapter (`rustls`, no OpenSSL), plus model-discovery via `/v1/models` and models.dev. |
| `tools` | The tool families — fs, shell, web, git, grep, glob, diagnostics, plan — behind one `Executor`. |
| `sandbox` | The execution seam: host / macOS Seatbelt / Linux Landlock / container / ssh backends. |
| `policy` | Deny-first authorization + the fail-closed shell command scanner. |
| `context` | Budget-aware two-phase compaction (deterministic prune + LLM summarize). |
| `lockfile` | `medha.lock` — the portable, declarative harness configuration. |
| `store` | SQLite (WAL) persistent event log + content-addressed artifact store. |
| `permissions` | Out-of-workspace file access: live ask-then-persist trust flow. |
| `medha-cli` | The `medha` binary: TUI, plain REPL, headless one-shot, and an ACP editor bridge. |

## Build & run

Requires Rust 1.85+ (edition 2024).

```sh
cargo build --release       # → target/release/medha
./target/release/medha --setup   # first run: pick a provider interactively
```

Then:

```sh
medha                        # interactive full-screen TUI (in a terminal)
medha "fix the failing test" # headless one-shot
medha --plain                # scrolling REPL instead of the TUI
medha --acp                  # editor bridge (JSON-RPC over stdio)
```

Provider config resolves in priority order: **CLI flag → env var →
`~/.medha/config.toml` → first-run wizard.** API keys live in the OS keychain,
never in a config file. Env overrides: `MEDHA_BASE_URL`, `MEDHA_MODEL`,
`MEDHA_API_KEY` (also accepts `OPENAI_*` spellings).

## Configuration: `medha.lock`

Drop a `medha.lock` (TOML) in your project root to version the harness
configuration with the code. Absent file = built-in defaults (nothing changes
for a bare checkout). See [`medha.lock.example`](medha.lock.example) for every
section with inline comments. Sections:

- `[budget]` — per-task ceilings (turns, tokens, cost, wall-clock)
- `[context]` — compaction tuning (trigger ratios, protected head/tail)
- `[policy]` — tool classes requiring human approval
- `[sandbox]` — execution backend + network posture
- `[verify]` — a deterministic check (e.g. `cargo check`) run after edits
- `[reasoning]` — request-side thinking control for reasoning models
- `[ui]` — TUI presentation defaults

Session-level env overrides layer on top (precedence: **env > lock > default**):
`MEDHA_MAX_TURNS`, `MEDHA_APPROVE`, `MEDHA_VERIFY`, `MEDHA_SANDBOX`, etc.

## Architecture in brief

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

The kernel is the only code that calls providers and writes the log. Every other
concern — tools, sandbox, policy, verifier, gate, context engine, store — sits
behind a trait, so each is independently swappable.

## Security model

- **Deny-first policy** — every tool intent passes `authorize()` before
  execution; unregistered tools are denied outright.
  ([`crates/policy/src/lib.rs`](crates/policy/src/lib.rs))
- **Fail-closed shell scanner** — `shell.exec` commands are scanned for
  destructive/credential-reading patterns (hard-deny) and obfuscation
  (escalate to human). Ambiguity never fails open.
- **OS-native sandbox** — shell/build commands run behind macOS Seatbelt or
  Linux Landlock (filesystem write-jail), zero external dependencies. Container
  and ssh backends are opt-in heavy tiers.
  ([`crates/sandbox/src/exec.rs`](crates/sandbox/src/exec.rs))
- **Environment clearing** — `shell.exec` starts from an empty env with an
  allowlist, so injected API keys never reach an arbitrary command.
- **Containment-coupled trust-flow** — a web-tainted consequential action is
  auto-escalated to the human gate *unless* the sandbox blocks network
  exfiltration. ([`crates/kernel/src/loop_.rs`](crates/kernel/src/loop_.rs))
- **Tamper-evident log** — every event is SHA-256 hash-chained to the previous;
  the SQLite store verifies the chain on resume.

See [`docs/FEATURES.md`](docs/FEATURES.md) for the full feature reference and
[`PROGRESS.md`](PROGRESS.md) for the phase-by-phase status.

## Status

**Working today** (clean `cargo check`, zero warnings): the full agent loop with
streaming; all ten tool families; the Seatbelt/Landlock/host/container/ssh
sandbox; the deny-first policy + shell scanner; two-phase compaction; the
SQLite event log + artifact store; the TUI, plain REPL, headless mode, and ACP
editor bridge; `medha.lock` configuration; the OpenAI-compatible provider with
reasoning support.

**Not yet built** (roadmap, reconstructed from in-code phase markers — see
[`PROGRESS.md`](PROGRESS.md)): cross-vendor adversarial verification (a second
model as verifier); the full five-sheath context compiler; guided/constrained
decoding for weak-tool-call models; memory and skill subsystems; native
Anthropic/Gemini adapters; `medha undo` over snapshots (snapshots are taken but
the undo surface isn't wired).

> **Note on the spec:** the code comments reference a multi-volume design spec
> ("Vol 1/3/4/7", "§4.x", "Phase 0–7") that is **not present in this
> repository**. The roadmap in [`PROGRESS.md`](PROGRESS.md) is reconstructed from
> the code's own phase language, not from the spec documents themselves.

## License

Apache-2.0.
