# Medha Desktop

Medha Desktop is the native desktop app for Medha. It brings agent conversations,
files, tool activity, approvals and a terminal into one window, with support for
local models and hosted providers.

![Medha Desktop](../../docs/assets/medha-desktop.gif)

[Install](../../README.md#desktop-app) · [Develop](#development) ·
[Build](#build) · [Technical overview](../../docs/WHAT_IS_MEDHA.md)

## Features

- **Conversations:** start, resume, search and rewind chats; follow streaming
  answers, reasoning, tool steps and usage.
- **Model controls:** choose the model, reasoning effort, autonomy mode and
  streaming in the composer. Configure providers, custom endpoints, API keys and
  defaults in Settings.
- **Projects and Personal chats:** work in a project folder or give each Personal
  chat its own managed folder for generated files.
- **Files and changes:** preview Markdown, code, images, PDFs, DOCX and
  spreadsheets; inspect session edits, Git diffs and pending agent patches.
- **Approvals and questions:** review requests and choose the access or answer
  you want to give.
- **Agents:** inspect transcripts, send steering or follow-up instructions, and
  stop individual agents.
- **Extensions:** install and manage skills, plugins and MCP servers, including
  their enabled state and tool access.
- **Terminal:** use your shell through a native PTY, with interactive programs,
  resizing, Unicode and Ctrl+C. Hiding the terminal preserves running commands.
- **Images:** attach, paste or drop images into the composer.

## Get started

1. Install the app using the [desktop installation instructions](../../README.md#desktop-app).
2. Open **Settings** to configure a local model or hosted provider.
3. Start a Personal chat, or use the folder menu to open a project.

The desktop package includes the Medha backend. Desktop and the terminal app can
be installed separately.

Personal folders are created when you start their chats. Opening a project shows
that folder's existing Medha history. Switching folders keeps running chats and
terminal tabs attached to their original workspace; changing directory inside a
terminal does not change the agent's workspace.

Rewind can fork a conversation, restore recorded workspace edits, or do both.
Arbitrary shell commands and external side effects are outside its undo scope.

## Shared backend and data

Desktop, the terminal app and editor ACP connect to one authenticated local
backend per Medha home. They use the shared `medha-client` library and typed
`medha-protocol` contract. The apps start or join the backend automatically.

The default home is `~/.medha`; `MEDHA_HOME` selects a different one. Apps using
the same home and workspace can follow the same live conversation. Closing one
viewer leaves other viewers attached. Global models, credentials and preferences
are shared, while project history and state are managed by the backend.

API keys stay in Medha's credential store. Settings receives key-presence
information rather than stored secret values. Remembered file and folder grants
apply to future chats in that project and include command access; **Allow once**
does not persist a grant.

See the [technical overview](../../docs/WHAT_IS_MEDHA.md) for runtime, storage and
permission details.

## Keyboard shortcuts

| Action | macOS | Windows / Linux |
|---|---|---|
| Toggle sidebar | `⌘B` | `Ctrl+B` |
| Toggle work panel | `⌘.` | `Ctrl+.` |
| Toggle terminal | `Ctrl+Backtick` | `Ctrl+Backtick` |
| New chat | `⌘N` | `Ctrl+N` |
| Search and commands | `⌘K` | `Ctrl+K` |

Slash commands such as `/model`, `/reasoning`, `/mode`, `/agents`, `/changes`,
`/files`, `/context`, `/settings`, `/plugins` and `/rewind` open the corresponding
controls. `/clear` and `/new` start a fresh chat and retain conversation history.

## Development

The app uses React and TypeScript with Tauri 2. Install:

- The Rust toolchain pinned in [rust-toolchain.toml](../../rust-toolchain.toml).
- Node.js 24 and npm, matching the desktop CI environment.
- The [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your
  operating system. Linux also needs D-Bus development headers; the
  [CI workflow](../../.github/workflows/ci.yml) lists the packages used by the project.

From the repository root:

```sh
cd apps/desktop
npm ci
npm run tauri dev
```

This prepares the backend, starts the Vite development server and opens the
native app. Set `MEDHA_DESKTOP_WORKSPACE=/absolute/path/to/project` to choose a
project during development.

## Build

From `apps/desktop`:

```sh
npm run tauri build
```

The build prepares a release backend and builds the frontend automatically.
Native packages are written under `src-tauri/target/release/bundle/`; the bundled
backend is built for the same target as the desktop app.

For an app-only build on macOS:

```sh
npm run tauri build -- --bundles app
```

Open the resulting `Medha.app`, or run `npm run link:command` to link its
`medha-desktop` executable into `~/.local/bin`. The linking script refuses to
replace an existing command. With that directory on your `PATH`, open a project
with:

```sh
medha-desktop /absolute/path/to/project
```

## Tests and checks

From `apps/desktop`:

```sh
npm run build
npm test
npm run prepare:backend
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

Preparing the backend supplies the executable used by the desktop integration
tests. [CI](../../.github/workflows/ci.yml) runs desktop checks on macOS, Linux and
Windows. Installed-app verification should also exercise starting and resuming
chats, approvals, extensions and terminal interactions.

## More documentation

- [Medha installation and usage](../../README.md)
- [Technical overview](../../docs/WHAT_IS_MEDHA.md)
- [Release packaging](../../.github/workflows/release.yml)
