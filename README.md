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

This repository builds one binary, `medha`, from a Cargo workspace of eleven crates:

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
| `gate` | The Eval Gate — runs scenarios against the real agent and scores them with deterministic checks over the event log + filesystem ("CI for cognition"). |
| `medha-cli` | The `medha` binary: TUI, plain REPL, headless one-shot, ACP editor bridge, and the `medha gate` subcommand. |

## Build & run

Requires Rust 1.85+ (edition 2024).

```sh
cargo build --release       # → target/release/medha
./target/release/medha      # first run opens the TUI's model setup (or: --setup)
```

Then:

```sh
medha                        # interactive full-screen TUI (in a terminal)
medha "fix the failing test" # headless one-shot
medha --plain                # scrolling REPL instead of the TUI
medha --acp                  # editor bridge (JSON-RPC over stdio)
medha gate scenarios/        # run the eval scenarios (CI for cognition)
```

Provider config resolves in priority order: **CLI flag → env var →
`~/.medha/config.toml` → TUI first-run model setup.** API keys never live in
`config.toml`; they resolve through a layered store — **env var →
`~/.medha/credentials.toml` (owner-only, 0600) → OS keychain**. The owner-only
file is the default, the same convention as other agent CLIs — and it never
triggers macOS password dialogs, which keychain-first storage does on every
rebuild of an unsigned dev binary. Keys stored by older keychain builds are
migrated into the file on first use. `MEDHA_CRED_STORE=keychain` (or building
with `MEDHA_DEFAULT_CRED_STORE=keychain`, meant for signed release binaries)
opts into keychain-first. Keys are cached in-process, so the store is read at
most once per endpoint per run. Env overrides: `MEDHA_BASE_URL`,
`MEDHA_MODEL`, `MEDHA_API_KEY` (also accepts `OPENAI_*` spellings).

### Model profiles

The TUI is the single setup surface — there is no separate terminal wizard.
On a fresh install, `medha` opens straight into the model-setup form; `medha
--setup` opens the same form on demand. `/model` opens an arrow-key model menu
(↑↓ move, Enter/→ choose, Esc/← back). Saved models are listed first with the
active one marked ✓ and preselected — Enter switches between turns; `/model
<name>` switches without opening the menu. Management actions follow the list:
add a model (provider presets or a custom base URL + API key), add or update
an API key, choose the startup default, and remove a saved model (anything but
the active one). The guided add form asks for a profile name, base URL, API
key, model ID, and optional context window. API-key input is masked and stored
only in the secret store above; `config.toml` stores no secret. Wizard-era
configs with a `[provider]` block migrate automatically into a normal named,
removable profile on first load.

## Configuration: `medha.lock`

Drop a `medha.lock` (TOML) in your project root to version the harness
configuration with the code. Absent file = built-in defaults (nothing changes
for a bare checkout). See [`medha.lock.example`](medha.lock.example) for every
section with inline comments. Sections:

- `[budget]` — per-task ceilings (turns, tokens, cost, wall-clock)
- `[context]` — compaction tuning (trigger ratios, protected head/tail)
- `[policy]` — tool classes requiring human approval + the `autonomy` dial default
- `[sandbox]` — execution backend + network posture
- `[verify]` — a deterministic check (e.g. `cargo check`) run after edits
- `[reasoning]` — request-side thinking control for reasoning models
- `[ui]` — TUI presentation defaults
- `[gate]` — eval-gate policy (scenarios dir, promote threshold, seeds)

Session-level env overrides layer on top (precedence: **env > lock > default**):
`MEDHA_MAX_TURNS`, `MEDHA_APPROVE`, `MEDHA_VERIFY`, `MEDHA_SANDBOX`, etc.

## Autonomy: the `/mode` dial

How much the agent does **without asking** is a live dial — `/mode` in the TUI
(an arrow-key picker), `[policy] autonomy` in `medha.lock`, or `MEDHA_MODE`:

| Level | Reversible edits | Safe shell (build/test) |
|-------|------------------|-------------------------|
| `careful` *(default)* | ask | ask |
| `normal` | auto | ask |
| `yolo` | auto | auto |

The differentiator vs other "YOLO" modes (which skip *everything*): **the dial has
an unremovable floor.** It can only ever turn an *allowed* action into an
*approval prompt*, never the reverse — so at **every** level, including `yolo`,
the dangerous-command scanner still hard-denies `rm -rf /` / `sudo` / `curl|sh`,
external actions and `git commit` still hit the human gate, out-of-workspace and
personal-file access is still gated, and web-tainted actions still escalate. A
floor action is **asked** interactively and **denied** when unattended — so
`MEDHA_MODE=yolo` headless can't delete your home directory, it refuses. The TUI
shows a **⚠ yolo** badge so autonomous mode is never invisible.

