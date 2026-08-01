# Medha Codebase Audit Issue Ledger

This document tracks the findings from the full-codebase audit that began at
commit `80ac7a4c45f1a1b8afebd7629a069ab57f0078d9`, together with the remediation
working tree reviewed on 2026-08-01.

It covers the kernel and replay model, context and compaction, memory and
persistence, policy and permissions, filesystem and command sandboxing, tools,
providers, LSP/MCP, orchestration and worktrees, Gate, CLI/TUI/ACP surfaces,
configuration, and installers.

Line links refer to the audited commit and may drift as fixes land. Keep the
`AUD-###` identifiers stable when moving an item to an external issue tracker.

## Status key

- The bracket in each issue heading is a compact visual status marker, not a
  GitHub task-list checkbox.
- `[ ]` Open
- `[~]` In progress
- `[x]` Fixed and regression-tested
- `Reproduced` means the failure was observed during the audit.
- `Source-proven` means the complete failure path is visible in production code.
- `Hardening` means a real weakness exists, but impact depends on deployment or
  hostile local/external conditions.
- `Audit coverage gap` means the audit could not establish a result because a
  required specialized check was unavailable.

## Severity guide

- **Critical:** direct trust-boundary failure, arbitrary host access, or durable
  state corruption that invalidates Medha's core safety/replay guarantees.
- **High:** exploitable security issue, data-loss race, indefinite hang, false
  success, or practical process/memory exhaustion.
- **Medium:** bounded correctness, durability, resource, multi-process, or
  platform defect with a concrete trigger.
- **Low:** narrower race, misleading behavior, documentation mismatch, or
  defense-in-depth gap.

## Triage summary

| Severity | Fixed | Validation pending | Open |
|---|---:|---:|---:|
| Critical | 4 | 0 | 0 |
| High | 29 | 1 (AUD-070) | 0 |
| Medium | 28 | 2 (AUD-050, AUD-075) | 0 |
| Low | 8 | 3 (AUD-060, AUD-071, AUD-073) | 0 |
| **Total** | **69** | **6** | **0** |

There are **75 audited findings** in total. `[~]` here means that the production
fix and regression coverage exist, but the closure rule requiring execution on
the relevant hosted/platform environment has not yet been met. It does not mean
that a known production failure path remains unpatched.

## Audit validation baseline

- `cargo test --workspace --all-targets`: **719 passed, 1 ignored**
- Focused ignored process-reaping regression: **failed 3 of 5 runs**
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo fmt --all -- --check`: passed
- Working tree was clean after the read-only audit.
- A dependency-advisory scan was not run because `cargo-audit` and `cargo-deny`
  were not installed. This ledger therefore makes no claim that dependencies
  are free of published vulnerabilities.

## Critical-fix validation

The remediation working tree was validated on 2026-07-29:

- All four Critical findings are fixed and regression-tested.
- The directly related High-severity canonical path-lock race, AUD-022, is also
  fixed and regression-tested.
- `cargo test --workspace --all-targets`: **763 passed, 0 failed, 1 ignored**
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo fmt --all -- --check`: passed
- `git diff --check`: passed
- The one ignored test is the already-marked flaky sandbox success-path helper
  reaping test; no new regression test is ignored.
- These fixes are present in the working tree and have not been committed by
  this remediation pass.

## Full-remediation validation

The current remediation working tree was re-audited on 2026-08-01 after the
subsequent CI, release, Windows, installer, and process-teardown fixes:

- `cargo test --workspace --all-targets`: **918 passed, 0 failed**.
- The full pass allowed localhost loopback for in-process mock HTTP servers;
  under an outer sandbox that forbids socket creation, the MCP/provider/headless
  fixtures fail at `TcpListener::bind` with `EPERM` before exercising Medha.
  They use no external service or Internet dependency.
- The formerly flaky Git cancellation/descendant-reaping regression passed
  **10/10** additional post-suite runs.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `cargo metadata --locked --no-deps --format-version 1`: passed.
- `git diff --check`: passed.
- Pinned `actionlint` 1.7.12 over every workflow: passed.
- Every external GitHub Action reference is pinned to an immutable 40-hex
  commit, and the dependency-security workflow rejects future mutable refs.
- Pinned `cargo-deny` 0.20.2: **advisories ok, licenses ok, sources ok**.
- Selective `x86_64-pc-windows-msvc` all-target checks passed for permissions,
  orchestrator, sandbox, store, Gate, memory, and LSP. A full macOS-hosted
  Windows cross-link is not a substitute for a Windows runner because native
  SDK headers/libraries are unavailable there.
- The remaining `[~]` items require a current hosted GitHub workflow or a real
  Windows runtime. They are listed explicitly in the triage table and again in
  the post-remediation validation gates below.

The original baseline remains above for audit history; this section is the
authoritative current result.

---

## Critical

### [x] AUD-001 — Repository-controlled permission grants are automatically trusted

