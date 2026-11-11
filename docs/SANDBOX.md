# The Medha sandbox

Medha lets a model run commands on your machine: tests, builds, scripts, Git. The sandbox decides what
those commands can touch. This page explains what is protected, how each option works, and how to set
it up for your project.

For the internals (Landlock rules, Seatbelt profiles, Git control mounts), see
[Sandbox (Jails)](WHAT_IS_MEDHA.md#sandbox-jails) in the architecture guide.

## What runs inside it

The sandbox covers every program Medha starts. Work Medha does itself is guarded by its own rules
instead.

| Action | Where it runs |
|---|---|
| Shell commands (`shell.exec`), background tasks, terminal programs | Sandbox |
| The `[verify]` command, such as `cargo check` | Sandbox |
| Local MCP servers (started as a program on your machine) | Sandbox |
| Language servers | Sandbox |
| Plugin hooks and snippets | Sandbox, with their own narrower scope |
| File tools: `read`, `write`, `edit`, `grep`, `glob` | Inside Medha, behind its path checks |
| Web search and fetch, model calls | Inside Medha |
| Remote MCP servers | On that server, not on your machine |
| A command that needs your keys (`git push`, `ssh`, `docker`) | Outside the sandbox, once, after you approve that run |

Keys and tokens (`~/.ssh`, `~/.aws`, `~/.gitconfig` and similar) are closed to file tools and
commands alike. Approving a folder never opens them.

## What a sandboxed command can do

With the default settings on macOS and Linux:

- **Write** only inside the project, a private HOME, a private temp folder, and folders you approved.
- **Read** the project, system tools (`/usr`, `/bin` and so on) and the folders you approved. Your real
  home folder is closed.
- **No network.** A command that needs it asks you first.
- **No changes to Git's control files.** `.git/config` and `.git/hooks` are read-only, because Git
  later runs what they name outside any sandbox. `git add` and `git commit` still work.
- **No keys in the environment.** Commands start from a clean environment, so an API key Medha holds
  never reaches them.

The private HOME has its own `.cargo`, `.npm`, `.cache` and so on. Your installed toolchains, such
as `~/.cargo/registry` and `~/.rustup/toolchains`, are linked in read-only, so builds work without
access to the rest of your home folder.

When a command is refused a folder, Medha shows which one and asks. It never widens access without
asking you.

## Choosing a backend

Set it in `medha.lock` at the project root:

```toml
[sandbox]
backend = "native"   # native, container, ssh or host
network = "deny"     # or "allow"
```

| Backend | What it does | Choose it when |
|---|---|---|
| `native` (default) | Your OS's own jail: Seatbelt on macOS, Landlock on Linux | Almost always. Nothing to install |
| `container` | Each command in a fresh Docker or Podman container, removed afterwards | You want the strongest isolation, or the project needs a fixed toolset |
| `ssh` | Each command on another machine over SSH | The code and tools live on a remote box |
| `host` | No jail. Commands run with your own rights | You accept the risk, for example in a throwaway VM |

### How each system behaves

| System | `native` gives you | If the jail cannot start |
|---|---|---|
| macOS | Seatbelt jail | Medha checks once at start. If `sandbox-exec` fails, commands run without a jail |
| Linux | Landlock for files, seccomp for network, a private mount namespace for Git controls | Commands are refused. Medha never runs them unprotected |
| Windows | No OS jail yet. Medha snapshots Git's control files before each command and puts them back afterwards | Not applicable |

On Windows, and with `host`, Medha treats commands as unconfined. After the model has read web
content, it asks before running a command rather than trusting the jail to stop it sending data out.

## Network

`network = "deny"` (the default) blocks all network access from commands. When a command needs it,
for example `npm install`, Medha asks, and you choose:

- **Once:** for this one command.
- **This session:** until Medha exits.
- **Always:** remembered for this project on this machine.
- **Deny.**

Use `network = "allow"` only when the project's commands always need downloads. Medha's own model
and web calls are not affected by this setting.

## Extra folders

To let commands write to a folder outside the project, such as a shared build cache, list it in
`medha.lock`:

```toml
[sandbox]
extra_writable = ["/Users/you/shared-build"]
```

Paths must be absolute and must not contain `..`. You can also approve a folder when Medha asks;
that approval applies only to the folder you approved.

## Setting up containers

Medha does not install Docker or download images. It runs your `docker` (or `podman`) program, so
whatever works in your terminal works for Medha.

1. **Install a runtime:** Docker Desktop, OrbStack, Podman or Docker Engine. Check that `docker ps`
   (or `podman ps`) works in your terminal.
2. **Pull an image** that has every tool your commands use:

   ```sh
   docker pull python:3.12-slim
   ```

   Medha starts containers with `--pull never`, so it never fetches an image on its own. A missing
   image makes the command fail with the runtime's error.
3. **Point `medha.lock` at it:**

   ```toml
   [sandbox]
   backend = "container"
   image   = "python:3.12-slim"
   # runtime = "podman"   # optional; Medha tries docker, then podman
   # memory  = "2g"       # optional limits
   # pids    = 512
   ```

### What happens on each command

```
Medha, outside the container:
  docker create --pull never
      the project mounted at /workspace, working directory /workspace
      all capabilities dropped, no new privileges
      no network, unless you allowed it
      .git/config and .git/hooks mounted read-only
      a private HOME and /tmp
      your image, running your command
  docker start, wait, collect the output
  remove the container
```

The command inside cannot reach the Docker socket, so it cannot start other containers or reach your
machine through Docker. Medha's keys are not passed in.

Tips:

- One image must hold every tool the commands need. `rust:1` has no Python, and `python:3.12-slim`
  has no Rust. A small Dockerfile on top of an official image is usually the simplest way to get one
  image with everything in it.
- Open Medha at the repository root. A subfolder cannot safely mount its parent's Git data.
- On Linux, Medha runs the container as your user so files it writes stay yours. Set `user` in
  `[sandbox]` only if you need a different one.

### Stricter cleanup

On macOS and Linux, Medha stops a command's whole process group when it ends. A program that
deliberately escapes its group can outlive it. If that matters, require containers:

```toml
[sandbox]
strict_cleanup = true
backend = "container"
image = "your-image"
```

With `strict_cleanup` set and no working container setup, commands are refused rather than run with
weaker cleanup.

## Setting up SSH

```toml
[sandbox]
backend = "ssh"
host = "you@devbox"
remote_dir = "/home/you/project"
```

Medha runs `ssh` from your machine, so set up key login first and check that `ssh you@devbox` works
without a password prompt. The project must already exist at `remote_dir`. Medha does not copy it.

The remote machine has its own files and network, so Medha's local jail does not apply there.
Medha treats these commands as unconfined.

## Turning it off for one run

```sh
medha --no-sandbox          # this run only
MEDHA_SANDBOX=host medha    # same, through the environment
MEDHA_SANDBOX=native medha  # force the native jail for this run
```

Without a jail, only Medha's command checks and your approvals stand between the model and your
machine.

## When something goes wrong

| Message | Cause | Fix |
|---|---|---|
| `native Git protection requires private user/mount namespaces; the OS refused setup` | Linux blocks unprivileged user namespaces. Ubuntu 23.10 and later do this by default | Ask an administrator to allow user namespaces for the Medha program in the system security policy (an AppArmor profile on Ubuntu) |
| `native Linux confinement is unavailable` | The kernel has no Landlock | Use a newer kernel, or `backend = "container"` |
| `container execution requires an image` | `backend = "container"` without `image` | Add `image = "..."` |
| `SSH execution needs a nonempty host and an installed ssh program` | `host` missing, or no `ssh` on PATH | Set `host` and install OpenSSH |
| A command fails with "no such image" | The image is not on this machine | `docker pull <image>` |
| A build cannot download dependencies | Network is denied | Approve the network request, or set `network = "allow"` |
| `native sandbox workspace contains host credential directories` | The opened folder contains something like `.ssh` or `.aws` | Open a narrower folder: the project, not your home folder |

A misconfigured `container` or `ssh` backend never falls back to running commands locally. Chat
keeps working, and commands stay unavailable until the setup is fixed.