## The Eval Gate: `medha gate`

Tests for the *agent*, not just the code — "CI for cognition." `medha gate`
runs the real agent against fixture **scenarios** in isolation, then scores each
run with **deterministic checks** and returns a **promote / hold / reject**
verdict. Its exit code gates CI (`0` all promote · `1` any reject · `2` any
hold).

```sh
medha gate scenarios/fix-failing-test            # one scenario
medha gate scenarios/                            # every scenario under a dir
medha gate scenarios/ --seeds 3                  # 3 repeats → pass-rate + 95% CI
medha gate scenarios/ --json                     # machine-readable, for CI
medha gate scenarios/fix-failing-test --validate # lint only, no model run (free)
```

**A scenario** (`scenarios/<id>/scenario.yaml`) is a task + a fixture workspace
+ checks:

```yaml
id: fix-failing-test
task: "Running `sh test.sh` fails. Fix calc.sh so it passes. Don't edit test.sh."
fixture: fixture                       # copied into a throwaway workspace per run
contract: { max_turns: 20, max_wall_s: 300 }
checks:                                # deterministic — no LLM-as-judge
  - command: { run: "sh test.sh", expect_exit: 0 }   # did it actually pass?
  - unchanged: "test.sh"                              # no cheating by editing the test
  - tool_not_used: "web.fetch"                        # a local bug — no web thrashing
  - event_absent: { kind: policy, contains: "dangerous_pattern" }
```

Check kinds: `command` (exit code / stdout), `unchanged`/`changed` (diff vs the
pristine fixture), `exists`/`absent`, `tool_used`/`tool_not_used` (scan the
event log), `event_absent`/`event_present`. Every check is exact and free of any
model call (Vol 5 §2: *"deterministic checks first, judges last"*).

**Why deterministic-first, and why the run is autonomous:** the *agent* run is
stochastic, but the *scoring* is a pure function of the finished run — same run,
same verdict — so multi-seed pass-rates carry a Wilson confidence interval. Each
run is **hermetic**: a fresh temp workspace (the fixture, copied) and a throwaway
`MEDHA_HOME`, so a run never touches your real code or `~/.medha`. Because a gate
run is unattended (no human to approve), the agent runs autonomously; safety
comes from the disposable sandbox + the deny-first scanner, not a human gate.

**Deferred (documented follow-ons):** LLM-as-judge with calibration, canary /
win-rate / promotion / rollback, microVM isolation, `--ablate`, and the
trace→eval flywheel. This ships the deterministic core they build on.

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
  Linux Landlock (filesystem write-jail), zero external dependencies. Medha
  probes the OS jail at startup and explicitly warns/falls back to host mode if
  the platform refuses it. Container and ssh backends are opt-in heavy tiers.
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
sandbox; the deny-first policy + shell scanner + the `/mode` autonomy dial (careful/normal/yolo
with a floor that stays gated at every level); two-phase compaction; the
SQLite event log + artifact store; the TUI, plain REPL, headless mode, and ACP
editor bridge; scoped project/user skills with progressive loading; the human-in-the-loop
`clarify` tool (multi-question radio/checkbox forms that compose with `yolo` —
ask up front, then run autonomous); `medha.lock`
configuration; the OpenAI-compatible provider with reasoning support; and the
deterministic **Eval Gate** (`medha gate`) with scenario runner, event-log +
filesystem checks, multi-seed pass-rates, and JSON/CI output.

**Not yet built** (roadmap, reconstructed from in-code phase markers — see
[`PROGRESS.md`](PROGRESS.md)): cross-vendor adversarial verification (a second
model as verifier); the full five-sheath context compiler; guided/constrained
decoding for weak-tool-call models; long-term memory; the *evolution* layer on
top of the Eval Gate (LLM-as-judge, canary/win-rate, skill promotion/rollback,
the trace→eval flywheel); native Anthropic/Gemini adapters; `medha undo` over
snapshots (snapshots are taken but the undo surface isn't wired).

> **Note on the spec:** the code comments reference a multi-volume design spec
> ("Vol 1/3/4/7", "§4.x", "Phase 0–7") that is **not present in this
> repository**. The roadmap in [`PROGRESS.md`](PROGRESS.md) is reconstructed from
> the code's own phase language, not from the spec documents themselves.

## License

Apache-2.0.
