# Medha Desktop

The Tauri app uses `work/desktop-concept.html` as its visual reference. Desktop
and TUI share the kernel, providers, tools, policies, orchestrator, configuration,
credentials, memory and event store. A typed local stdio bridge adapts that
runtime to the desktop; there is no second agent backend.

Launching the installed app opens Personal chats. Each Personal chat has its own
managed folder for generated files. `medha-desktop /path/to/project` opens that
project and its existing Medha history. Use the folder menu to open or switch
projects in the same window. Running chats and terminal tabs remain attached to
their original workspace. Global models, API keys and search preferences use the
same `~/.medha` configuration as the TUI.

The composer selects a model, thinking/effort, autonomy mode and streaming.
Settings adds, edits, removes and defaults models, including the TUI's local
provider presets and custom endpoints. Model discovery uses the shared provider
adapter. Context/output limits and advanced deployment settings are available
on the model form. Adding a model first asks for a supported protocol, then a
compatible provider. Connection and model fields appear after that selection.
Provider presets fill the endpoint and authentication defaults; custom endpoints
start empty. Changing the connection discards stale discovery results. Protocol
values and availability come from the same picker table as the TUI.
Keys stay in Medha's existing credential store and are never
returned to the webview. Web search, instructions and appearance live in Settings.

Details open on demand: readable tool steps, clickable filenames, grouped work,
agent transcripts, approvals, questions and usage. Live agent transcripts refresh
every two seconds and support steering, stopping and follow-up instructions.
Files preview Markdown, code, images, PDF pages, DOCX reading views and XLSX/CSV
sheets. Changes shows each edit in the current session, newest first, with a
whole-session view. Write/edit steps can jump to that file in Changes. Git changes
shows actual staged, unstaged and untracked diffs, including repositories one
folder below the workspace. Pending
agent patches have a separate review view and use the existing merge checks.

Extensions manages skills, plugins and MCP servers, including reviewed plugin
access, installation, enable/disable, updates and previous versions. Tool access
lists the current runtime's tools. Removing a configured MCP server disconnects
it from idle chats when they reload; active chats reload after work finishes.
Connecting a local MCP server explicitly reviews its executable before starting it.

Use **⌘B** for the sidebar, **⌘.** for the work panel and **⌃`** for the terminal.
Collapsed sessions slide into view on hover; the same toggle keeps them open.
Reading size and theme are remembered. `/model`, `/reasoning`, `/mode`, `/agents`,
`/changes`, `/files`, `/context`, `/settings`, `/plugins`, `/rewind`, `/clear` and
`/new` open the desktop controls. Clear starts a fresh chat and keeps history.
Rewind forks the conversation, restores recorded workspace edits, or does both.
It does not undo arbitrary shell commands or external side effects.

The terminal uses a native PTY and your actual login shell, with interactive
programs, history, Ctrl+C, Unicode and resizing. Hiding preserves running commands;
closing a tab or the app stops its shell. New tabs use the selected chat/project
folder. Shell `cd` is independent of the agent workspace. Up to four tabs are
shown per workspace, bounded to sixteen across the window.

Images can be attached, pasted or dropped on the composer. Source images up to
64 MiB use the shared media decoder and are normalized to the bridge's 2 MiB
per-image transport budget. This removes the old 2 MiB source-file restriction.
Admission errors and resizing notes appear in the composer.

## Develop

```sh
cd apps/desktop
npm ci
npm run tauri dev
```

Set `MEDHA_DESKTOP_WORKSPACE=/path/to/project` to choose a project during
development. The frontend never launches arbitrary commands or reads credentials.

## Build and launch

```sh
npm run tauri build -- --bundles app
```

The native executable is `medha-desktop`. On macOS, `npm run link:command` links
the built executable into `~/.local/bin`, refusing to replace an existing command.
The built `Medha.app` also opens normally. The bundle includes the `medha` sidecar;
the app starts it as `medha serve`, which is internal, not a user-facing command.

Preview bounds are 2 MiB for text and 24 MiB for binary files. Unsupported or
larger files can open in their system app. DOCX previews prioritize reading;
they do not reproduce Word's exact page layout. Sheets show 100 rows at a time
and up to 100 columns.