- **Confidence:** Source-proven
- **Area:** CLI, lockfile, permissions, filesystem trust
- **Evidence:** [`main.rs:987-996`](crates/medha-cli/src/main.rs#L987),
  [`main.rs:1083-1092`](crates/medha-cli/src/main.rs#L1083),
  [`lockfile/src/lib.rs:687-733`](crates/lockfile/src/lib.rs#L687),
  [`permissions/src/lib.rs:117-147`](crates/permissions/src/lib.rs#L117),
  [`permissions/src/lib.rs:275-290`](crates/permissions/src/lib.rs#L275),
  [`permissions/src/lib.rs:423-455`](crates/permissions/src/lib.rs#L423)
- **Failure:** On first use of a workspace, Medha moves the repository-controlled
  `[permissions]` section from `medha.lock` into machine-local `trust.lock`
  without asking the user. The permission manager then loads those paths as
  prompt-free grants. A cloned repository can include a `Read` grant for `/`,
  causing outside-workspace secrets to be readable through `fs.read` and sent
  to the configured model provider.
- **Required fix:** Do not migrate portable grants automatically. Treat every
  repository-provided permission as untrusted input and require explicit,
  path-specific confirmation before writing machine-local trust.
- **Acceptance criteria:** A fresh clone containing a grant for `/`, the home
  directory, or a credential path must not receive access until the local user
  explicitly approves it. Add first-run and existing-trust regression tests.
- **Resolution (2026-07-29):** Repository permission entries are now parsed only
  to emit an ignored-grant warning; they are never imported into local trust.
  Persistent trust must live outside the workspace and is written only after an
  explicit `Always` approval. Per-workspace state is bound to the canonical OS
  path with a SHA-256-derived identity and marker, and identity mismatches fail
  closed. Legacy repository-local runtime state—including `trust.lock`,
  `events.db`, artifacts, logs, and snapshots—is detected and warned about but
  never copied, moved, deleted, or followed through symlinks.
- **Regression coverage:** Fresh malicious grants for `/`, existing path-scoped
  local trust, hard-jail behavior, repository-local trust-path rejection,
  formerly colliding workspace names, mismatched/unmarked state directories,
  legacy runtime entries, and legacy symlink entries.

### [x] AUD-002 — Full-compaction replay drops the protected head and tail

- **Confidence:** Source-proven
- **Area:** Context compaction, event replay, resume
- **Evidence:** [`engine.rs:316-367`](crates/context/src/engine.rs#L316),
  [`engine.rs:409-442`](crates/context/src/engine.rs#L409),
  [`loop_.rs:671-688`](crates/kernel/src/loop_.rs#L671),
  [`events.rs:750-765`](crates/kernel/src/events.rs#L750),
  [`events.rs:896-904`](crates/kernel/src/events.rs#L896)
- **Failure:** The live compacted state is `protected head + middle summary +
  protected tail`, but the durable compaction event stores only the middle
  summary. Both replay projectors clear all earlier messages and insert only
  that summary. After resume, the initial instructions and recent protected
  messages disappear even though the live session retained them.
- **Required fix:** Persist the exact canonical compacted snapshot, or persist
  source-event boundaries sufficient to reconstruct the retained head, summary,
  and tail.
- **Acceptance criteria:** For both legacy and ordered/canonical histories,
  `resume(live_after_compaction)` must reproduce byte-equivalent model input,
  including protected messages and tool-call/result pairing.
- **Resolution (2026-07-29):** Compaction now persists a versioned exact request
  snapshot containing both the canonical ordered provider state and its legacy
  shadow, while compilation carries exact source-occurrence identities through
  transformations. Resume hydrates the stored system sheath and canonical
  provider state rather than reconstructing it by value equality. Snapshot
  validation requires a one-to-one legacy shadow and a closed,
  provider-sendable tool-call/result grammar. A malformed checkpoint is
  atomically inert and cannot fall through to a partial summary.
- **Regression coverage:** Byte-equivalent legacy and ordered replay, duplicate
  equal text with distinct provider signatures, checkpoint-only resume, changed
  current system text, grouped or dangling tool results, malformed checkpoints
  after valid checkpoints, and genuine legacy summaries after checkpoints.

### [x] AUD-003 — Mutation commit order can differ from log order and corrupt rewind/rebuild

- **Confidence:** Source-proven
- **Area:** Kernel concurrency, filesystem writes, memory projection, rewind
- **Evidence:** [`loop_.rs:780-848`](crates/kernel/src/loop_.rs#L780),
  [`loop_.rs:850-913`](crates/kernel/src/loop_.rs#L850),
  [`events.rs:486-533`](crates/kernel/src/events.rs#L486),
  [`projection.rs:108-158`](crates/memory/src/projection.rs#L108)
- **Failure:** Tool side effects run concurrently, while observations are
  appended in model-request order. Rewind assumes the first logged observation
  is the first committed mutation, and memory mutations have no same-key
  serialization. Live state can therefore differ from event-rebuilt state;
  rewind can select a snapshot that is not the original bytes; competing memory
  writes can change version/provenance/trust ordering. The filesystem
  raw-path-alias mechanism that can expose the same ordering defect is tracked
  separately in AUD-022.
- **Required fix:** Serialize mutations by memory key, parallelize only provably
  disjoint mutations, and record actual commit sequence for replay and rollback.
  Apply AUD-022's canonical file-identity fix to filesystem mutations.
- **Acceptance criteria:** Stress tests with reversed completion order and
  competing same-key memory writes, together with AUD-022's alias tests, must
  leave live state, replayed state, and rewind results identical.
- **Resolution (2026-07-29):** The kernel now separates mutation identity from
  blast radius, keeps reads bounded-parallel, and serializes mutations in
  request order. A shared in-process mutex plus a SQLite `BEGIN IMMEDIATE`
  mutation lease spans the side effect, observation append, and memory event
  append, so another process cannot commit a conflicting mutation between state
  change and durable ordering. All memory scopes share a global lane; direct CLI
  and TUI memory changes, undo, and rewind use the same durable discipline.
  Waiting/read tools release the mutation lane, preventing parent/child
  deadlock. Shell background execution is rejected, and timeout/cancellation
  settles the entire process tree before the observation completes.
- **Regression coverage:** Reversed conflicting mutations, same-key memory
  writes, deterministic memory rebuild, independent kernels and SQLite
  connections, parent-waits-for-child mutation, direct CLI memory edits, rewind
  and undo, shell timeout, self-backgrounding, and future cancellation.

### [x] AUD-004 — Gate scenario checks execute unrestricted repository-controlled host commands

- **Confidence:** Source-proven
- **Area:** Eval Gate, host isolation
- **Evidence:** [`checks.rs:121-153`](crates/gate/src/checks.rs#L121),
  [`run.rs:1-8`](crates/gate/src/run.rs#L1)
- **Failure:** A scenario's command check runs synchronously through `sh -c`
  with the workspace as `cwd`, inherited parent environment, unrestricted host
  filesystem/network, unbounded output capture, no timeout, and no process-group
  teardown. This contradicts Gate's hermetic-isolation contract and permits a
  repository-supplied scenario to execute arbitrary host code or hang the Gate.
- **Required fix:** Run checks through the configured sandbox/container using an
  environment allowlist, bounded output, an enforced deadline, and whole-process
  tree termination.
- **Acceptance criteria:** Scenarios cannot read outside approved roots, inherit
  Medha/provider secrets, use forbidden network access, exceed the output cap,
  or survive timeout/cancellation.
- **Resolution (2026-07-29):** Command checks now fail closed unless a trusted
  Docker or Podman runtime outside repository-controlled locations is
  available. The check uses an inert `create` followed by an exact named
  `start`, with no image pull, no network, a read-only root, a clean fixed
  environment, entrypoint and healthcheck overrides, CPU/memory/PID limits, a
  deadline, and bounded output. An owned lifecycle task always force-removes the
  named container on completion, timeout, error, or cancellation; uncertain
  creation races are retried and synchronously covered on drop.
- **Regression coverage:** Host/native/SSH fail-closed behavior, secret-free
  environment and exact argv construction, output bounding, timeout process
  trees, cancellation before workload start, cancellation during create,
  uncertain create cleanup, and drop-time survivor checks.

---

## High

### [x] AUD-005 — Gate fixture paths escape the scenario and recursive copy follows symlinks

- **Confidence:** Source-proven
- **Area:** Eval Gate, filesystem containment
- **Evidence:** [`scenario.rs:204-226`](crates/gate/src/scenario.rs#L204),
  [`run.rs:35-43`](crates/gate/src/run.rs#L35),
  [`run.rs:130-148`](crates/gate/src/run.rs#L130)
- **Failure:** Absolute and `../` fixture paths are accepted. `copy_dir` uses
  `is_dir`/`copy`, follows symlinks, can copy arbitrary host files, recurse
  through symlink cycles, or consume unbounded disk.
- **Required fix:** Canonicalize the fixture beneath the scenario directory,
  reject absolute/traversal paths, and copy with `symlink_metadata` while
  rejecting symlinks and special files.
- **Acceptance criteria:** Add traversal, absolute path, file symlink, directory
  symlink, cycle, FIFO, and device-file tests.

### [x] AUD-006 — Native sandbox allows broad reads of the host filesystem

- **Confidence:** Source-proven
- **Area:** Sandbox, shell, MCP/LSP subprocesses
- **Evidence:** [`exec.rs:857-906`](crates/sandbox/src/exec.rs#L857),
  [`exec.rs:972-1014`](crates/sandbox/src/exec.rs#L972),
  [`tools/src/lib.rs:3910-3950`](crates/tools/src/lib.rs#L3910),
  [`lockfile/src/lib.rs:401-415`](crates/lockfile/src/lib.rs#L401),
  [`main.rs:1059-1076`](crates/medha-cli/src/main.rs#L1059),
  [`WHAT_IS_MEDHA.md:267-318`](docs/WHAT_IS_MEDHA.md#L267)
- **Failure:** macOS Seatbelt is `allow default` with write restrictions; Linux
  Landlock explicitly grants read access beneath `/`. Children keep `HOME` and
  other path variables, default network policy is allowed, and several home
  tool/cache directories are writable. This does not block sensitive reads as
  documented. In yolo, `MEDHA_APPROVE=none`, Gate, or an approved subprocess,
  host credentials can be read and potentially exfiltrated.
- **Required fix:** Introduce a deny-by-default read allowlist, isolate or remap
  `HOME`, never make whole credential-bearing tool directories writable, and
  prefer network deny unless explicitly required.
- **Acceptance criteria:** Platform tests must prove that SSH, cloud, Medha,
  package-manager, Git, Docker, Kubernetes, and shell-history credentials cannot
  be read from native-sandbox children.
- **Resolution (2026-07-31):** The first remediation shipped a deny-by-default
  read profile that omitted the traversal grants a deny-by-default posture
  structurally requires, so `sandbox_apply` aborted every child, the probe
  misread that as "platform unsupported", and selection silently fell back to
  the host backend — a net loss versus the original write jail (AUD-067). Now
  the profile grants the literal root plus directory/symlink metadata only,
  adds `/private/var/select` and the developer-toolchain roots, and keeps
  credential subtrees hard-denied. The availability probe runs the real
  production profile builder — one source of policy — with a separate
  `native_sandbox_supported` probe distinguishing platform refusal from
  profile defects (AUD-068), and a guard test fails the suite if the platform
  can sandbox but our profile does not apply. Runtime approvals flow into the
  exec profile per spawn through the shared `ApprovedRoots` handle, with a
  sandbox-denial escalation prompt and path-scoped retry (AUD-069).
- **Regression coverage:** `seatbelt_children_cannot_read_host_credentials`
  (SSH, AWS, Medha, npm, Git, Docker, Kubernetes, shell-history) now executes
  on every macOS machine that can sandbox instead of skipping on the broken
  probe; write-jail, network-deny-default, live-approval, Once-withdrawal, and
  deny-fails-closed tests alongside it.

### [x] AUD-007 — Shell safety scanner is fail-open for straightforward variants

- **Confidence:** Source-proven
- **Area:** Policy, shell execution
- **Evidence:** [`policy/src/lib.rs:202-229`](crates/policy/src/lib.rs#L202),
  [`policy/src/lib.rs:239-300`](crates/policy/src/lib.rs#L239),
  [`policy/src/lib.rs:349-453`](crates/policy/src/lib.rs#L349),
  [`policy/src/lib.rs:97-115`](crates/policy/src/lib.rs#L97)
- **Failure:** Recursive `rm` detection expects the first option group to contain
  `r`, so forms such as `rm -f -r /` are missed. Literal pipeline matching misses
  wrappers such as `curl URL | env sh`. Interpreters, PowerShell, `find -delete`,
  plain GET exfiltration, and other compositions can bypass the blacklist.
  Careful/normal modes ordinarily gate shell, but yolo/no-approval execution
  relies on this scanner as the safety floor.
- **Required fix:** Parse shell syntax into an AST where supported; treat
  interpreters/wrappers and unparseable or ambiguous syntax as `Human` or deny;
  add platform-specific PowerShell handling.
- **Acceptance criteria:** A mutation/exfiltration corpus covering reordered
  flags, wrappers, variables, interpreters, PowerShell, encoded commands, and
  redirections must fail closed.

### [x] AUD-008 — Skill package scanning can be bypassed by extension and binary content

- **Confidence:** Source-proven
- **Area:** Skill supply chain, policy guard
- **Evidence:** [`guard.rs:69-85`](crates/policy/src/guard.rs#L69),
  [`guard.rs:91-115`](crates/policy/src/guard.rs#L91),
  [`guard.rs:140-167`](crates/policy/src/guard.rs#L140),
  [`tools/src/skills.rs:709-755`](crates/tools/src/skills.rs#L709),
  [`tools/src/skills.rs:848-860`](crates/tools/src/skills.rs#L848)
- **Failure:** Non-UTF-8 files are skipped, while command scanning covers only
  selected extensions, shebangs, and Markdown code. A skill can put executable
  instructions in `payload.dat` or a binary and tell the model to invoke it;
  runtime scanning sees only a benign-looking interpreter command.
- **Required fix:** Quarantine executable/binary assets, inspect interpreter
  input paths at execution time, and require explicit review for unrecognized
  runnable content.
- **Acceptance criteria:** Renamed scripts, binary launchers, extensionless
  payloads, nested archives, and `sh payload.dat` cannot pass as `Safe`.

### [x] AUD-009 — Progressive context bypasses path authorization and upgrades trust

- **Confidence:** Source-proven
- **Area:** Context discovery, permissions, trust flow
- **Evidence:** [`ctxfiles.rs:229-245`](crates/context/src/ctxfiles.rs#L229),
  [`ctxfiles.rs:323-347`](crates/context/src/ctxfiles.rs#L323),
  [`loop_.rs:834-876`](crates/kernel/src/loop_.rs#L834)
- **Failure:** Absolute touched paths are accepted and ancestor context files are
  read without enforcing the workspace/approved-root boundary. Discovery runs
  even when the underlying tool call was denied or failed, then emits the file
  as `Workspace` trust. A denied `/tmp/untrusted/file` request can still load
  `/tmp/untrusted/AGENTS.md`.
- **Required fix:** Canonicalize against an authorized root set, discover only
  after a successful path-touching operation, and preserve the source's actual
  trust label.
- **Acceptance criteria:** Denied, missing, external, symlinked, and failed paths
  cannot inject context or create workspace-trusted context events.

### [x] AUD-010 — `web.fetch` SSRF guard is vulnerable to DNS rebinding

- **Confidence:** Source-proven
- **Area:** Web tools, networking
- **Evidence:** [`tools/src/lib.rs:2746-2792`](crates/tools/src/lib.rs#L2746),
  [`tools/src/lib.rs:3478-3517`](crates/tools/src/lib.rs#L3478)
- **Failure:** Medha resolves and validates the hostname, but Reqwest performs a
  separate resolution when connecting. A hostile DNS server can return a public
  IP for validation and a loopback, RFC1918, link-local, or cloud-metadata IP for
  the actual request. Redirect validation repeats the same check/use gap.
- **Required fix:** Pin validated IPs into the request resolver, validate the
  connected peer, and repeat pinning independently for every redirect hop.
- **Acceptance criteria:** Deterministic rebinding tests must fail for IPv4,
  IPv6, redirects, mixed public/private answers, and metadata targets.

### [x] AUD-011 — Policy-decision log failure is ignored and execution proceeds

- **Confidence:** Source-proven
- **Area:** Kernel, event durability, policy
- **Evidence:** [`loop_.rs:1243-1296`](crates/kernel/src/loop_.rs#L1243)
- **Failure:** The result of appending `Event::policy` is discarded with `.ok()`.
  If the event database is full, locked, or corrupt, a consequential tool can
  execute without the required policy record. A later observation failure then
  leaves changed external state without the claimed intent → policy →
  observation audit chain.
- **Required fix:** Fail closed when the policy record is not durable. Use a
  write-ahead/outbox protocol for side effects that must survive later logging
  failure.
- **Acceptance criteria:** Injected append failures before policy, observation,
  and memory-write records must never produce an untracked side effect.

### [x] AUD-012 — Event hash chain omits security-relevant fields

- **Confidence:** Source-proven
- **Area:** Event log integrity
- **Evidence:** [`events.rs:597-614`](crates/kernel/src/events.rs#L597),
  [`store/src/lib.rs:249-309`](crates/store/src/lib.rs#L249)
- **Failure:** The chain authenticates previous hash, kind, session, payload, and
  timestamp, but not event ID, parent ID, trust, or provenance. Direct changes
  from `web` to `user`, automation to interactive, or altered cross-event IDs
  still verify. Suffix deletion is also undetectable without an external anchor.
- **Required fix:** Version a canonical encoding covering every event field and
  anchor the terminal hash plus row count outside the mutable event table.
- **Acceptance criteria:** One-bit changes to every stored field, row reparenting,
  row deletion, suffix truncation, and row reordering must fail verification.

### [x] AUD-013 — Wall, token, and cost budgets are not hard ceilings

- **Confidence:** Source-proven
- **Area:** Kernel budgets, agents, providers
- **Evidence:** [`budgets.rs:151-203`](crates/kernel/src/budgets.rs#L151),
  [`loop_.rs:609-616`](crates/kernel/src/loop_.rs#L609),
  [`loop_.rs:714-727`](crates/kernel/src/loop_.rs#L714)
- **Failure:** Budgets are checked only between turns. One provider/tool/verifier
  operation can exceed wall time indefinitely or overshoot the remaining token
  and cost allowance. Providers that omit `Usage` record zero spend. Concurrent
  agents can all pass a shared-pool check before any reserves or records spend.
- **Required fix:** Create a task deadline used by every await, reserve worst-case
  spend atomically before requests, reconcile against actual usage, and define a
  fail-closed accounting policy for missing usage.
- **Acceptance criteria:** Single-turn hangs, missing usage, one-response
  overshoot, and concurrent-agent reservation tests stay within configured
  ceilings.

### [x] AUD-014 — Context compaction and its summarizer cannot be cancelled

- **Confidence:** Source-proven
- **Area:** Context, provider streaming, cancellation
- **Evidence:** [`kernel/src/context.rs:33-58`](crates/kernel/src/context.rs#L33),
  [`loop_.rs:640-647`](crates/kernel/src/loop_.rs#L640),
  [`engine.rs:421-427`](crates/context/src/engine.rs#L421),
  [`compactor.rs:311-323`](crates/context/src/compactor.rs#L311)
- **Failure:** `ContextEngine::compile` has no cancellation/deadline input. Full
  compaction awaits the summarizer, whose provider connection and stream drain
  have neither cancellation nor timeout. A stalled summarizer makes Esc and wall
  budgets ineffective.
- **Required fix:** Thread cancellation and deadline through compilation and
  summarization; race connection and every stream read against both.
- **Acceptance criteria:** Cancelling before connection, mid-stream, and during
  fallback returns promptly and leaves consistent context state.

### [x] AUD-015 — Cancellation can start queued tools and multiply settle grace

- **Confidence:** Source-proven
- **Area:** Kernel cancellation, parallel dispatch
- **Evidence:** [`loop_.rs:780-848`](crates/kernel/src/loop_.rs#L780)
- **Failure:** Every intent is logged as admitted before bounded dispatch.
  Futures that were not running when cancellation occurred can later start and
  each receives a fresh settle-grace interval. At cap 1, 100 calls and a
  five-second grace can approach 500 seconds and begin 99 actions after cancel.
- **Required fix:** Distinguish queued from started calls, synthesize immediate
  interrupted observations for never-started calls, and apply one shared settle
  deadline to the active set.
- **Acceptance criteria:** Cancellation latency is bounded by one grace period
  regardless of queue length, and no never-started action executes.

### [x] AUD-016 — Default tool fan-out and provider accumulation permit practical OOM/FD exhaustion

- **Confidence:** Source-proven
- **Area:** Kernel, provider streaming, tool dispatch
- **Evidence:** [`loop_.rs:20-24`](crates/kernel/src/loop_.rs#L20),
  [`loop_.rs:808-848`](crates/kernel/src/loop_.rs#L808),
  [`loop_.rs:907-912`](crates/kernel/src/loop_.rs#L907),
  [`loop_.rs:1114-1198`](crates/kernel/src/loop_.rs#L1114)
- **Failure:** Default parallelism is 10,000. All complete observations are
  collected before payload spilling, while streamed text, reasoning, calls, and
  canonical parts grow without a byte/token cap. A hostile or defective provider
  can create thousands of calls or an effectively infinite stream.
- **Required fix:** Use a conservative default such as 8–32, impose absolute
  intent/byte limits, spill incrementally, and stop provider accumulation at a
  hard bound.
- **Acceptance criteria:** Adversarial high-fan-out and infinite-stream tests
  terminate within fixed memory, descriptor, and time budgets.

### [x] AUD-017 — Foreground command capture reads stdout/stderr without a bound

- **Confidence:** Source-proven
- **Area:** Sandbox execution, Gate, Git, diagnostics
- **Evidence:** [`exec.rs:636-675`](crates/sandbox/src/exec.rs#L636),
  [`tools/src/lib.rs:4638-4659`](crates/tools/src/lib.rs#L4638),
  [`worktree.rs:117-173`](crates/orchestrator/src/worktree.rs#L117),
  [`gate/src/run.rs:45-52`](crates/gate/src/run.rs#L45),
  [`gate/src/run.rs:82-85`](crates/gate/src/run.rs#L82)
- **Failure:** `ExecBackend::run` drains both pipes with unbounded `read_to_end`.
  Git truncates stdout only after capture; orchestration bounds Git stdout but
  still captures stderr without a cap. Gate's main Medha child also pipes both
  streams into unbounded `wait_with_output`. Repository-controlled
  build/compiler/Git/agent output can exhaust process memory before outer
  handling runs.
- **Required fix:** Use rolling or spill-to-artifact capture with independent
  stdout/stderr and aggregate caps across every execution path.
- **Acceptance criteria:** Multi-gigabyte stdout/stderr generators stay within a
  fixed RSS envelope and return explicit truncation/artifact metadata.

### [x] AUD-018 — Successful bounded commands can orphan background helpers

- **Confidence:** Reproduced
- **Area:** Sandbox process lifecycle, verifier/install commands
- **Evidence:** [`exec.rs:535-559`](crates/sandbox/src/exec.rs#L535),
  [`exec.rs:1498-1531`](crates/sandbox/src/exec.rs#L1498)
- **Failure:** Pipe EOF is used to decide when to kill the process group, but a
  helper that redirects its pipes may not have joined the group yet. The ignored
  regression failed three of five focused runs with
  `successful verifier orphaned a background helper`.
- **Required fix:** Keep the leader identity valid while repeatedly confirming
  group quiescence, or use a stronger supervisor/job/cgroup abstraction.
- **Acceptance criteria:** Run the existing ignored test hundreds of times under
  load with zero surviving helpers; make it a normal CI test.
- **Resolution (2026-07-31):** Completion is decided by `waitid(WNOWAIT)` group
  quiescence rather than pipe EOF; the orphan assertion has not failed in any
  run. The 128-way stress test is a normal (non-ignored) CI test; its residual
  flake was the unrelated `passed()` assertion tripping on scheduler-induced
  timeouts under full-suite load, so it now runs with a 30 s bound and accepts
  the bounded kill as an outcome — the zero-survivors count remains the strict
  invariant, and a killed group must be reaped exactly like a completed one.

### [x] AUD-019 — Background-task completion can leave descendants and pipe pumps alive

- **Confidence:** Source-proven
- **Area:** Shell background tasks, process cleanup
- **Evidence:** [`exec.rs:712-754`](crates/sandbox/src/exec.rs#L712),
  [`exec.rs:774-827`](crates/sandbox/src/exec.rs#L774),
  [`tools/src/lib.rs:4099-4108`](crates/tools/src/lib.rs#L4099)
- **Failure:** After the direct child exits, pipe-pump joins are abandoned after
  200 ms and the task is marked done. Session cleanup kills only tasks whose
  direct child still reports running. A descendant holding a pipe or continuing
  after its leader exits may survive cleanup while detached pump tasks retain
  buffers.
- **Required fix:** Track process-group liveness separately from leader status,
  abort/join pump tasks explicitly, and kill all known groups on session drop.
- **Acceptance criteria:** A leader that exits after spawning redirected and
  pipe-holding descendants leaves no processes, tasks, or retained buffers.

### [x] AUD-020 — Gate treats any normal child exit as completed, including non-zero failure

- **Confidence:** Source-proven
- **Area:** Eval Gate verdict correctness
- **Evidence:** [`run.rs:79-101`](crates/gate/src/run.rs#L79),
  [`verdict.rs:30-43`](crates/gate/src/verdict.rs#L30)
- **Failure:** `wait_with_output()` returning successfully sets `completed=true`
  without checking `status.success()`. If fixture checks pass, a crashed or
  non-zero Medha run can be counted as a passing seed and promoted.
- **Required fix:** Store exit status separately and require a successful exit in
  `SeedResult::passed`.
- **Acceptance criteria:** Exit codes, signals, launch errors, and timeouts each
  produce distinct failed run results regardless of fixture state.

### [x] AUD-021 — Gate timeout kills only the direct Medha child

- **Confidence:** Source-proven
- **Area:** Eval Gate process lifecycle
- **Evidence:** [`run.rs:45-52`](crates/gate/src/run.rs#L45),
  [`run.rs:75-91`](crates/gate/src/run.rs#L75)
- **Failure:** Gate relies on Tokio `kill_on_drop`, which targets only the direct
  child. Shell, build, LSP, MCP, and compiler descendants can survive timeout or
  cancellation and retain ports, locks, CPU, or files.
- **Required fix:** Start the run in an owned process group/job/cgroup, kill the
  entire tree, and wait for reaping before scoring.
- **Acceptance criteria:** Timeout tests with nested grandchildren on Unix and
  Windows leave no live process and release held ports/files.

### [x] AUD-022 — Raw path aliases bypass same-file write serialization

- **Confidence:** Source-proven
- **Area:** Filesystem tools, concurrency
- **Evidence:** [`sandbox/src/lib.rs:42-46`](crates/sandbox/src/lib.rs#L42),
  [`sandbox/src/lib.rs:98-113`](crates/sandbox/src/lib.rs#L98)
- **Failure:** Locks are keyed by the model's raw path text. `x`, `./x`, an
  absolute spelling, case aliases, and symlink aliases can refer to one target
  while acquiring different mutexes. Concurrent read-modify-write operations can
  both succeed while one silently overwrites the other.
- **Required fix:** Resolve first and key locks by canonical target identity,
  handling not-yet-created files through a secured canonical parent plus name.
- **Acceptance criteria:** Alias, symlink, case-folding, and new-file concurrency
  tests serialize one physical target.
- **Resolution (2026-07-29):** Write guards now resolve and authorize the target
  before locking, then key the lock by canonical physical identity. A
  not-yet-created target uses its secured canonical existing parent plus the
  unresolved tail; case-insensitive platforms normalize case. The guard pins
  that resolved target through write/restore, and `fs.write`, `fs.edit`, and
  `fs.multi_edit` use the guarded operation.
- **Regression coverage:** Relative-dot and absolute aliases, symlink leaf
  aliases, platform case aliases, and concurrent read-modify-write of a new
  nested file. This closes path-alias serialization only; AUD-024's
  parent-component replacement race remains open.

### [x] AUD-023 — `fs.write` approval preview is not pinned to execution state

- **Confidence:** Source-proven
- **Area:** Approval gate, filesystem integrity
- **Evidence:** [`tools/src/lib.rs:1514-1603`](crates/tools/src/lib.rs#L1514),
  [`tools/src/lib.rs:1677-1700`](crates/tools/src/lib.rs#L1677)
- **Failure:** `fs.write` previews a diff against current content but has no
  equivalent of the preview pin used by edit tools. The file may change while
  approval is displayed, so execution overwrites content the user never saw in
  the approved diff.
- **Required fix:** Pin a strong content hash/file identity during preview and
  verify it under the canonical path lock immediately before writing.
- **Acceptance criteria:** Any intervening create, delete, replace, or content
  change invalidates approval and produces a fresh preview.

### [x] AUD-024 — Parent-component symlink swap can redirect an authorized write

- **Confidence:** Source-proven
- **Area:** Filesystem sandbox, permissions
- **Evidence:** [`permissions/src/lib.rs:174-210`](crates/permissions/src/lib.rs#L174),
  [`sandbox/src/lib.rs:224-266`](crates/sandbox/src/lib.rs#L224),
  [`sandbox/src/lib.rs:311-348`](crates/sandbox/src/lib.rs#L311),
  [`sandbox/src/lib.rs:401-421`](crates/sandbox/src/lib.rs#L401),
  [`sandbox/src/lib.rs:465-470`](crates/sandbox/src/lib.rs#L465)
- **Failure:** Medha canonicalizes an existing ancestor, authorizes the resulting
  path, then later creates/writes/renames by pathname. A concurrent process can
  replace an intermediate directory with a symlink after authorization.
  `restore()` additionally copies to the raced final pathname.
- **Required fix:** Use capability-based directory handles or Linux `openat2`
  `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS`, with platform equivalents.
- **Acceptance criteria:** A stress test swapping every parent component cannot
  cause write, restore, snapshot, or directory creation outside the root.
- **Resolution (2026-07-31):** Fixed on Unix via `UnixWriteCapability`: the
  authorized directory is held as an open handle and every later component is
  traversed with `openat`+`O_NOFOLLOW`, so a swapped parent cannot redirect
  the write.
- **SCOPE (Windows):** the `#[cfg(not(unix))]` branches still operate by
  pathname after authorization, so the race remains open there — the same
  accepted platform gap as the Windows exec sandbox (see AUD-060's tier note).
  Documented on the capability type; a port needs `NtCreateFile` relative
  opens against a held directory handle.

### [x] AUD-025 — ACP and TUI exit with detached foreground turns still running

- **Confidence:** Source-proven
- **Area:** ACP, TUI, shutdown
- **Evidence:** [`acp.rs:261-318`](crates/medha-cli/src/acp.rs#L261),
  [`main.rs:1617-1631`](crates/medha-cli/src/main.rs#L1617),
  [`main.rs:1681-1688`](crates/medha-cli/src/main.rs#L1681),
  [`update.rs:4123`](crates/medha-cli/src/tui_tea/update.rs#L4123),
  [`update.rs:5175-5177`](crates/medha-cli/src/tui_tea/update.rs#L5175),
  [`tui_tea/mod.rs:2447-2451`](crates/medha-cli/src/tui_tea/mod.rs#L2447)
- **Failure:** Foreground turns are spawned without a retained join handle.
  Shutdown/EOF or `/exit` breaks the surface loop immediately. Active tools can
  be dropped mid-operation, while child processes or blocking work may continue.
- **Required fix:** Own the foreground task, cancel it on shutdown, deny/drain
  pending approvals, and join it with a bounded process-safe grace period.
- **Acceptance criteria:** Exit during provider streaming, approval, file write,
  shell, verifier, LSP/MCP, and agent activity leaves consistent logs and no live
  work.

### [x] AUD-026 — ACP JSON-RPC requests frequently receive no response

- **Confidence:** Source-proven
- **Area:** ACP protocol
- **Evidence:** [`acp.rs:238-322`](crates/medha-cli/src/acp.rs#L238)
- **Failure:** Only `initialize`/`hello` replies to a request ID. Requests carrying
  IDs for `message.send`, cancel, shutdown, approval response, and unknown
  methods are silently left unanswered, so conforming clients can wait forever.
- **Required fix:** Return a result or JSON-RPC error for every message with an
  ID; distinguish notifications from requests.
- **Acceptance criteria:** A protocol conformance test asserts exactly one
  response for every supported, invalid, busy, and unknown request.

### [x] AUD-027 — ACP stdout can block the Tokio runtime indefinitely

- **Confidence:** Source-proven
- **Area:** ACP I/O, async runtime
- **Evidence:** [`acp.rs:25-52`](crates/medha-cli/src/acp.rs#L25)
- **Failure:** Async tasks take a synchronous mutex and perform blocking
  `writeln!` plus `flush` directly to stdout. If the editor stops reading, a
  runtime worker can block while holding the writer lock and stall unrelated
  cancellation/progress.
- **Required fix:** Send frames through a bounded async channel to one async
  writer task; treat backpressure/broken pipe as connection cancellation.
- **Acceptance criteria:** A non-reading client cannot block runtime timers,
  cancellation, process cleanup, or other sessions.

### [x] AUD-028 — Cross-process worktree sweep can delete a newly created live checkout

- **Confidence:** Source-proven
- **Area:** Agents, Git worktrees, multi-process race
- **Evidence:** [`worktree.rs:515-576`](crates/orchestrator/src/worktree.rs#L515),
  [`worktree.rs:588-665`](crates/orchestrator/src/worktree.rs#L588)
- **Failure:** `git worktree add` completes before the owner marker is written.
  A second Medha process can sweep during that gap, see a clean markerless
  checkout, and force-remove it while the first process is admitting the child.
- **Required fix:** Use a repository-wide cross-process lock or publish a
  sibling lease atomically before making the worktree visible.
- **Acceptance criteria:** A two-process barrier test around add/marker/sweep
  cannot remove a checkout owned by either process.

### [x] AUD-029 — Headless kernel/provider failures return exit status zero

- **Confidence:** Source-proven
- **Area:** CLI, CI automation
- **Evidence:** [`main.rs:1691-1751`](crates/medha-cli/src/main.rs#L1691)
- **Failure:** Headless mode prints `run_session` errors but ultimately returns
  `Ok(())`. Scripts and CI cannot distinguish a failed provider/kernel run from
  success.
- **Required fix:** Propagate the error or return a documented non-zero status;
  define separate exit codes for budget stop, rejection, and internal failure if
  needed.
- **Acceptance criteria:** CLI integration tests assert non-zero exit for
  provider, event-log, kernel, and tool-system failures.

### [x] AUD-030 — Resumed canonical tool results bypass artifact spilling

- **Confidence:** Source-proven
- **Area:** Resume, provider request building, memory use
- **Evidence:** [`loop_.rs:484-493`](crates/kernel/src/loop_.rs#L484),
  [`loop_.rs:547-563`](crates/kernel/src/loop_.rs#L547),
  [`events.rs:875-894`](crates/kernel/src/events.rs#L875),
  [`openai_chat.rs:87`](crates/providers/src/protocol/openai_chat.rs#L87)
- **Failure:** Initial legacy messages are spill-bounded, but ordered hydration
  reconstructs complete `ToolResultPart.content`. Real adapters prefer ordered
  history, so a resumed session can resend arbitrarily large prior tool payloads
  and hit context/HTTP/allocation failures.
- **Required fix:** Apply equivalent spill semantics during ordered hydration
  while retaining opaque provider state and tool-call identity.
- **Acceptance criteria:** Live and resumed requests for a tool result above the
  spill threshold have equivalent bounded size and content references.

### [x] AUD-065 — Orchestrator Git subprocesses have no deadline or cancellation-safe teardown

- **Confidence:** Source-proven
- **Area:** Orchestrator, Git, cancellation, process lifecycle
- **Evidence:** [`worktree.rs:120-173`](crates/orchestrator/src/worktree.rs#L120),
  [`worktree.rs:179-202`](crates/orchestrator/src/worktree.rs#L179),
  [`worktree.rs:791-829`](crates/orchestrator/src/worktree.rs#L791)
- **Failure:** Worktree creation, patch extraction, verification support, apply,
  and cleanup await Git without a deadline. The commands do not set
  `kill_on_drop`; dropping a future during cancellation does not guarantee that
  its Git child is terminated. Some paths also capture output without bounds.
  A stuck credential helper, hook, filesystem, or Git process can indefinitely
  block agent settlement/worktree cleanup and leave a child alive.
- **Required fix:** Give every Git subprocess a bounded deadline, set
  cancellation-safe child teardown, terminate its process tree, and use bounded
  or artifact-backed output capture.
- **Acceptance criteria:** Hung Git and credential-helper fixtures cannot outlive
  cancellation/deadline, agent settlement completes within a fixed bound, and
  no child process or worktree lease remains.
- **Resolution (2026-08-01):** Every orchestrator Git command now runs with a
  bounded deadline, bounded stdout/stderr capture, process-group teardown, and
  `kill_on_drop`. The pipe-pump task is wrapped in an abort-on-drop guard so an
  I/O error or early return cannot detach it. Deadline and dropped-future
  regressions verify that descendants disappear and file locks are released;
  the full-suite timing race found during this review is covered with a
  settlement observation window longer than the production grace period.

---

## Medium

### [x] AUD-031 — Finished background tasks and retained output are never evicted

- **Confidence:** Source-proven
- **Area:** Shell tasks, memory growth
- **Evidence:** [`tools/src/lib.rs:3981-3998`](crates/tools/src/lib.rs#L3981),
  [`tools/src/lib.rs:4064-4071`](crates/tools/src/lib.rs#L4064),
  [`tools/src/lib.rs:4253-4260`](crates/tools/src/lib.rs#L4253),
  [`exec.rs:678-708`](crates/sandbox/src/exec.rs#L678)
- **Failure:** Only a task that completes inside the initial foreground window is
  removed. Explicitly backgrounded, promoted, polled, exited, and killed tasks
  stay in `TaskTable` indefinitely. Each can retain approximately 1 MB of stdout
  plus 1 MB of stderr, command text, channels, and metadata.
- **Required fix:** Add total task/output limits, TTL or LRU eviction, explicit
  `task.remove`, and automatic removal/reaping of terminal entries.
- **Acceptance criteria:** Tens of thousands of short background tasks keep
  table size and RSS within configured bounds while recent results remain
  inspectable.

### [x] AUD-032 — The per-path write-lock map grows for the process lifetime

- **Confidence:** Source-proven
- **Area:** Filesystem tools, memory growth
- **Evidence:** [`sandbox/src/lib.rs:42-46`](crates/sandbox/src/lib.rs#L42),
  [`sandbox/src/lib.rs:104-113`](crates/sandbox/src/lib.rs#L104)
- **Failure:** Every unique raw path spelling inserts an `Arc<Mutex>` into a
  `HashMap`; no entry is removed. Long sessions or adversarial path spellings
  create unbounded metadata growth in addition to the alias race in AUD-022.
- **Required fix:** Use canonical keys and weak/ref-counted entries that are
  removed after the final guard/user disappears.
- **Acceptance criteria:** A stress test with many unique paths returns the lock
  table to a small steady-state size.

### [x] AUD-033 — Memory projection mutations and rebuilds are not transactional

- **Confidence:** Source-proven
- **Area:** Memory, SQLite, FTS
- **Evidence:** [`projection.rs:108-158`](crates/memory/src/projection.rs#L108),
  [`projection.rs:176-187`](crates/memory/src/projection.rs#L176),
  [`projection.rs:249-294`](crates/memory/src/projection.rs#L249)
- **Failure:** Upsert/forget changes primary rows and FTS through separate
  statements. Rebuild clears live tables and then applies events incrementally,
  silently skipping malformed operations. Crash or SQLite failure can leave FTS
  divergent or a previously valid projection partially empty.
- **Required fix:** Use one transaction per mutation and build into staging
  tables before an atomic swap; malformed durable events must fail visibly.
- **Acceptance criteria:** Failure injection at every statement preserves the
  prior valid projection or commits the complete new one, never a mixture.

### [x] AUD-034 — Context-file limits are enforced after full read and clone

- **Confidence:** Source-proven
- **Area:** Context files, async runtime, memory use
- **Evidence:** [`ctxfiles.rs:91-134`](crates/context/src/ctxfiles.rs#L91)
- **Failure:** `AGENTS.md`, `MEDHA.md`, and `PERSONA.md` are synchronously read in
  full before truncation. Caution paths clone the complete content for the judge.
  A huge context file can block a Tokio worker and allocate far above the
  advertised 8K/20K limits.
- **Required fix:** Read bounded head/tail data on a blocking worker and cap judge
  input independently.
- **Acceptance criteria:** Multi-gigabyte context-file tests stay within a fixed
  allocation and do not stall async timers.

### [x] AUD-035 — Ranged `fs.read` still loads and splits the entire file

- **Confidence:** Source-proven
- **Area:** Filesystem tools, memory use
- **Evidence:** [`tools/src/lib.rs:1435-1495`](crates/tools/src/lib.rs#L1435),
  [`sandbox/src/lib.rs:386-399`](crates/sandbox/src/lib.rs#L386)
- **Failure:** The 2 MB guard applies only to whole-file requests. Supplying
  `offset`/`limit` bypasses the guard, but `read_resolved` still loads the entire
  file and the tool builds a vector of all line slices before returning a range.
- **Required fix:** Implement bounded streaming/seeked line-range reads and apply
  an absolute input-byte ceiling.
- **Acceptance criteria:** Reading ten lines from a multi-gigabyte file uses
  bounded memory proportional to the requested range.

### [x] AUD-036 — Artifact writes are non-atomic and can preserve a corrupt hash-named blob

- **Confidence:** Source-proven
- **Area:** Artifact store, durability
- **Evidence:** [`store/src/lib.rs:151-173`](crates/store/src/lib.rs#L151)
- **Failure:** `put` checks `path.exists()` then writes directly to the final
  hash-named path. Crash/concurrent read can expose a partial blob; later puts
  see that the path exists and never repair or verify it.
- **Required fix:** Write and fsync a sibling temporary file, verify its digest,
  atomically publish it, and verify any pre-existing blob before trusting it.
- **Acceptance criteria:** Crash and concurrent put/get tests never return bytes
  whose digest differs from the requested artifact hash.

### [x] AUD-037 — Artifact range reads load the whole blob and can overflow

- **Confidence:** Source-proven
- **Area:** Artifact store, panic/OOM
- **Evidence:** [`store/src/lib.rs:174-183`](crates/store/src/lib.rs#L174)
- **Failure:** `get` reads the complete artifact before slicing. Its
  `start + len` arithmetic is unchecked; a large `len` can overflow and produce
  a panic or invalid range behavior.
- **Required fix:** Seek and read only the requested bounded range, use checked or
  saturating arithmetic, and cap tool-requested length.
- **Acceptance criteria:** `usize::MAX`, offsets past EOF, huge blobs, and
  concurrent readers return bounded deterministic results without panic.

### [x] AUD-038 — Permission persistence is non-atomic and diverges from in-memory trust

- **Confidence:** Source-proven
- **Area:** Permissions, trust durability, multi-process
- **Evidence:** [`permissions/src/lib.rs:293-317`](crates/permissions/src/lib.rs#L293),
  [`permissions/src/lib.rs:319-383`](crates/permissions/src/lib.rs#L319)
- **Failure:** Trust is added to memory before persistence. Persistence performs
  an unlocked, non-atomic read-modify-write. A disk failure leaves a live grant
  that disappears on restart; concurrent Medha processes can lose each other's
  grants or expose a truncated trust file.
- **Required fix:** Cross-process lock a stable sibling, write atomically, and
  publish the in-memory grant only after durable success.
- **Acceptance criteria:** Concurrent writers and injected write/rename failures
  preserve every grant and keep disk and memory identical.

### [x] AUD-039 — Gate run artifacts permanently leak temporary directories

- **Confidence:** Source-proven
- **Area:** Eval Gate, disk resources
- **Evidence:** [`run.rs:35-43`](crates/gate/src/run.rs#L35),
  [`run.rs:94-101`](crates/gate/src/run.rs#L94)
- **Failure:** Every seed creates `medha-gate-<ULID>` with workspace, pristine
  copy, home, event DB, and artifacts. `RunArtifact` holds plain paths; no
  `TempDir`, `Drop`, or production cleanup removes them.
- **Required fix:** Own a `TempDir` through evaluation and add an explicit
  keep-on-failure/debug option.
- **Acceptance criteria:** Successful, failed, timed-out, cancelled, and panicked
  Gate runs clean their directories unless preservation was requested.

### [x] AUD-040 — Invalid and zero-match `unchanged` checks pass silently

- **Confidence:** Source-proven
- **Area:** Eval Gate scoring
- **Evidence:** [`checks.rs:53-73`](crates/gate/src/checks.rs#L53),
  [`checks.rs:155-184`](crates/gate/src/checks.rs#L155)
- **Failure:** Invalid glob parsing and a pattern matching no files both produce
  an empty diff set. `unchanged` then passes, allowing a typo or vacuous check to
  rubber-stamp a run.
- **Required fix:** Validate globs during scenario load and require
  `unchanged`/`changed` checks to declare whether zero baseline matches are
  permitted.
- **Acceptance criteria:** Invalid globs and accidental zero-match patterns fail
  validation with the scenario path and pattern.

### [x] AUD-041 — Gate threshold and seed inputs are insufficiently validated

- **Confidence:** Source-proven
- **Area:** Eval Gate, cost/resource controls
- **Evidence:** [`main.rs:190-193`](crates/medha-cli/src/main.rs#L190),
  [`gate/src/lib.rs:61-84`](crates/gate/src/lib.rs#L61),
  [`verdict.rs:57-75`](crates/gate/src/verdict.rs#L57)
- **Failure:** A threshold at or below zero, including negative infinity,
  promotes every scenario—even with zero passing seeds—because promotion is
  checked first. NaN, positive infinity, and values above one instead prevent
  promotion or force Hold/Reject. Very large seed counts preallocate a large
  vector and trigger unbounded paid model work. No practical ceiling is applied.
- **Required fix:** Require finite threshold in `(0,1]`, validate seed count
  against a documented maximum, avoid eager large capacity, and confirm costly
  runs.
- **Acceptance criteria:** Negative, NaN, infinite, above-one, zero, and huge
  values fail before provider calls or allocation.

### [x] AUD-042 — ACP resume discards the loaded prior transcript

- **Confidence:** Source-proven
- **Area:** ACP, resume
- **Evidence:** [`main.rs:1526`](crates/medha-cli/src/main.rs#L1526),
  [`main.rs:1617-1629`](crates/medha-cli/src/main.rs#L1617),
  [`acp.rs:229`](crates/medha-cli/src/acp.rs#L229)
- **Failure:** Main loads resumed history, but the ACP entry point is not given
  it and constructs a transcript containing only the system message. Editor
  resume therefore silently starts without the prior conversation.
- **Required fix:** Pass the hydrated transcript/canonical history into ACP just
  as TUI and headless modes do.
- **Acceptance criteria:** Equivalent resume tests across ACP, TUI, REPL, and
  headless produce identical first provider requests.

### [x] AUD-043 — Cancelled or disconnected ACP approvals remain in the pending map

- **Confidence:** Source-proven
- **Area:** ACP, memory/lifecycle
- **Evidence:** [`acp.rs:55-110`](crates/medha-cli/src/acp.rs#L55),
  [`acp.rs:301-318`](crates/medha-cli/src/acp.rs#L301)
- **Failure:** Pending approval entries are removed only when an
  `approval.respond` arrives. Cancellation/shutdown does not drain or reject
  them, retaining senders and leaving approval futures tied to detached work.
- **Required fix:** Use an RAII pending-entry guard and drain all entries with a
  denial on cancellation, turn completion, EOF, and shutdown.
- **Acceptance criteria:** Pending count returns to zero for response, dropped
  future, cancel, protocol error, and disconnect.

### [x] AUD-044 — Agent follow-up can be silently lost during a settle race

- **Confidence:** Source-proven
- **Area:** Orchestrator, agent messaging
- **Evidence:** [`orchestrator/src/lib.rs:1397-1418`](crates/orchestrator/src/lib.rs#L1397),
  [`registry.rs:232-247`](crates/orchestrator/src/registry.rs#L232)
- **Failure:** `followup` checks `agent.is_running()`, then calls `steer` and
  ignores its boolean result. If the agent settles between the two operations,
  the method returns success although the message was never queued or used to
  resume the agent.
- **Required fix:** Make lookup-and-send atomic or, when `steer` returns false,
  re-resolve and enter the settled-agent resume path.
- **Acceptance criteria:** A barrier-controlled settle race delivers the
  follow-up exactly once or returns a visible error.

### [x] AUD-045 — Agent capacity is released before verification and settlement finish

- **Confidence:** Source-proven
- **Area:** Orchestrator, concurrency limits
- **Evidence:** [`orchestrator/src/lib.rs:1583-1782`](crates/orchestrator/src/lib.rs#L1583)
- **Failure:** The active-agent semaphore permit is dropped immediately after
  the child runner returns, before patch capture, verification, outbox delivery,
  worktree cleanup, and durable settlement. Many agents can exceed
  `max_active` in these expensive post-run phases.
- **Required fix:** Define the capacity lifetime as the complete admitted-agent
  lifecycle or add a separately bounded post-processing pool.
- **Acceptance criteria:** Instrumented stress tests never exceed configured
  concurrency across run, verification, patch, and settlement phases.

### [x] AUD-046 — Concurrent Medha instances can mark another live instance's agent abandoned

- **Confidence:** Source-proven
- **Area:** Agent outbox, multi-process recovery
- **Evidence:** [`medha-cli/src/agents.rs:512-587`](crates/medha-cli/src/agents.rs#L512)
- **Failure:** Recovery treats every open dispatch carrying a different instance
  ID as belonging to a dead process. Two live Medha processes sharing the same
  event log can produce an “abandoned” terminal result while the original child
  is still running and later reports a contradictory completion.
- **Required fix:** Store a process lease/heartbeat with liveness validation and
  claim recovery atomically before writing terminal state.
- **Acceptance criteria:** Two-process tests cannot create duplicate or
  contradictory terminal results for one dispatch.

### [x] AUD-047 — Atomic file replacement drops mode bits and filesystem metadata

- **Confidence:** Source-proven
- **Area:** Filesystem writes, correctness
- **Evidence:** [`sandbox/src/lib.rs:401-421`](crates/sandbox/src/lib.rs#L401)
- **Failure:** Medha writes a new default-mode sibling and renames it over the
  target without copying permissions or metadata. Editing a `0755` script usually
  turns it into `0644`; ACLs, xattrs, flags, and other metadata may also be lost.
- **Required fix:** Preserve required metadata from the original, define new-file
  mode semantics, and fsync the file and parent directory.
- **Acceptance criteria:** Tests cover executable files, read-only files, ACLs,
  xattrs where supported, new files, and crash durability.

### [x] AUD-048 — Malformed or unreadable `medha.lock` silently falls back to defaults

- **Confidence:** Source-proven
- **Area:** Configuration, security policy
- **Evidence:** [`lockfile/src/lib.rs:658-675`](crates/lockfile/src/lib.rs#L658)
- **Failure:** Missing, unreadable, and invalid TOML all become `None`, and
  `load_default` silently selects defaults. A typo can remove intended network
  deny, budgets, verification, sandbox, approval, or agent limits without
  failing startup.
- **Required fix:** Distinguish not-found from read/parse errors; fail startup for
  a present invalid lockfile and report the exact source location.
- **Acceptance criteria:** Missing remains optional; unreadable and malformed
  files return non-zero with actionable diagnostics and never run a task.

### [x] AUD-049 — Unix installer can skip checksum verification and trusts archive layout

- **Confidence:** Source-proven (checksum); Hardening (archive handling)
- **Area:** Installer, supply chain
- **Evidence:** [`install.sh:89-104`](install.sh#L89)
- **Failure:** Any checksum-download failure—including a transient or proxy
  failure for a published checksum—silently disables verification. A downloaded
  checksum is verified only when `shasum` exists, so common Linux systems with
  only `sha256sum` also skip it. As a separate hardening gap, the archive is
  extracted normally and the first executable named `medha` is selected without
  explicitly validating traversal, absolute paths, or link entries; actual
  exposure depends on the host `tar` implementation and configuration.
- **Required fix:** Support both checksum tools and fail if a published checksum
  cannot be verified. Validate archive entries before extraction and require one
  expected regular-file path.
- **Acceptance criteria:** Installer tests cover `sha256sum`-only hosts, missing
  verifier, mismatches, duplicate binaries, traversal, absolute paths, links,
  and special entries.

### [~] AUD-050 — PowerShell installer checksum and PATH handling are brittle

- **Confidence:** Source-proven
- **Area:** Windows installer, supply chain
- **Evidence:** [`install.ps1:43-55`](install.ps1#L43),
  [`install.ps1:65-70`](install.ps1#L65)
- **Failure:** Removing every non-hex character from the entire checksum file
  also retains hexadecimal letters from the filename, producing a value longer
  than the expected 64-digit digest and rejecting normal checksum-file formats.
  The optional-checksum path catches only `System.Net.WebException`, while
  PowerShell 7 can surface HTTP failures as `HttpResponseException`; a missing
  optional checksum can therefore abort installation. PATH detection uses
  substring wildcard matching and can mistake a similarly named directory for
  the install directory.
- **Required fix:** Parse the first whitespace-delimited 64-hex token and reject
  malformed or ambiguous records. Handle a precise not-found response across
  supported PowerShell versions while failing closed on other download errors,
  and compare PATH as normalized path segments.
- **Acceptance criteria:** Test GNU, BSD, bare-hash, CRLF, uppercase, malformed,
  and multiple-record checksum files; PowerShell 5/7 missing and transient
  checksum responses; and exact versus similarly named PATH entries.
- **Implementation (2026-07-31):** The installer now parses exactly one
  non-empty checksum record and one 64-hex digest, distinguishes a precise 404
  across Windows PowerShell and PowerShell 7, fails closed otherwise, validates
  ZIP entries, and compares normalized PATH segments. PowerShell regression
  fixtures cover the checksum, archive, and PATH cases. The marker remains
  `[~]` until that suite executes on a real current Windows runner.

### [x] AUD-051 — Synchronous SQLite work blocks asynchronous runtime workers

- **Confidence:** Source-proven
- **Area:** Store, memory, runtime responsiveness
- **Evidence:** [`store/src/lib.rs:199-246`](crates/store/src/lib.rs#L199),
  [`store/src/lib.rs:566-667`](crates/store/src/lib.rs#L566),
  [`memory/src/lib.rs:28-58`](crates/memory/src/lib.rs#L28)
- **Failure:** Async trait methods take `std::sync::Mutex<Connection>` and perform
  SQLite transactions synchronously. Busy waits, I/O latency, or contention can
  freeze a current-thread runtime and delay cancellation, streaming, and UI.
- **Required fix:** Use a dedicated database worker or `spawn_blocking` with
  async request serialization and bounded busy timeouts.
- **Acceptance criteria:** Slow/locked database tests do not block independent
  timers, cancellation, or provider streams.

### [x] AUD-052 — FTS search state is unauthenticated by event-log verification

- **Confidence:** Source-proven
- **Area:** Store, search integrity
- **Evidence:** [`store/src/lib.rs:229-238`](crates/store/src/lib.rs#L229),
  [`store/src/lib.rs:249-309`](crates/store/src/lib.rs#L249),
  [`store/src/lib.rs:409-450`](crates/store/src/lib.rs#L409)
- **Failure:** `verify()` checks only `events`, while search reads separate
  `events_fts`. FTS text, source, IDs, and snippets can be altered or corrupted
  while hash verification reports success.
- **Required fix:** Rebuild FTS only from verified events, or validate every hit
  against its authenticated source row.
- **Acceptance criteria:** FTS tampering is detected/repaired before results are
  returned and cannot inject arbitrary snippets.

### [x] AUD-053 — Replay deduplicates legitimate repeated user messages and can erase trust

- **Confidence:** Source-proven
- **Area:** Event replay, trust
- **Evidence:** [`events.rs:646-671`](crates/kernel/src/events.rs#L646),
  [`events.rs:791-810`](crates/kernel/src/events.rs#L791)
- **Failure:** Adjacent user messages are deduplicated solely by role and text.
  A user intentionally repeating a line loses one input on resume. Identical text
  arriving under different trust/provenance can also collapse into the stronger
  surviving representation.
- **Required fix:** Deduplicate only through explicit retry/event identity and
  carry trust in canonical user-message state.
- **Acceptance criteria:** Repeated text with same/different trust and retry
  markers replays exactly as originally admitted.

### [x] AUD-054 — Standalone compactor's protected-tail calculation is off by one

- **Confidence:** Source-proven
- **Area:** Context compactor API
- **Evidence:** [`compactor.rs:229-253`](crates/context/src/compactor.rs#L229),
  [`engine.rs:689-703`](crates/context/src/engine.rs#L689)
- **Failure:** The exported standalone helper checks the candidate before
  assigning it to `start`. If the last message alone satisfies the token target
  and `protect_last_n=1`, the returned tail can be empty and the newest message
  becomes compactable. The production pipeline already uses the correct order.
- **Required fix:** Assign the candidate before the threshold break and share one
  tested implementation with the production engine.
- **Acceptance criteria:** Property tests prove at least `protect_last_n`
  complete messages remain for all budgets and message sizes.
- **Resolution (2026-07-31):** One shared walk, `tail_start_index_by` in the
  compactor, is now the single implementation; the engine delegates to it and
  supplies only its per-message cost (full envelope incl. tool-call args),
  which is the one intentional difference. The candidate joins the tail before
  the threshold check in that single place, with saturating token accounting.
  The property sweep asserts the `protect_last_n` floor across lengths 1–24,
  protection 0–len+2, and budgets from below-single-message to `u32::MAX`.

### [x] AUD-055 — Absolute and traversal Gate checks can inspect host paths

- **Confidence:** Source-proven
- **Area:** Eval Gate scoring containment
- **Evidence:** [`checks.rs:75-95`](crates/gate/src/checks.rs#L75)
- **Failure:** `workspace.join(p)` preserves an absolute `p`, and a relative
  `../outside` path can also escape when `.exists()` reaches the OS. `exists`
  and `absent` checks can therefore inspect host paths rather than the throwaway
  workspace. This breaks hermetic scoring even when no command check is used.
- **Required fix:** Require normalized relative check paths and resolve them
  beneath the run workspace.
- **Acceptance criteria:** Absolute, prefix, and traversal paths fail scenario
  validation on Unix and Windows.

### [x] AUD-056 — Model-controlled `shell.exec.timeout_s` has no maximum

- **Confidence:** Source-proven
- **Area:** Shell tool, availability
- **Evidence:** [`tools/src/lib.rs:4131-4159`](crates/tools/src/lib.rs#L4131)
- **Failure:** The model can replace the 50-second foreground-promotion window
  with an arbitrarily large integer. A slow command can therefore occupy the
  tool call far longer than the documented self-management window unless the
  user cancels.
- **Required fix:** Add a schema/runtime maximum and separate “promotion delay”
  from a hard command lifetime.
- **Acceptance criteria:** Zero, overflow-sized, and above-maximum values are
  rejected or clamped and cancellation remains prompt.

### [x] AUD-066 — Settled-agent transcript lookup stops working after registry eviction

- **Confidence:** Source-proven
- **Area:** Orchestrator, agent transcripts, retention
- **Evidence:** [`registry.rs:74-76`](crates/orchestrator/src/registry.rs#L74),
  [`registry.rs:153-164`](crates/orchestrator/src/registry.rs#L153),
  [`orchestrator/src/lib.rs:1176-1186`](crates/orchestrator/src/lib.rs#L1176)
- **Failure:** The registry promises that settled transcripts remain readable,
  but it removes the complete agent entry after 32 newer agents settle.
  `transcript()` must first resolve that in-memory entry to discover the durable
  session ID, so the persisted transcript becomes unreachable through the
  advertised API even though its events still exist.
- **Required fix:** Retain a lightweight durable agent-path-to-session index, or
  resolve transcript identity directly from the durable dispatch/event history
  after roster eviction.
- **Acceptance criteria:** After hundreds of agents settle and after process
  restart, every retained transcript remains addressable by its documented
  stable identifier without keeping full live-agent state in memory.
- **Resolution (2026-07-31):** Eviction now moves the agent's path→session
  pair into a lightweight archive (two strings per agent, FIFO-capped at
  4096) instead of discarding it. `transcript()` falls back to
  `archived_session()`, which applies the same strictly-under-`from`
  containment as `reach` — a nested parent resolves its child by name or by
  the stable session id after any number of newer settlements, while a
  sibling that learns the id still cannot read it. Root surfaces keep their
  direct-ULID path, which also covers process restart.

---

## Low

### [x] AUD-057 — Concurrent identical permission requests can prompt twice

- **Confidence:** Source-proven
- **Area:** Permissions UX, concurrency
- **Evidence:** [`permissions/src/lib.rs:432-500`](crates/permissions/src/lib.rs#L432)
- **Failure:** Trust is checked before acquiring `prompt_mutex` and is not
  rechecked after acquiring it. Two same-path requests can queue; after the
  first receives “Always”, the second still prompts unnecessarily.
- **Required fix:** Recheck workspace/trust state under the serialized prompt
  guard before calling the human gate.
- **Acceptance criteria:** Concurrent same-path requests produce at most one
  prompt after a persistent grant.

### [x] AUD-058 — `shell.exec` can report `running` after the task already exited

- **Confidence:** Source-proven
- **Area:** Shell task status race
- **Evidence:** [`tools/src/lib.rs:4179-4206`](crates/tools/src/lib.rs#L4179)
- **Failure:** A task can exit after `wait_until` returns false but before
  `running_view`. The response unconditionally says `"status":"running"` even
  when the process has already completed.
- **Required fix:** Snapshot terminal state atomically when constructing the
  response and return exit code/output if completion won the race.
- **Acceptance criteria:** Barrier-controlled deadline/exit races never report a
  completed task as running.

### [x] AUD-059 — Gate wall-time grace arithmetic can overflow

- **Confidence:** Source-proven
- **Area:** Eval Gate input validation
- **Evidence:** [`run.rs:75-85`](crates/gate/src/run.rs#L75)
- **Failure:** `wall + 30` is unchecked. An extreme configured `u64` can overflow
  in debug builds or wrap in optimized builds, producing panic or an unexpectedly
  short deadline.
- **Required fix:** Validate practical wall limits and use checked/saturating
  duration arithmetic.
- **Acceptance criteria:** Boundary values cannot panic or shorten the requested
  deadline.

### [~] AUD-060 — Crashed Windows agent worktrees are never identified as stale

- **Confidence:** Source-proven
- **Area:** Windows, agents, worktree cleanup
- **Evidence:** [`worktree.rs:60-86`](crates/orchestrator/src/worktree.rs#L60)
- **Failure:** Non-Unix `owner_alive` returns `true` for every parsed foreign PID.
  Worktrees belonging to crashed processes are therefore retained indefinitely
  and their branches/directories accumulate.
- **Required fix:** Use a Windows process handle/liveness query or a lease with
  expiration and start-time/PID-reuse protection.
- **Acceptance criteria:** A killed Windows owner is reaped; a live or PID-reused
  owner is never removed.
- **Implementation (2026-07-31):** Windows ownership now records process
  creation time and checks it with `OpenProcess`, `GetProcessTimes`, and
  `WaitForSingleObject`, rejecting both dead owners and PID reuse. The
  Windows-only regression uses a directly sleeping PowerShell process so its
  kill cannot strand the former `ping.exe` helper, and it compiles under
  `x86_64-pc-windows-msvc`; `[~]` remains until it executes on a Windows runner.

### [x] AUD-061 — Security and autonomy documentation contradict implementation

- **Confidence:** Source-proven
- **Area:** Documentation, user expectations
- **Evidence:** [`WHAT_IS_MEDHA.md:284-307`](docs/WHAT_IS_MEDHA.md#L284),
  [`WHAT_IS_MEDHA.md:718-732`](docs/WHAT_IS_MEDHA.md#L718),
  [`policy/src/lib.rs:97-115`](crates/policy/src/lib.rs#L97),
  [`exec.rs:857-865`](crates/sandbox/src/exec.rs#L857)
- **Failure:** Documentation says sensitive reads are blocked and irreversible
  shell actions remain human-gated at every autonomy level, including yolo.
  Native sandboxes allow broad reads and yolo deliberately stops escalating
  safe-scanned shell.
- **Required fix:** Correct the documentation immediately and, preferably, make
  implementation meet the stronger stated safety contract.
- **Acceptance criteria:** Documentation tests or reviewed policy tables match
  every autonomy/backend decision and clearly state platform limitations.

### [x] AUD-062 — Several read-only tools still have unbounded input enumeration

- **Confidence:** Source-proven
- **Area:** Tools, resource hardening
- **Evidence:** [`sandbox/src/lib.rs:424-437`](crates/sandbox/src/lib.rs#L424),
  [`tools/src/lib.rs:1849-1888`](crates/tools/src/lib.rs#L1849)
- **Failure:** Directory listing can collect every entry, and word counting reads
  the complete file. Very large directories/files can cause avoidable latency
  and allocation even though output may later be bounded elsewhere.
- **Required fix:** Add entry/byte limits, pagination, streaming counts, and
  explicit truncation metadata.
- **Acceptance criteria:** Million-entry directories and multi-gigabyte files
  execute with bounded memory and predictable cancellation.
- **Resolution (2026-07-31):** `fs.list` retains at most 2 000 entries via
  `list_bounded`, which still counts the remainder and reports
  `truncated`/`total_entries` plus a hint toward `glob`. `word_count` no
  longer loads the file through `sbx.read()` (which had silently bypassed the
  fs.read whole-file cap); it streams the counts in one 64 KiB buffer with
  UTF-8 sequences carried across chunk boundaries, so a multi-gigabyte file
  costs one fixed buffer. Both run in `spawn_blocking`, so cancellation
  behaviour matches the other read-only tools.

### [x] AUD-063 — Gate check diagnostics hide invalid matches and containment (diagnostics only)

- **Confidence:** Source-proven
- **Area:** Eval Gate diagnostics
- **Evidence:** [`checks.rs:53-95`](crates/gate/src/checks.rs#L53)
- **Failure:** In addition to the scoring defects in AUD-040/AUD-055, returned
  details say only “no changes”, “found”, or “not found” and omit whether the
  pattern matched zero fixture files or resolved outside the workspace. This
  makes a vacuous pass difficult to notice during review.
- **Required fix:** Include match count, normalized relative target, and
  validation status in every check outcome.
- **Acceptance criteria:** Human-readable and machine-readable results expose
  zero-match and containment information.

### [x] AUD-064 — Dependency vulnerability status is unknown

- **Confidence:** Audit coverage gap
- **Area:** Supply chain
- **Evidence:** The audit environment did not have `cargo-audit` or `cargo-deny`;
  `cargo tree --workspace --duplicates` is not an advisory scan.
- **Failure:** No claim can currently be made about RustSec advisories, yanked
  crates, license policy, or vulnerable transitive versions.
- **Required fix:** Add pinned CI jobs for `cargo audit` and/or `cargo deny`, with
  an explicit reviewed allowlist and scheduled advisory refresh.
- **Acceptance criteria:** CI publishes a passing advisory/license/source report
  and fails on new unapproved findings.

---

## Found during the AUD-006 remediation review

### [x] AUD-067 — Seatbelt read allowlist omitted the root, so every native-sandboxed command aborted

- **Severity:** High
- **Confidence:** Reproduced (`sandbox-exec` exit 134 without the root grant;
  exit 0 with it)
- **Area:** Sandbox, macOS Seatbelt
- **Failure:** The deny-by-default read profile never granted `/` or ancestor
  traversal, and macOS path resolution stats every component, so children died
  with SIGABRT before `main`. The probe shared the defect, so selection fell
  back to the host backend: the AUD-006 remediation removed the working write
  jail and delivered nothing in exchange.
- **Resolution (2026-07-31):** `(allow file-read* (literal "/"))` plus
  `(allow file-read-metadata (vnode-type DIRECTORY) (vnode-type SYMLINK))` —
  stat/lstat and link resolution, not readdir or contents (`/var`, `/tmp`,
  `/etc` are symlinks on macOS). `/private/var/select` (sh interpreter
  indirection) and the developer-toolchain roots joined the readable set.
  Verified live: exec, `cd`, ancestor `stat` succeed; `ls $HOME`, dotfile and
  credential reads stay denied.

### [x] AUD-068 — The availability probe couldn't distinguish a broken profile from an unsupported platform

- **Severity:** Medium
- **Confidence:** Source-proven
- **Area:** Sandbox probing, test integrity
- **Failure:** The probe hand-copied the profile shape, inherited its defects,
  and reported them as platform limitations; the Seatbelt security tests gated
  on the same signal and passed vacuously — which is how AUD-067 shipped.
- **Resolution (2026-07-31):** Two probes with distinct meanings:
  `native_sandbox_supported` applies a permissive profile (a machine
  property), `native_backend_available` applies the real production profile
  builder — one source of policy. Both cached. Security tests gate on
  platform support; a guard test asserts supported ⇒ available, so a profile
  regression fails the suite instead of skipping it. The CLI warning now
  names a profile failure as a Medha bug rather than blaming the platform.

### [x] AUD-069 — Runtime permission grants never reached the exec sandbox

- **Severity:** Medium
- **Confidence:** Source-proven
- **Area:** Sandbox, permissions, agent UX coherence
- **Failure:** File tools honoured live approvals in-process while the exec
  backend froze its filesystem roots at startup, so the same user "yes"
  opened `fs.read` but left `shell.exec` denied on the same folder.
- **Resolution (2026-07-31):** `ApprovedRoots` is one live handle shared by
  the permission manager (which loads trust-file grants into it at startup
  and publishes `Always` approvals) and both native exec backends (which
  snapshot it inside every per-spawn profile build). A sandbox denial on an
  unapproved, non-credential path escalates to the standard approval card and
  retries path-scoped: `Always` persists, `Once` is granted for the single
  retry and withdrawn, `Deny` fails closed with one prompt. Credential paths
  never reach a card; MCP/LSP server spawns deliberately receive an empty
  handle.

---

## Found during CI and release re-audit

### [~] AUD-070 — Manual releases can publish default-branch bytes under a different tag and bypass platform CI

- **Severity:** High
- **Confidence:** Source-proven
- **Area:** GitHub Actions, release provenance, platform validation
- **Evidence:** [`.github/workflows/release.yml`](.github/workflows/release.yml),
  [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
- **Failure:** Manual dispatch accepted a tag used by the publishing step while
  build checkout still resolved the workflow's default ref. The release gate
  also covered Linux only, so Windows/macOS compile or test failures could be
  discovered after assets were being published. The result could be a release
  whose name, source bytes, and tested bytes were not one immutable identity.
- **Resolution (2026-08-01):** Release validation now checks out the exact tag,
  matches it to the workspace version, proves that it points at the checked-out
  SHA, and passes that immutable SHA to both reusable three-OS CI and every
  artifact build. Manual dispatch can dry-run any exact ref/SHA with publishing
  off by default; publishing additionally requires an existing version-matching
  tag. Publishing is the only job with `contents: write`; all jobs have explicit
  timeouts.
- **Acceptance criteria:** A current hosted run must show Linux, macOS, and
  Windows verification for the exact validated SHA; a mismatched/manual tag
  must fail before build or publish. Static `actionlint` is green, but that
  cannot exercise GitHub's reusable-workflow runtime, so this stays `[~]`.

### [~] AUD-071 — A sandbox unit test hard-codes `/bin/sh` and fails on Windows

- **Severity:** Low
- **Confidence:** Source-proven
- **Area:** Windows CI, sandbox test portability
- **Evidence:** [`crates/sandbox/src/exec.rs`](crates/sandbox/src/exec.rs)
- **Failure:** `host_backend_runs_and_captures` launched `/bin/sh` unconditionally.
  The product path was portable, but `cargo test --workspace --all-targets`
  could not complete on a normal Windows runner.
- **Resolution (2026-08-01):** The regression uses `/bin/sh` on Unix and
  `cmd.exe /D /S /C` on Windows, comparing normalized output. The Windows target
  now cross-checks cleanly and the macOS runtime test passes.
- **Acceptance criteria:** The test must execute successfully on a current real
  Windows runner; cross-compilation alone leaves this `[~]`.

### [x] AUD-072 — Unix installer's wget-only checksum-404 parser is malformed and untested

- **Severity:** Low
- **Confidence:** Reproduced by inspection and regression fixture
- **Area:** Unix installer, wget fallback
- **Evidence:** [`install.sh`](install.sh),
  [`crates/medha-cli/tests/unix_installer_e2e.rs`](crates/medha-cli/tests/unix_installer_e2e.rs)
- **Failure:** The awk expression used `^HTTP\\//`, so normal wget status lines
  such as `HTTP/1.1 404 Not Found` were not recognized reliably. Wget-only hosts
  could abort an otherwise valid install when an older release legitimately had
  no checksum, and the curl-only fixture left the fallback uncovered.
- **Resolution (2026-08-01):** The parser now matches `^HTTP\//`. A fake-wget
  end-to-end test proves that an exact 404 continues, a 503 fails closed, and a
  valid archive installs. Both Unix installer tests pass.

### [~] AUD-073 — Release builds pin a macOS runner image already in deprecation

- **Severity:** Low
- **Confidence:** Confirmed against the current official runner-image schedule
- **Area:** GitHub Actions, release continuity
- **Evidence:** [`.github/workflows/release.yml`](.github/workflows/release.yml),
  [GitHub Actions runner-images announcement](https://github.com/actions/runner-images/blob/main/images/macos/macos-14-Readme.md)
- **Failure:** Both Apple release targets used `macos-14`. GitHub began
  deprecating that image on 2026-07-06 and says it will be fully unsupported on
  2026-11-02, making future release failures inevitable if the label remains.
- **Resolution (2026-08-01):** Both Apple builders now use the supported,
  explicit `macos-15` label while retaining the separate aarch64 and x86_64
  Rust targets.
- **Acceptance criteria:** Both target builds must complete on a current hosted
  `macos-15` runner for the reconciled exact SHA. Until AUD-070's hosted run
  supplies that evidence, this remains `[~]`.

### [x] AUD-074 — Privileged release and CI workflows execute mutable third-party Action tags

- **Severity:** High
- **Confidence:** Source-proven
- **Area:** GitHub Actions, release supply chain
- **Evidence:** [`.github/workflows/release.yml`](.github/workflows/release.yml),
  [`.github/workflows/ci.yml`](.github/workflows/ci.yml),
  [`.github/workflows/security.yml`](.github/workflows/security.yml)
- **Failure:** Checkout, toolchain, cache, artifact, and release-publishing
  actions used mutable major tags or `master`. A compromised or moved upstream
  ref could change the code that builds artifacts; the release publisher runs
  with `contents: write`.
- **Resolution (2026-08-01):** Every external action is pinned to the exact
  40-hex commit resolved from its reviewed upstream ref. CI explicitly receives
  only `contents: read`, and the dependency-security workflow now runs for all
  workflow changes and rejects any future mutable external `uses:` reference.
  The guard and `actionlint` both pass locally.
- **Acceptance criteria:** No external workflow action uses a branch or tag,
  privileged write permission exists only on the publisher job, and a mutable
  reference makes the security job fail.

### [~] AUD-075 — Release packaging cannot be dry-run without publishing a tag

- **Severity:** Medium
- **Confidence:** Source-proven
- **Area:** GitHub Actions, release validation
- **Evidence:** [`.github/workflows/release.yml`](.github/workflows/release.yml)
- **Failure:** The former manual dispatch required an existing tag and always
  reached the write-permission publisher. There was no way to exercise the
  reusable exact-SHA CI, five target builds, packaging, and artifact upload for
  an untagged candidate without creating a public release.
- **Resolution (2026-08-01):** Manual dispatch now accepts any Git ref or SHA and
  a boolean `publish` input that defaults to false. A dry-run completes
  validation, three-OS CI, all target builds, packaging, and artifact upload,
  then skips the publisher. `publish: true` fails unless the ref is an existing
  semantic-version tag matching the workspace version and checked-out SHA.
- **Acceptance criteria:** A hosted dry-run of the pushed remediation SHA must
  build/upload every platform artifact and create no tag or release. This stays
  `[~]` until that run is observed.

## Consolidated remediation record

The original failure descriptions and acceptance criteria remain above. The
following map records where the regression evidence now lives for the items
whose headings are `[x]`; it supplements the item-specific resolution notes
without erasing the audit history.

| Surface | Audited IDs | Primary regression evidence |
|---|---|---|
| Gate containment, verdicts, cleanup, limits, and diagnostics | AUD-004–AUD-005, AUD-020–AUD-021, AUD-039–AUD-041, AUD-055, AUD-059, AUD-063 | `crates/gate/src/{checks,run,scenario,verdict,report}.rs` unit and process-tree tests |
| Filesystem, permissions, native isolation, context files, skills, shell, and web trust | AUD-001, AUD-006–AUD-010, AUD-017–AUD-019, AUD-022–AUD-024, AUD-032, AUD-034–AUD-035, AUD-047, AUD-056–AUD-058, AUD-061–AUD-062, AUD-067–AUD-069 | sandbox/permissions/tools/context unit tests plus memory-poisoning and permission-flow integration tests |
| Kernel durability, replay, budgets, cancellation, artifacts, memory, and compaction | AUD-002–AUD-003, AUD-011–AUD-016, AUD-030, AUD-033, AUD-036–AUD-038, AUD-051–AUD-054 | `mutation_order_e2e`, `policy_durability_e2e`, `request_accounting_e2e`, `runtime_limits_e2e`, replay and compaction suites |
| CLI, ACP, agents, worktrees, and lifecycle accounting | AUD-025–AUD-029, AUD-031, AUD-042–AUD-046, AUD-065–AUD-066 | ACP protocol/backpressure tests, headless-exit E2E, agent recovery/settlement tests, mutation-lease E2E, and Git descendant-reaping tests |
| Configuration, installers, supply chain, CI, and release | AUD-048–AUD-049, AUD-064, AUD-072, AUD-074 | strict lockfile tests, Unix installer E2E, pinned cargo-deny, immutable-action guard, scheduled security workflow, and actionlint |

## Post-remediation validation gates

No known code fix remains open. The audit becomes fully closed only after:

1. **AUD-070, AUD-073, AUD-075:** Push the remediation commit and dispatch a
   `publish: false` release dry-run for its exact SHA. Observe reusable
   Linux/macOS/Windows CI, all five target builds (including both `macos-15`
   targets), artifact upload, and a skipped publisher.
2. **AUD-050, AUD-060, AUD-071:** Run
   `cargo test --workspace --all-targets` on a real Windows runner and confirm
   the PowerShell installer, Win32 owner-liveness, and portable host-backend
   regressions execute rather than merely cross-compile.

Repository handoff note (2026-08-01): the maintainer designated GitHub `main`
at `80ac7a4` (`v0.1.5`) as the authoritative base and explicitly asked that the
backup branch be ignored. The complete remediation is staged as the next commit
on that base. No reset, force-push, release publication, or tag was performed by
the audit pass.

## Closure rules

An item should be marked fixed only when:

1. The production failure path has been removed rather than hidden by a retry.
2. A deterministic regression test covers the original trigger.
3. The test includes cancellation/error behavior where relevant.
4. Platform-specific behavior is tested or explicitly gated.
5. Documentation and configuration examples match the new behavior.
6. `cargo test --workspace --all-targets`, Clippy with warnings denied, and
   formatting checks pass.
