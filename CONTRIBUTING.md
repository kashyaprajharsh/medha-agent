# Contributing to Medha

Thank you for helping improve Medha. We welcome bug reports, documentation,
accessibility improvements, tests, and code contributions. Small, focused
contributions are a good place to start.

Participation in this project is covered by our [Code of Conduct](CODE_OF_CONDUCT.md).
For suspected vulnerabilities, follow the private reporting instructions in our
[security policy](SECURITY.md).

For Rust contributions, follow the [Rust contribution guide](RUST_GUIDELINES.md).
It combines the writing, review, and porting standards used to assess changes.

## Choose where to start

- Read the [README](README.md) for installation and usage, and the
  [technical overview](docs/WHAT_IS_MEDHA.md) for architecture and runtime behavior.
- Search [existing issues](https://github.com/kashyaprajharsh/medha-agent/issues)
  and [pull requests](https://github.com/kashyaprajharsh/medha-agent/pulls) before
  starting work. If you plan to take an existing issue, leave a comment so others
  can coordinate with you.
- Use the [issue forms](https://github.com/kashyaprajharsh/medha-agent/issues/new/choose)
  for bugs, feature requests, and documentation problems. Use
  [Discussions](https://github.com/kashyaprajharsh/medha-agent/discussions) for
  setup questions, troubleshooting, and early ideas.
- Open an issue before a substantial architectural change, a new dependency,
  a public interface change, or a change to permissions or persistent data.
  Explain the problem and proposed approach. Small fixes can go directly to a
  pull request.

## Set up a development environment

### Rust workspace

Install [Git](https://git-scm.com/) and Rust through
[rustup](https://rustup.rs/). The repository's
[rust-toolchain.toml](rust-toolchain.toml) selects the development toolchain and
its formatting and linting components. The minimum supported Rust version is
declared separately in [Cargo.toml](Cargo.toml) and checked in CI.

Fork the repository on GitHub, then clone your fork and create a branch:

```sh
git clone https://github.com/YOUR-USERNAME/medha-agent.git
cd medha-agent
git remote add upstream https://github.com/kashyaprajharsh/medha-agent.git
git switch -c fix/describe-the-change
cargo build -p medha-cli --bin medha --locked
```

On Linux, the workspace also needs D-Bus development headers and `pkg-config`.
For Debian or Ubuntu:

```sh
sudo apt-get update
sudo apt-get install -y libdbus-1-dev pkg-config
```

Run the CLI built from your checkout with:

```sh
cargo run -p medha-cli --bin medha --
```

Launching an agent session requires a configured model. Follow the
[model setup instructions](README.md#connect-a-model); automated tests should
use fixtures or mock providers instead of paid model calls.

### Desktop app

Desktop development additionally requires Node.js 24, npm, and the native
prerequisites for Tauri 2 on your operating system. Follow the
[desktop development guide](apps/desktop/README.md#development) for setup,
launching the app, and platform dependencies.

The desktop shell has a separate Cargo manifest and lockfile. Workspace checks
at the repository root do not cover that manifest.

## Find the right part of the repository

| Area | Entry points |
| --- | --- |
| Agent loop, events, and verification | [`kernel/src/loop_.rs`](crates/kernel/src/loop_.rs), [`events.rs`](crates/kernel/src/events.rs), [`verify.rs`](crates/kernel/src/verify.rs) |
| Shared session runtime and configuration | [`runtime/src/session.rs`](crates/runtime/src/session.rs), [`config.rs`](crates/runtime/src/config.rs) |
| Backend startup and chat creation | [`medha-cli/src/serve.rs`](crates/medha-cli/src/serve.rs), [`serve_chats.rs`](crates/medha-cli/src/serve_chats.rs) |
| Chat multiplexing and attached clients | [`backend/src/lib.rs`](crates/backend/src/lib.rs) |
| Shared backend client and process lifecycle | [`medha-client/src/lib.rs`](crates/medha-client/src/lib.rs) |
| Application commands, presentation events, and authenticated local transport | [`protocol/src/lib.rs`](crates/protocol/src/lib.rs), [`wire/src/lib.rs`](crates/wire/src/lib.rs) |
| CLI, terminal UI, and editor ACP adapter | [`medha-cli/src/main.rs`](crates/medha-cli/src/main.rs), [`tui_tea/`](crates/medha-cli/src/tui_tea/), [`editor/mod.rs`](crates/medha-cli/src/editor/mod.rs) |
| Child-agent sessions, capability narrowing, and writer worktrees | [`orchestrator/src/lib.rs`](crates/orchestrator/src/lib.rs), [`runtime/src/agents.rs`](crates/runtime/src/agents.rs), [`tools/src/agents.rs`](crates/tools/src/agents.rs) |
| Tool registry, MCP host, and language servers | [`tools/src/lib.rs`](crates/tools/src/lib.rs), [`mcp/src/lib.rs`](crates/mcp/src/lib.rs), [`lsp/src/lib.rs`](crates/lsp/src/lib.rs) |
| Model providers and context management | [`providers/src/lib.rs`](crates/providers/src/lib.rs), [`context/src/lib.rs`](crates/context/src/lib.rs) |
| Tool policy, file permissions, and shell-command parsing | [`policy/src/lib.rs`](crates/policy/src/lib.rs), [`permissions/src/lib.rs`](crates/permissions/src/lib.rs), [`shell-reader/src/lib.rs`](crates/shell-reader/src/lib.rs) |
| Filesystem operations and execution backends | [`sandbox/src/lib.rs`](crates/sandbox/src/lib.rs), [`exec.rs`](crates/sandbox/src/exec.rs) |
| Event store, artifacts, and memory | [`store/src/lib.rs`](crates/store/src/lib.rs), [`memory/src/lib.rs`](crates/memory/src/lib.rs) |
| Messaging gateway, Telegram, and pairing | [`gateway/src/lib.rs`](crates/gateway/src/lib.rs), [`telegram.rs`](crates/gateway/src/telegram.rs), [`pairing.rs`](crates/gateway/src/pairing.rs), [`runtime/src/messaging.rs`](crates/runtime/src/messaging.rs) |
| Desktop frontend and native shell | [`apps/desktop/src/`](apps/desktop/src/), [`src-tauri/src/main.rs`](apps/desktop/src-tauri/src/main.rs) |
| Agent behavior scenarios and evaluation harness | [`scenarios/`](scenarios/), [`gate/src/lib.rs`](crates/gate/src/lib.rs) |
| Public architecture documentation | [`docs/WHAT_IS_MEDHA.md`](docs/WHAT_IS_MEDHA.md) |
| Continuous integration and dependency checks | [`ci.yml`](.github/workflows/ci.yml), [`security.yml`](.github/workflows/security.yml) |

The backend spans several crates: the CLI starts the service, `backend` manages
attached chats and clients, `medha-client` connects frontends, `protocol` defines
application messages, and `wire` authenticates and frames the local connection.

## Make a focused change

- Follow the surrounding code's conventions. Keep responsibilities clear and
  prefer small modules and functions; workspace Clippy lints are enforced in CI.
- Keep unrelated refactors, formatting, generated files, and dependency upgrades
  out of the change. Explain any new dependency and update the relevant lockfile
  when the dependency graph changes.
- Preserve permission boundaries, credential handling, event history, and
  compatibility with existing sessions. Explain changes to those contracts in
  the pull request, including migration or recovery behavior where needed.
- For a bug fix, add a regression test when it can capture the failure. For new
  behavior, cover the relevant success and failure paths. Prefer one end-to-end
  test through the real binary, such as those in
  [`serve_e2e.rs`](crates/medha-cli/tests/serve_e2e.rs); add a unit test only for
  a security edge case or operating-system behavior that no higher path can
  reach. Documentation and presentation-only edits need checks appropriate to
  their scope.
- Keep automated tests deterministic. Prefer local fixtures, temporary
  directories, and mock services. Make any external-service or platform
  requirement explicit.
- Update user-facing documentation when commands, configuration, permissions, or
  visible behavior change.

AI-assisted contributions are welcome. You are responsible for understanding the
change, checking its sources and licensing, and verifying its behavior. Review
generated code and explanations with the same care as any other contribution.

## Checks before opening a pull request

Run the checks that apply to your change before requesting review. The commands
below match the project's CI workflows. Record their results in the pull request;
for any check you did not run, state why. For documentation-only changes, use the
[documentation checks](#documentation-checks).

Unless a section says otherwise, run commands from the repository root.

### Rust formatting, linting, and tests

For changes to the Rust workspace:

```sh
# Check formatting without modifying files.
cargo fmt --all --check

# Lint every workspace target; warnings fail the check.
cargo clippy --workspace --all-targets -- -D warnings

# Run the workspace tests and report every failing test, as CI does.
# Install the runner once with `brew install cargo-nextest` or
# `cargo install cargo-nextest --locked`; settings are in .config/nextest.toml.
cargo nextest run --workspace --all-targets --profile ci

# Without nextest, this runs the same tests, one test binary at a time.
cargo test --workspace --all-targets --no-fail-fast

# Run the CI and release helper tests.
python3 -m unittest discover -s .github/scripts -p 'test_*.py'

# Check the diff for whitespace errors and conflict markers.
git diff --check
```

To fix formatting, run `cargo fmt --all`, review the diff, and rerun the check.
Address Clippy warnings and failing tests before requesting review; report any
pre-existing failure separately with the command and error that demonstrate it.

While developing, use a focused test run. Replace `kernel` with the package you
changed and `test_name` with the test's name:

```sh
# List available test cases in a package.
cargo test -p kernel -- --list

# Run one test or a group matching its name.
cargo test -p kernel test_name

# Run all tests in the affected package.
cargo test -p kernel
```

A focused test run helps during development; run the workspace checks above
before requesting review of Rust changes. For a bug fix, add a test that
reproduces the failure before the fix when feasible. For new behavior, test both
the expected result and relevant failure paths.

CI also checks the minimum supported Rust version on Linux. To reproduce that
check, install the additional toolchain once and run:

```sh
rustup toolchain install 1.89 --profile minimal
cargo +1.89 check --workspace --all-targets --locked
```

Continue using the toolchain pinned in `rust-toolchain.toml` for formatting and
Clippy. [CI](.github/workflows/ci.yml) runs tests on Linux, macOS, and Windows.
Some integration tests need a browser, installed language servers, or native
sandbox support. Check the workflow for the corresponding setup and report any
skipped coverage. A check passing on one platform does not establish behavior on
the others.

### Desktop build, type checking, and tests

From the repository root, check desktop frontend changes with:

```sh
npm --prefix apps/desktop ci
npm --prefix apps/desktop run build
npm --prefix apps/desktop test
```

`npm run build` runs TypeScript's project check followed by the Vite build. The
desktop scripts are defined in [package.json](apps/desktop/package.json).

For native shell or desktop/backend integration changes, run the frontend checks
above, then these commands, also from the repository root:

```sh
npm --prefix apps/desktop run prepare:backend
cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --locked --no-fail-fast
```

The frontend build and backend preparation must precede the native shell checks.
To fix native shell formatting, run
`cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml`, review the diff,
and rerun its formatting check.

On macOS, run the real WebKit artifact rendering checks for artifact changes:

```sh
npm --prefix apps/desktop run test:artifacts:webkit
```

See the [desktop checks](apps/desktop/README.md#tests-and-checks) for details.

For UI changes, exercise the affected flow in an app or terminal built from your
branch. Include screenshots or a short recording when they help reviewers.
Keyboard focus, physical paste, approvals, and terminal behavior should be
checked through the actual interaction as relevant to the change.

### Agent behavior and scenarios

Validate scenario definitions without making model requests:

```sh
cargo run -p medha-cli --bin medha -- gate scenarios/ --validate
```

For changes to agent behavior, also run the relevant scenarios against a
configured model. To run the full suite:

```sh
cargo run -p medha-cli --bin medha -- gate scenarios/
```

Scenario execution makes real model requests and may incur provider charges.
Record the model, provider, relevant configuration, and results. State clearly
when you only validated definitions or could not run a model evaluation.

### Dependency security checks

For Rust dependency or dependency-policy changes, use the same `cargo-deny`
version as the [dependency security workflow](.github/workflows/security.yml).
Install it once, then check both Rust dependency graphs from the repository root:

```sh
cargo install cargo-deny --version 0.20.2 --locked

cargo deny --all-features check advisories
cargo deny --all-features check licenses bans sources

cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml --config deny.toml --all-features check advisories
cargo deny --manifest-path apps/desktop/src-tauri/Cargo.toml --config deny.toml --all-features check licenses bans sources
```

For desktop npm dependency changes:

```sh
npm --prefix apps/desktop ci
npm --prefix apps/desktop audit --omit=dev --audit-level=moderate
```

Include changes to the relevant lockfiles. For workflow changes, GitHub Action
references must be pinned to immutable 40-character commit SHAs; CI checks this
along with the dependency policy.

### Documentation checks

For documentation-only changes, check Markdown formatting, relative links, and
documented paths and commands. Validate any changed YAML against its intended
schema. A full application build is generally unnecessary for prose-only edits.

```sh
git diff --check
```

### Final review checklist

- [ ] The applicable formatting, linting, and test commands above pass.
- [ ] Bug fixes or new behavior have appropriate test coverage.
- [ ] Desktop changes have passed the frontend checks and, when applicable, the
  separate native shell checks and a manual check of the affected flow.
- [ ] Scenario changes have been validated; model evaluation results or their
  limitations are recorded when agent behavior changes.
- [ ] Dependency changes pass the applicable security checks and include updated
  lockfiles.
- [ ] Documentation reflects the change, and `git diff --check` passes.
- [ ] The pull request records exact commands, results, and any checks not run.

## Open a pull request

1. Target `main` and keep the pull request focused on one problem. Use a draft
   pull request if the approach or implementation is still in progress.
2. Write a clear title that describes the resulting change. Use the pull request
   template to explain the problem, the solution, and how you verified it.
3. Link related issues. Use `Closes #123` only when the change resolves that issue.
   An issue is not required for a small, self-contained fix.
4. Describe compatibility, persistent-data, security, or platform implications
   when applicable. Separate observed results from expected behavior and
   untested assumptions.
5. Review your own diff, remove accidental files and secrets, and respond to
   review comments. Keep verification results current after substantive changes.

Once the applicable checks pass, commit and push your branch. Stage only files
that belong to your change; replace the example paths and message below:

```sh
git status --short
git diff --check
git add path/to/changed-file path/to/another-changed-file
git diff --cached
git commit -m "Describe the change"
git push -u origin HEAD
```

Open a pull request against `main` on GitHub and fill out the template. If you
use the optional [GitHub CLI](https://cli.github.com/), you can create the pull
request and inspect its CI results with:

```sh
gh pr create --repo kashyaprajharsh/medha-agent --base main --template .github/PULL_REQUEST_TEMPLATE.md
gh pr checks <number> --repo kashyaprajharsh/medha-agent
```

Use a draft PR while work or required checks are still in progress. Before
requesting final review, inspect the CI results and resolve failures caused by
your change.

Maintainers may request changes, suggest a smaller scope, or close a proposal
that does not fit the project. Review timing depends on maintainer availability.
There is no required commit-message format; use concise, descriptive messages.
Releases and version changes are coordinated by maintainers.

## Licensing

Medha is licensed under [Apache-2.0](LICENSE). Contributions submitted for
inclusion are provided under that license unless explicitly stated otherwise.
Only submit material you have the right to contribute, and preserve applicable
copyright and attribution notices.
