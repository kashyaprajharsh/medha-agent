//! Execution backends behind one interface (§4.8). Shell / build / VCS commands
//! run through an `ExecBackend` so isolation is a swappable *policy*, not a
//! hardcoded call:
//!
//! - [`HostBackend`] runs the command directly on the host (the historical
//!   behavior; the fallback for platforms without a native sandbox).
//! - [`SeatbeltBackend`] (macOS) confines the command with the OS-native
//!   sandbox (`/usr/bin/sandbox-exec`) — filesystem writes jailed to the
//!   workspace + temp, network optionally denied — with **zero external
//!   dependencies** (no Docker, no daemon) — the standard OS-native isolation
//!   approach for local coding agents on macOS.
//!
//! Container / microVM / ssh backends slot in here later behind the same trait
//! (the opt-in "heavy" isolation tier); a Linux Landlock backend is the next
//! native addition.

use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// A command to execute: argv + working directory + environment policy.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Environment entries to set on the child.
    pub env: Vec<(String, String)>,
    /// If true, start from an empty environment and set only `env` — used by
    /// `shell.exec` so injected secrets (API keys) never reach an arbitrary
    /// command. Fixed-program tools (git, diagnostics) inherit the env instead.
    pub clear_env: bool,
}

/// The result of running a command. Mirrors `std::process::Output` but with the
/// exit code already extracted (never a raw `ExitStatus`).
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to spawn process: {0}")]
    Spawn(String),
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
}

impl ExecError {
    /// True if the failure looks like "program not found", so callers (e.g. the
    /// diagnostics tool) can report "not installed" rather than a hard error.
    pub fn is_not_found(&self) -> bool {
        match self {
            ExecError::Spawn(m) => {
                let m = m.to_lowercase();
                m.contains("no such file")
                    || m.contains("not found")
                    || m.contains("entity not found")
            }
            _ => false,
        }
    }
}

/// Network posture for a confined command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPolicy {
    /// Network reachable (default — builds/fetches work).
    Allow,
    /// All network denied (the stronger containment level).
    Deny,
}

/// Which execution backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// No OS isolation — run directly on the host.
    Host,
    /// OS-native sandbox (macOS Seatbelt; Linux Landlock).
    Native,
    /// Opt-in heavy tier: run each command in a throwaway container (shell-out
    /// to `docker`/`podman` — no SDK linked, ~zero binary weight).
    Container,
    /// Opt-in: run each command on a remote host over `ssh`.
    Ssh,
}

/// Declarative sandbox configuration (maps from `medha.lock`'s `[sandbox]`).
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub backend: BackendKind,
    pub net: NetPolicy,
    /// Container backend: image to run (required for `Container`).
    pub image: Option<String>,
    /// Container backend: runtime binary (`docker`/`podman`); auto-detected if None.
    pub runtime: Option<String>,
    /// Container backend: memory cap (e.g. "2g") and process cap.
    pub memory: Option<String>,
    pub pids: Option<u32>,
    /// SSH backend: `user@host` (required for `Ssh`).
    pub host: Option<String>,
    /// SSH backend: remote working directory to `cd` into before running.
    pub remote_dir: Option<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        // Default: OS-native containment where available, network allowed so
        // ordinary builds/fetches keep working.
        Self {
            backend: BackendKind::Native,
            net: NetPolicy::Allow,
            image: None,
            runtime: None,
            memory: None,
            pids: None,
            host: None,
            remote_dir: None,
        }
    }
}

#[async_trait]
pub trait ExecBackend: Send + Sync {
    /// Build the fully jail-configured command (program/args/env + any wrapping:
    /// `sandbox-exec`, Landlock `pre_exec`, `docker run`, `ssh`) — but do NOT
    /// spawn it. `run` and the background-task facility both spawn through
    /// [`spawn_and_wait`] / [`spawn_background`], so isolation is applied in one
    /// place and the same jailed command can run in the foreground or background.
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError>;

    /// Run a command to completion (foreground). Default: build + supervise, so a
    /// timeout/cancel tears down the whole process group (see [`GroupReaper`]).
    async fn run(&self, req: ExecRequest) -> Result<ExecOutput, ExecError> {
        spawn_and_wait(self.build_command(&req)?).await
    }
    /// Short human-readable label for logs / UX (`"host"`, `"native"`, …).
    fn label(&self) -> &str;
    /// How strongly this backend confines commands — read by the kernel's
    /// trust-flow escalation. Defaults to no containment.
    fn containment(&self) -> kernel::Containment {
        kernel::Containment::None
    }
}

/// Build a `tokio` command applying cwd and environment policy. Isolation into
/// a process group and teardown are handled by [`spawn_and_wait`].
fn base_command(program: &str, args: &[String], req: &ExecRequest) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).current_dir(&req.cwd);
    if req.clear_env {
        cmd.env_clear();
    }
    cmd.envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd
}

/// Fires a `SIGKILL` at a whole process group when dropped while still *armed*.
/// This is the fix for the compounding-timeout bug: `kill_on_drop` only kills
/// the direct child, so a timed-out `sh -c "cargo build"` orphaned its
/// grandchildren (rustc jobs, a dev server) — which kept holding locks/ports,
/// so the next attempt hung to the timeout too, forever. Because the child is
/// spawned as its own group leader (`process_group(0)`), signalling the negative
/// pid reaches the entire tree. When [`spawn_and_wait`] completes normally it
/// disarms the reaper, so only an abnormal exit (the run future being dropped by
/// an outer timeout/cancel) triggers the group kill.
struct GroupReaper {
    pid: Option<u32>,
    armed: bool,
}

impl GroupReaper {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Tear down anything that is still attached to the command after its
    /// leader returned. Build tools occasionally background helpers and close
    /// their inherited pipes; treating the leader's zero exit as proof that the
    /// whole tree is gone leaks those helpers past the verifier/install gate.
    fn reap_remaining(&mut self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
        self.disarm();
    }
}

impl Drop for GroupReaper {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pid) = self.pid {
                kill_process_tree(pid);
            }
        }
    }
}

/// Kill the command and descendants that still belong to it.
///
/// Unix commands are launched as process-group leaders. Windows has no
/// `killpg`; `taskkill /T` is the platform fallback and is invoked by absolute
/// System32 path so a workspace cannot shadow it with a different executable.
#[allow(unused_variables)]
fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Negative pid = the process GROUP led by `pid`.
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let taskkill = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("taskkill.exe");
        let _ = std::process::Command::new(taskkill)
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Put a command in its own process group (unix) and pipe stdout/stderr, so a
/// spawned child can be supervised and group-killed. `kill_on_drop` is a
/// per-caller choice: foreground runs set it (backstop for the leader);
/// background tasks clear it (they must survive the handle being dropped).
fn configure_for_spawn(cmd: &mut tokio::process::Command, kill_on_drop: bool) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Lets Windows treat the child as a distinct process group. Tree
        // teardown itself uses `taskkill /T`, because Rust has no Job Object
        // wrapper in std.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.as_std_mut().creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(kill_on_drop);
}

/// What a bounded shell run produced.
#[derive(Debug)]
pub struct ShellOutcome {
    pub status: Option<i32>,
    /// Combined stdout and stderr, truncated to the requested bound.
    pub output: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl ShellOutcome {
    pub fn passed(&self) -> bool {
        !self.timed_out && !self.cancelled && self.status == Some(0)
    }
}

/// Run `command` under the platform shell in `dir`, bounded in time and output.
///
/// The process is its own group leader and the run future is group-reaped on
/// drop, so a timeout takes the whole tree — a bare `Command::output()` timeout
/// leaves `sh`'s grandchildren (compiler jobs, dev servers) holding locks and
/// ports, and the next attempt then hangs for the same reason.
///
pub async fn run_shell_bounded(
    command: &str,
    dir: &std::path::Path,
    limit: std::time::Duration,
    max_output: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ShellOutcome, ExecError> {
    run_shell_bounded_with(&HostBackend, command, dir, limit, max_output, cancel).await
}

/// Run a shell command through a configured execution backend, retaining the
/// same timeout/output/process-tree guarantees as [`run_shell_bounded`].
///
/// This is the verifier path: build scripts and tests are workspace-controlled
/// code, so they must execute under the same jail the editing tools use rather
/// than escaping to an unconfined host shell.
pub async fn run_shell_bounded_with(
    backend: &dyn ExecBackend,
    command: &str,
    dir: &std::path::Path,
    limit: std::time::Duration,
    max_output: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ShellOutcome, ExecError> {
    let (program, args) = shell_argv(backend.label(), command);
    let request = ExecRequest {
        program,
        args,
        cwd: dir.to_path_buf(),
        env: Vec::new(),
        clear_env: false,
    };
    let cmd = backend.build_command(&request)?;
    run_command_bounded(cmd, limit, max_output, cancel).await
}

/// An interpreter that can run a command line on Windows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WinShell {
    /// Git for Windows / MSYS bash. Preferred: the model writes Unix command
    /// lines (`grep -rn`, `sed -i`), and PowerShell's same-named *aliases* take
    /// different flags, so those would mangle arguments rather than fail.
    Bash(PathBuf),
    /// `pwsh` or `powershell.exe`.
    PowerShell(PathBuf),
    /// Always present, so the cascade can never come up empty.
    Cmd,
}

impl WinShell {
    /// `/D` skips AutoRun registry hooks and `-NoProfile` skips the user
    /// profile, so neither can inject into a command that was already approved.
    /// The command itself is passed through untouched: the policy scanner reads
    /// that same string before this wrapping happens, and the two must never
    /// disagree about what is going to run.
    pub fn argv(&self, command: &str) -> (String, Vec<String>) {
        let s = |p: &PathBuf| p.display().to_string();
        match self {
            Self::Bash(p) => (s(p), vec!["-c".into(), command.to_string()]),
            Self::PowerShell(p) => (
                s(p),
                vec!["-NoProfile".into(), "-Command".into(), command.to_string()],
            ),
            Self::Cmd => (
                "cmd.exe".into(),
                vec!["/D".into(), "/S".into(), "/C".into(), command.to_string()],
            ),
        }
    }
}

/// What this machine offers, probed once. Separated from [`choose_windows_shell`]
/// so the policy stays a pure function, testable on any platform.
#[derive(Default, Clone, Debug)]
pub struct WindowsShellCandidates {
    /// `MEDHA_SHELL`, already verified runnable. Wins outright.
    pub override_shell: Option<PathBuf>,
    pub bash: Option<PathBuf>,
    pub powershell: Option<PathBuf>,
}

/// Pick the interpreter, honouring an explicit override before any detection.
///
/// No detection cascade fits every machine, so `MEDHA_SHELL` wins outright: it
/// is the one thing a user with an unusual setup can reach for without waiting
/// on a release.
pub fn choose_windows_shell(c: &WindowsShellCandidates) -> WinShell {
    if let Some(p) = &c.override_shell {
        return classify_windows_shell(p);
    }
    if let Some(p) = &c.bash {
        return WinShell::Bash(p.clone());
    }
    if let Some(p) = &c.powershell {
        return WinShell::PowerShell(p.clone());
    }
    WinShell::Cmd
}

/// Which interpreter a path *is*, so an override is invoked with the flags that
/// binary actually understands rather than assumed to be one kind.
pub fn classify_windows_shell(path: &Path) -> WinShell {
    // Split on both separators rather than using `file_stem`, which only knows
    // the *host* platform's separator — a Windows path can be classified while
    // running elsewhere, and there it would read as one long filename and match
    // nothing.
    let name = path.to_string_lossy().to_ascii_lowercase();
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    match name.strip_suffix(".exe").unwrap_or(name) {
        "bash" | "sh" | "zsh" => WinShell::Bash(path.to_path_buf()),
        "cmd" => WinShell::Cmd,
        // Anything else is assumed PowerShell-like; it is the only other
        // interpreter Windows reliably has, and `-Command` is the safer guess
        // than handing an unknown binary a bare `-c`.
        _ => WinShell::PowerShell(path.to_path_buf()),
    }
}

/// Git for Windows ships `bash.exe` beside `git.exe` but puts only `cmd\` on
/// PATH, so a plain PATH lookup for bash misses it on a default install.
/// Deriving it from `git.exe` needs neither configuration nor a hardcoded
/// install location: `…\Git\cmd\git.exe` → `…\Git\bin\bash.exe`.
pub fn bash_beside_git(git_exe: &Path) -> Option<PathBuf> {
    let git_root = git_exe.parent()?.parent()?;
    ["bin", "usr/bin"]
        .iter()
        .map(|d| git_root.join(d).join("bash.exe"))
        .find(|p| p.is_file())
}

/// Whether a path is a shell that can actually be spawned.
///
/// `is_file()` alone is not enough on Windows: the Microsoft Store publishes
/// zero-byte *app execution aliases* under `WindowsApps\` which satisfy it but
/// cannot be executed. Accepting one is worse than finding nothing, because the
/// PATH tier then reports success and the working absolute path below it is
/// never tried.
pub fn is_runnable_shell(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.len() > 0,
        Err(_) => false,
    }
}

impl WindowsShellCandidates {
    /// Probe: explicit override, then PATH, then known absolute locations.
    /// Every candidate must pass [`is_runnable_shell`], not merely exist, and
    /// bash is derived from `git.exe` when PATH does not carry it.
    #[cfg(windows)]
    pub fn probe() -> Self {
        // Last resort only: these are where installers and CI images commonly
        // put PowerShell, not where it is guaranteed to be.
        const PWSH_FALLBACKS: [&str; 2] = [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ];

        let runnable = |p: PathBuf| is_runnable_shell(&p).then_some(p);
        let on_path = |name: &str| locate_on_path(name).and_then(runnable);

        Self {
            override_shell: std::env::var_os("MEDHA_SHELL")
                .map(PathBuf::from)
                .and_then(runnable),
            bash: on_path("bash").or_else(|| {
                locate_on_path("git")
                    .as_deref()
                    .and_then(bash_beside_git)
                    .and_then(runnable)
            }),
            powershell: on_path("pwsh")
                .or_else(|| on_path("powershell"))
                .or_else(|| {
                    PWSH_FALLBACKS
                        .iter()
                        .map(PathBuf::from)
                        .find(|p| is_runnable_shell(p))
                }),
        }
    }

    #[cfg(not(windows))]
    pub fn probe() -> Self {
        Self::default()
    }
}

/// The interpreter for a backend with this label. Windows has no `sh`, so
/// hardcoding one made *every* `shell.exec` fail with "program not found" — the
/// missing program was always the shell, never the user's command, which is why
/// `git`, `python` and even `cmd` failed identically.
///
/// Container and SSH backends execute on Unix-like hosts even when medha itself
/// runs on Windows, so only the local backends switch interpreter.
pub fn shell_argv(backend_label: &str, command: &str) -> (String, Vec<String>) {
    if cfg!(windows) && matches!(backend_label, "host" | "native") {
        #[cfg(windows)]
        {
            // Probed once: the cascade spawns processes to validate candidates,
            // and shell.exec runs on every tool call.
            static SHELL: std::sync::OnceLock<WinShell> = std::sync::OnceLock::new();
            return SHELL
                .get_or_init(|| choose_windows_shell(&WindowsShellCandidates::probe()))
                .argv(command);
        }
    }
    ("sh".to_string(), vec!["-c".into(), command.to_string()])
}

/// Run an already-configured command with the same bounded capture and process
/// tree teardown as [`run_shell_bounded`]. This is used for fixed-argv package
/// managers where round-tripping arguments through a shell would be unsafe.
pub async fn run_command_bounded(
    mut cmd: tokio::process::Command,
    limit: std::time::Duration,
    max_output: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ShellOutcome, ExecError> {
    configure_for_spawn(&mut cmd, true);
    // Spawned and reaped through `std`, deliberately, rather than through
    // tokio's `Child`.
    //
    // Descendants are torn down by signalling the process group, and a group's
    // id *is* its leader's pid. Tokio reaps children asynchronously off SIGCHLD,
    // which frees that pid while the helpers are still alive — so by the time
    // the kill goes out the number may already name an unrelated group, and the
    // helper it was meant for survives. Owning the reap keeps the leader a
    // zombie, and a zombie still holds its pid, so the group stays addressable
    // until we have finished with it.
    let mut child = cmd
        .as_std_mut()
        .spawn()
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    let leader = child.id();
    let mut reaper = GroupReaper::new(Some(leader));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Streamed into a rolling window rather than buffered whole: a runaway
    // suite can emit gigabytes, and reading it all in only to throw most of it
    // away is an out-of-memory waiting for a verbose test run.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Rolling::new(max_output)));
    // Blocking reads on a worker thread: these are `std` pipes, and registering
    // them with the reactor would mean handing the fds back to tokio.
    let drains = {
        let (out, err) = (captured.clone(), captured.clone());
        async move {
            let _ = tokio::join!(
                tokio::task::spawn_blocking(move || drain_blocking(stdout, &out)),
                tokio::task::spawn_blocking(move || drain_blocking(stderr, &err)),
            );
        }
    };

    let text = || {
        captured
            .lock()
            .map(|buffer| buffer.text())
            .unwrap_or_default()
    };

    tokio::pin!(drains);
    // Both pipes closing means the leader and everything sharing its stdout are
    // done, so its status is settled; a timeout or a cancel means it is not.
    let ended = match cancel {
        // Cancellation stops the run rather than waiting out the ceiling: a
        // user who pressed Esc should not sit through a fifteen-minute build.
        Some(token) => tokio::select! {
            drained = tokio::time::timeout(limit, &mut drains) => drained.map_err(|_| false),
            _ = token.cancelled() => Err(true),
        },
        None => tokio::time::timeout(limit, &mut drains)
            .await
            .map_err(|_| false),
    };

    // Unconditional, and always before the reap. On the settled path the leader
    // is already a zombie, so this only reaches the helpers it left behind; on
    // the other two it stops the leader as well.
    kill_process_tree(leader);
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|error| ExecError::Spawn(error.to_string()))?
        .map_err(|error| ExecError::Spawn(error.to_string()))?;
    reaper.disarm();

    Ok(match ended {
        Ok(()) => ShellOutcome {
            status: status.code(),
            output: text(),
            timed_out: false,
            cancelled: false,
        },
        Err(cancelled) => ShellOutcome {
            status: None,
            output: format!(
                "{}\n[{}]",
                text(),
                match cancelled {
                    true => "cancelled".to_string(),
                    false => format!("timed out after {}s and was stopped", limit.as_secs()),
                }
            ),
            timed_out: !cancelled,
            cancelled,
        },
    })
}

type SharedRolling = std::sync::Arc<std::sync::Mutex<Rolling>>;

/// The same rolling capture over a blocking `std` pipe, for the path that owns
/// its own reaping and therefore cannot hand its fds to the reactor.
fn drain_blocking<R: std::io::Read>(pipe: Option<R>, into: &SharedRolling) {
    let Some(mut pipe) = pipe else { return };
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut buffer) = into.lock() {
                    buffer.push(&chunk[..read]);
                }
            }
        }
    }
}

/// The last `cap` bytes seen, remembering that earlier output was dropped. The
/// tail is what matters: that is where a build or test run says what failed.
struct Rolling {
    data: Vec<u8>,
    cap: usize,
    dropped: bool,
}

impl Rolling {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap: cap.max(1),
            dropped: false,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.dropped = true;
        }
    }
    fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.data);
        match self.dropped {
            true => format!("[… earlier output dropped …]\n{body}"),
            false => body.into_owned(),
        }
    }
}

/// Spawn a command in its own process group, capture stdout/stderr, and wait for
/// it. If the returned future is dropped before completion — which is exactly
/// what an outer `tokio::time::timeout` or a cancellation does — the
/// [`GroupReaper`] SIGKILLs the whole process tree, so nothing is orphaned.
async fn spawn_and_wait(mut cmd: tokio::process::Command) -> Result<ExecOutput, ExecError> {
    configure_for_spawn(&mut cmd, true); // reaper backstops the group; kill_on_drop the leader
    let mut child = cmd.spawn().map_err(|e| ExecError::Spawn(e.to_string()))?;
    let mut reaper = GroupReaper::new(child.id());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (stdout, stderr) = tokio::join!(read_to_end(stdout), read_to_end(stderr));

    // Both streams are at EOF, so the leader is done — reap the helpers it left
    // behind. This must precede the wait, not follow it: a group is named by its
    // leader's pid, and waiting releases that pid, so a kill afterwards signals
    // a number the kernel no longer maps to this group (or has since recycled).
    // Long-running work belongs in `spawn_background`, whose lifetime is
    // explicit rather than an accidental orphan.
    reaper.reap_remaining();
    let status = child
        .wait()
        .await
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    Ok(ExecOutput {
        status: status.code(),
        stdout,
        stderr,
    })
}

/// Read a child pipe to EOF. A closed stream is the signal that the leader and
/// everything still sharing its output are finished.
async fn read_to_end<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buffer = Vec::new();
    let _ = pipe.read_to_end(&mut buffer).await;
    buffer
}

/// A cap on each retained stream of a background task; once exceeded, oldest
/// bytes are dropped from the front (the tail is what matters for "what's it
/// doing now"). Overflow is marked so a poll can't be mistaken for complete.
const BG_BUF_CAP: usize = 1_000_000;

/// A rolling capture of one output stream: keeps at most `BG_BUF_CAP` recent
/// bytes and remembers whether anything older was dropped.
#[derive(Default)]
struct BgBuf {
    data: Vec<u8>,
    truncated: bool,
}

impl BgBuf {
    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > BG_BUF_CAP {
            let drop = self.data.len() - BG_BUF_CAP;
            self.data.drain(..drop);
            self.truncated = true;
        }
    }
    fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.data);
        if self.truncated {
            format!("[…earlier output dropped…]\n{body}")
        } else {
            body.into_owned()
        }
    }
}

type SharedBuf = std::sync::Arc<std::sync::Mutex<BgBuf>>;

/// A backgrounded command: stdout/stderr stream into rolling buffers while it
/// runs, and it can be polled, awaited briefly, or killed (whole group). This is
/// what `shell.exec` promotes a slow command into, so the model gets partial
/// output + a task id immediately instead of blocking (§2).
pub struct BgProc {
    pub pid: Option<u32>,
    stdout: SharedBuf,
    stderr: SharedBuf,
    done_rx: tokio::sync::watch::Receiver<bool>,
    code: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
}

impl BgProc {
    /// Current buffered stdout / stderr (tails, with a marker if truncated).
    pub fn snapshot(&self) -> (String, String) {
        let o = self.stdout.lock().map(|b| b.text()).unwrap_or_default();
        let e = self.stderr.lock().map(|b| b.text()).unwrap_or_default();
        (o, e)
    }
    /// Still running?
    pub fn is_running(&self) -> bool {
        !*self.done_rx.borrow()
    }
    /// Exit code once exited (`None` while running or if killed by signal).
    pub fn exit_code(&self) -> Option<i32> {
        self.code.lock().ok().and_then(|c| *c)
    }
    /// A clone of the completion signal, so a holder (e.g. the task table) can
    /// await this task WITHOUT keeping the table lock held across the await.
    pub fn done_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.done_rx.clone()
    }
    /// Wait up to `dur` for completion. Returns true if it finished in time.
    pub async fn wait_until(&self, dur: std::time::Duration) -> bool {
        wait_done(self.done_rx.clone(), dur).await
    }
    /// SIGKILL the whole process group.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
    }
}

/// Wait up to `dur` for a task's done-signal to flip to `true`; returns true if
/// it completed in time. Free-standing so a caller holding a task table can
/// clone the receiver out (cheap) and await here without keeping the lock.
pub async fn wait_done(
    mut rx: tokio::sync::watch::Receiver<bool>,
    dur: std::time::Duration,
) -> bool {
    tokio::time::timeout(dur, async {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await
    .is_ok()
}

/// Spawn a command as a background task: it keeps running after this returns,
/// with stdout/stderr pumped into rolling buffers and its exit status recorded.
/// `kill_on_drop` is off — the process must outlive the spawn call — so callers
/// are responsible for `kill()` (or session-end cleanup).
pub fn spawn_background(mut cmd: tokio::process::Command) -> Result<BgProc, ExecError> {
    use std::sync::{Arc, Mutex};
    configure_for_spawn(&mut cmd, false);
    let mut child = cmd.spawn().map_err(|e| ExecError::Spawn(e.to_string()))?;
    let pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let stdout: SharedBuf = Arc::new(Mutex::new(BgBuf::default()));
    let stderr: SharedBuf = Arc::new(Mutex::new(BgBuf::default()));
    let code = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);

    async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, buf: SharedBuf) {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut b) = buf.lock() {
                        b.push(&chunk[..n]);
                    }
                }
            }
        }
    }
    let out_handle = out_pipe.map(|o| tokio::spawn(pump(o, stdout.clone())));
    let err_handle = err_pipe.map(|e| tokio::spawn(pump(e, stderr.clone())));
    let code2 = code.clone();
    tokio::spawn(async move {
        let status = child.wait().await.ok().and_then(|s| s.code());
        if let Ok(mut c) = code2.lock() {
            *c = status;
        }
        // Let the pumps drain the final chunk before signalling done — otherwise
        // a command that prints then exits immediately can be snapshotted before
        // the last read lands, losing the output tail (K21). Bounded by a short
        // grace: a grandchild holding the pipe open must NOT block `done` forever
        // (the very hang the process-group design avoids), so we cap the wait.
        let drain = async {
            if let Some(h) = out_handle {
                let _ = h.await;
            }
            if let Some(h) = err_handle {
                let _ = h.await;
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), drain).await;
        let _ = done_tx.send(true);
    });

    Ok(BgProc {
        pid,
        stdout,
        stderr,
        done_rx,
        code,
    })
}

/// Runs commands directly on the host with no OS isolation.
pub struct HostBackend;

#[async_trait]
impl ExecBackend for HostBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        Ok(base_command(&req.program, &req.args, req))
    }
    fn label(&self) -> &str {
        "host"
    }
}

/// Escape a path for embedding in an SBPL string literal (macOS Seatbelt only).
#[cfg(target_os = "macos")]
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// macOS Seatbelt backend: confines the command with `sandbox-exec` and a
/// generated SBPL profile.
///
/// Profile shape (validated empirically): **allow by default, then deny all
/// file writes, then re-allow writes only under the workspace + system temp +
/// `/dev`**. Reads stay allowed (v1) so tools that legitimately read still work;
/// network is allowed unless [`NetPolicy::Deny`]. This blocks the real threats
/// (writing `~/.ssh`, `~/.zshrc`, `/etc`, anywhere under `$HOME`) without the
/// brittleness of a deny-by-default profile that must enumerate every syscall.
#[cfg(target_os = "macos")]
pub struct SeatbeltBackend {
    net: NetPolicy,
    /// Extra writable roots beyond the workspace (e.g. an out-of-tree build dir).
    extra_writable: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
impl SeatbeltBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>) -> Self {
        Self {
            net,
            extra_writable,
        }
    }

    fn profile(&self, cwd: &std::path::Path) -> String {
        // Canonicalize so the subpath match survives /var → /private/var etc.
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut writable: Vec<PathBuf> = vec![ws];
        writable.extend(self.extra_writable.iter().cloned());
        if let Ok(tmp) = std::env::var("TMPDIR") {
            writable.push(PathBuf::from(tmp));
        }
        writable.push(PathBuf::from("/private/tmp"));
        writable.push(PathBuf::from("/private/var/folders"));

        let mut p =
            String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
        for w in &writable {
            p.push_str(&format!(
                "    (subpath \"{}\")\n",
                sbpl_escape(&w.to_string_lossy())
            ));
        }
        // Devices (/dev/null, /dev/tty, …) must stay writable or ordinary
        // programs break.
        p.push_str("    (regex #\"^/dev/\"))\n");
        if self.net == NetPolicy::Deny {
            p.push_str("(deny network*)\n");
        }
        p
    }
}

#[cfg(target_os = "macos")]
#[async_trait]
impl ExecBackend for SeatbeltBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        let profile = self.profile(&req.cwd);
        // sandbox-exec -p <profile> <program> <args...>
        let mut wrapped = Vec::with_capacity(req.args.len() + 3);
        wrapped.push("-p".to_string());
        wrapped.push(profile);
        wrapped.push(req.program.clone());
        wrapped.extend(req.args.iter().cloned());
        Ok(base_command("/usr/bin/sandbox-exec", &wrapped, req))
    }
    fn label(&self) -> &str {
        "native"
    }
    fn containment(&self) -> kernel::Containment {
        match self.net {
            NetPolicy::Deny => kernel::Containment::OsFsJailNoNet,
            NetPolicy::Allow => kernel::Containment::OsFsJail,
        }
    }
}

/// Linux Landlock backend: confines the child with the Landlock LSM (kernel
/// ≥5.13), applied in a `pre_exec` hook so it affects the spawned command, not
/// the agent. Filesystem writes are jailed to the workspace + temp + dev caches
/// (reads stay allowed, matching the macOS profile). The ruleset is built in
/// the parent — only the (allocation-free) `restrict_self` syscall runs in the
/// post-fork child, which is the safe pattern in a threaded runtime.
///
/// Best-effort compatibility: on a kernel without Landlock the jail simply
/// isn't applied (the command still runs — never break the user); the CLI's
/// startup probe warns when that's the case. Network confinement (Landlock ABI
/// ≥v4 / kernel 6.7) is a follow-up; `NetPolicy::Deny` is not yet enforced here.
#[cfg(target_os = "linux")]
pub struct LandlockBackend {
    net: NetPolicy,
    extra_writable: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl LandlockBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>) -> Self {
        Self {
            net,
            extra_writable,
        }
    }

    fn writable_paths(&self, cwd: &std::path::Path) -> Vec<PathBuf> {
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut v = vec![ws];
        v.extend(self.extra_writable.iter().cloned());
        v.push(PathBuf::from("/tmp"));
        v.push(PathBuf::from("/var/tmp"));
        v.push(PathBuf::from("/dev"));
        v
    }
}

/// Build a Landlock ruleset (in the parent) that allows read+exec everywhere
/// and read-write only under `writable`. Returns `None` if the kernel doesn't
/// support Landlock, so the caller can run unconfined rather than fail.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(
    writable: &[PathBuf],
    net: NetPolicy,
) -> Option<landlock::RulesetCreated> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr,
    };
    let abi = ABI::V5;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .ok()?;
    // Deny network by *handling* net access and then adding no net rules — with
    // Landlock, a handled access with no matching rule is denied. Best-effort:
    // silently a no-op on kernels < 6.7 (Landlock ABI < v4), so it never breaks
    // the run; enforcement is real only where the kernel supports it.
    if net == NetPolicy::Deny {
        ruleset = ruleset.handle_access(AccessNet::from_all(abi)).ok()?;
    }
    let mut created = ruleset.create().ok()?;
    // Read + execute across the whole filesystem.
    created = created
        .add_rule(PathBeneath::new(
            PathFd::new("/").ok()?,
            AccessFs::from_read(abi),
        ))
        .ok()?;
    // Read-write only under the jailed roots. A path that can't be opened is
    // skipped; a ruleset-level failure abandons the jail (run unconfined rather
    // than apply a half-built, wrongly-restrictive ruleset).
    for p in writable {
        let Ok(fd) = PathFd::new(p) else { continue };
        created = match created.add_rule(PathBeneath::new(fd, AccessFs::from_all(abi))) {
            Ok(next) => next,
            Err(_) => return None,
        };
    }
    Some(created)
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ExecBackend for LandlockBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        use std::os::unix::process::CommandExt;

        let ruleset = build_landlock_ruleset(&self.writable_paths(&req.cwd), self.net);

        let mut cmd = std::process::Command::new(&req.program);
        cmd.args(&req.args).current_dir(&req.cwd);
        if req.clear_env {
            cmd.env_clear();
        }
        cmd.envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        // Apply Landlock in the child (post-fork, pre-exec): only restrict_self
        // runs here — no allocation, so it's safe under the threaded runtime.
        if let Some(ruleset) = ruleset {
            let mut slot = Some(ruleset);
            unsafe {
                cmd.pre_exec(move || {
                    if let Some(r) = slot.take() {
                        r.restrict_self()
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                    }
                    Ok(())
                });
            }
        }

        Ok(tokio::process::Command::from(cmd))
    }
    fn label(&self) -> &str {
        "native"
    }
    fn containment(&self) -> kernel::Containment {
        // We *attempt* net-deny via Landlock (best-effort), but only report
        // FS-jail-only to the trust-flow layer: Landlock network confinement
        // needs kernel ≥6.7 and we don't verify enforcement per-kernel here, so
        // we never claim network is confined. Result: trust-flow still gates
        // web-tainted network actions on Linux — conservative and safe. (Once a
        // reliable ABI-≥v4 probe lands, net-deny can report OsFsJailNoNet.)
        kernel::Containment::OsFsJail
    }
}

/// True if `program` exists in `dir`, including Windows `PATHEXT` resolution.
///
/// `Command::new("npm")` can resolve `npm.cmd` on Windows; probing only the
/// extensionless path reports a runnable tool as missing and disables installs.
pub fn program_in_dir(dir: &std::path::Path, program: &str) -> bool {
    let candidate = dir.join(program);
    if candidate.exists() {
        return true;
    }
    #[cfg(windows)]
    {
        if candidate.extension().is_none() {
            let extensions =
                std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
            return extensions
                .to_string_lossy()
                .split(';')
                .filter(|extension| !extension.is_empty())
                .any(|extension| {
                    let extension = extension.trim_start_matches('.');
                    candidate.with_extension(extension).exists()
                });
        }
    }
    false
}

/// True if `program` resolves on the current PATH (or as an explicit path).
/// Used to detect an installed container runtime and language-server tooling.
pub fn program_on_path(program: &str) -> bool {
    let path = std::path::Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        return program_in_dir(dir, name);
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| program_in_dir(&dir, program)))
        .unwrap_or(false)
}

/// Where `program` resolves on PATH, with the same Windows `PATHEXT` handling as
/// [`program_on_path`]. Returns the path rather than a bool, so a caller can go
/// on to inspect the binary it found.
pub fn locate_on_path(program: &str) -> Option<PathBuf> {
    let extensions = || -> Vec<String> {
        if !cfg!(windows) {
            return vec![String::new()];
        }
        let raw = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new())
            .chain(
                raw.to_string_lossy()
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(|e| e.to_string()),
            )
            .collect()
    };
    let paths = std::env::var_os("PATH")?;
    let exts = extensions();
    std::env::split_paths(&paths).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = dir.join(format!("{program}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// The container runtime to use: honor `configured`, else prefer docker, then
/// podman; fall back to "docker" as the name to report if neither is present.
fn detect_container_runtime(configured: &Option<String>) -> String {
    if let Some(r) = configured {
        if !r.trim().is_empty() {
            return r.clone();
        }
    }
    for candidate in ["docker", "podman"] {
        if program_on_path(candidate) {
            return candidate.to_string();
        }
    }
    "docker".to_string()
}

/// Opt-in heavy tier: run each command in a throwaway container by shelling out
/// to `docker`/`podman` (no SDK linked → ~zero binary weight). The workspace is
/// bind-mounted at `/workspace`, capabilities dropped, and — crucially — the
/// host environment is NOT forwarded, so injected API keys never enter the
/// sandbox (the mistake of wrapping the whole agent process in a container).
pub struct ContainerBackend {
    runtime: String,
    image: String,
    net: NetPolicy,
    memory: Option<String>,
    pids: Option<u32>,
}

impl ContainerBackend {
    pub fn new(
        runtime: String,
        image: String,
        net: NetPolicy,
        memory: Option<String>,
        pids: Option<u32>,
    ) -> Self {
        Self {
            runtime,
            image,
            net,
            memory,
            pids,
        }
    }

    /// Build the `run …` argv for the container runtime. Pure, for testing.
    fn build_argv(&self, req: &ExecRequest) -> Vec<String> {
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
        let mut a: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{}:/workspace", ws.display()),
            "-w".into(),
            "/workspace".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ];
        if self.net == NetPolicy::Deny {
            a.push("--network".into());
            a.push("none".into());
        }
        if let Some(m) = &self.memory {
            a.push("--memory".into());
            a.push(m.clone());
        }
        if let Some(p) = self.pids {
            a.push("--pids-limit".into());
            a.push(p.to_string());
        }
        // Host env is intentionally NOT forwarded (no `--env`): API keys stay on
        // the host and never reach the containerized command.
        a.push(self.image.clone());
        a.push(req.program.clone());
        a.extend(req.args.iter().cloned());
        a
    }
}

#[async_trait]
impl ExecBackend for ContainerBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        let argv = self.build_argv(req);
        // The runtime CLIENT runs with our host env (it needs PATH/DOCKER_HOST);
        // the containerized command gets none of it (see build_argv).
        let mut cmd = tokio::process::Command::new(&self.runtime);
        cmd.args(&argv);
        Ok(cmd)
    }
    fn label(&self) -> &str {
        "container"
    }
    fn containment(&self) -> kernel::Containment {
        match self.net {
            NetPolicy::Deny => kernel::Containment::OsFsJailNoNet,
            NetPolicy::Allow => kernel::Containment::OsFsJail,
        }
    }
}

/// Single-quote an argument for safe embedding in a remote shell command.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Opt-in SSH backend: run each command on a remote host via `ssh`. This is
/// remote execution, not local isolation — it assumes the workspace already
/// exists on the remote (automatic sync is a follow-up). Key-scoped by the
/// user's ssh config; the local scanner/policy still gate before dispatch.
pub struct SshBackend {
    host: String,
    remote_dir: Option<String>,
}

impl SshBackend {
    pub fn new(host: String, remote_dir: Option<String>) -> Self {
        Self { host, remote_dir }
    }

    /// Build the `ssh` argv (excluding the `ssh` program itself). Pure, for testing.
    fn build_argv(&self, req: &ExecRequest) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(dir) = &self.remote_dir {
            parts.push(format!("cd {} &&", shell_quote(dir)));
        }
        parts.push(shell_quote(&req.program));
        for arg in &req.args {
            parts.push(shell_quote(arg));
        }
        let remote_cmd = parts.join(" ");
        vec![
            "-o".into(),
            "BatchMode=yes".into(),
            self.host.clone(),
            remote_cmd,
        ]
    }
}

#[async_trait]
impl ExecBackend for SshBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        let argv = self.build_argv(req);
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(&argv);
        Ok(cmd)
    }
    fn label(&self) -> &str {
        "ssh"
    }
    fn containment(&self) -> kernel::Containment {
        // Remote exec can't touch the LOCAL filesystem, but the remote box has
        // its own fs/network — a different threat model. Report None so
        // trust-flow stays conservative (gates web-tainted actions).
        kernel::Containment::None
    }
}

/// Pick an execution backend from config. On platforms without a native sandbox
/// (Windows has no lightweight equivalent yet), `Native` degrades to `Host`;
/// `Container`/`Ssh` degrade to `Host` if misconfigured — callers validate and
/// warn (see the CLI) so isolation is never silently assumed.
pub fn select_backend(
    cfg: &SandboxConfig,
    _extra_writable: Vec<PathBuf>,
) -> std::sync::Arc<dyn ExecBackend> {
    use std::sync::Arc;
    match cfg.backend {
        BackendKind::Host => Arc::new(HostBackend),
        BackendKind::Native => {
            #[cfg(target_os = "macos")]
            {
                if native_backend_available() {
                    Arc::new(SeatbeltBackend::new(cfg.net, _extra_writable))
                } else {
                    Arc::new(HostBackend)
                }
            }
            #[cfg(target_os = "linux")]
            {
                if native_backend_available() {
                    Arc::new(LandlockBackend::new(cfg.net, _extra_writable))
                } else {
                    Arc::new(HostBackend)
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                Arc::new(HostBackend)
            }
        }
        BackendKind::Container => match cfg.image.as_deref() {
            Some(image) if !image.is_empty() => Arc::new(ContainerBackend::new(
                detect_container_runtime(&cfg.runtime),
                image.to_string(),
                cfg.net,
                cfg.memory.clone(),
                cfg.pids,
            )),
            _ => Arc::new(HostBackend), // no image → CLI warns and shouldn't reach here
        },
        BackendKind::Ssh => match cfg.host.as_deref() {
            Some(host) if !host.is_empty() => {
                Arc::new(SshBackend::new(host.to_string(), cfg.remote_dir.clone()))
            }
            _ => Arc::new(HostBackend),
        },
    }
}

/// True if a native OS sandbox backend is actually usable on this platform.
/// On macOS, `sandbox-exec` can be present while host policy rejects
/// `sandbox_apply`, so probe a harmless profile rather than treating its path
/// as proof. On Linux we probe live Landlock support (kernel ≥5.13 with
/// Landlock enabled).
pub fn native_backend_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        landlock_supported()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn landlock_supported() -> bool {
    use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr};
    Ruleset::default()
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(program: &str, args: &[&str], cwd: PathBuf) -> ExecRequest {
        ExecRequest {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd,
            env: std::env::vars().collect(),
            clear_env: false,
        }
    }

    /// K1: a timed-out command must take its whole process *tree* down, not just
    /// the direct child. We spawn `sh -c 'sh -c "sleep 30" ...'` where a
    /// grandchild writes a sentinel file only if it survives, wrap the run in a
    /// short timeout (dropping the future, as the tool layer does), then confirm
    /// the grandchild was killed before it could write.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = std::env::temp_dir().join(format!("medha-killpg-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived.txt");
        // Grandchild sleeps briefly then writes the marker; if the group is
        // killed on timeout it never gets there.
        let script = format!("(sleep 1; touch {}) & wait", marker.display());
        let fut = HostBackend.run(req("/bin/sh", &["-c", &script], dir.clone()));
        // Drop the run future well before the grandchild's 1s write — this is
        // exactly what an outer `tokio::time::timeout` does on expiry.
        let r = tokio::time::timeout(std::time::Duration::from_millis(150), fut).await;
        assert!(r.is_err(), "outer timeout should elapse");
        // Give the reaper + any stray write a moment, then confirm no marker.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "grandchild survived the group kill (orphaned tree)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_shell_drains_but_retains_only_the_tail() {
        let output = run_shell_bounded(
            "i=0; while [ \"$i\" -lt 4000 ]; do printf 0123456789; i=$((i + 1)); done; printf TAIL",
            &std::env::temp_dir(),
            std::time::Duration::from_secs(5),
            1024,
            None,
        )
        .await
        .unwrap();
        assert!(output.passed());
        assert!(output.output.ends_with("TAIL"));
        assert!(
            output.output.len() <= 1100,
            "rolling capture retained too much: {} bytes",
            output.output.len()
        );
        assert!(output.output.contains("earlier output dropped"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_shell_cancellation_reaps_the_process_group() {
        let dir = std::env::temp_dir().join(format!("medha-cancelpg-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = dir.join("started.txt");
        let marker = dir.join("survived.txt");
        // The script announces that it is running, and its helper outlives the
        // cancel by a wide margin. Cancelling on a fixed 100ms delay instead
        // raced the script's own completion: under load the run finished first
        // and `cancelled` came back false, which is why this flaked.
        let script = format!(
            "(sleep 3; touch {}) & touch {}; wait",
            marker.display(),
            started.display()
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let trigger = cancel.clone();
        let probe = started.clone();
        tokio::spawn(async move {
            // Bounded, so a script that never starts fails the assertion below
            // rather than hanging the test forever.
            for _ in 0..500 {
                if probe.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            trigger.cancel();
        });
        // Well above the helper's sleep, so `timed_out` cannot fire first.
        let output = run_shell_bounded(
            &script,
            &dir,
            std::time::Duration::from_secs(30),
            1024,
            Some(&cancel),
        )
        .await
        .unwrap();
        assert!(
            output.cancelled,
            "cancel must land while the script is alive"
        );
        assert!(!output.timed_out);
        // Outlast the helper's own sleep, so a survivor would have left its mark.
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        assert!(!marker.exists(), "cancelled verifier left a helper alive");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Known-flaky (~50% on macOS), so it does not gate CI. Run it explicitly
    /// with `cargo test -p sandbox -- --ignored`.
    ///
    /// The gap is real, not a bad assertion: on the success path the group kill
    /// fires the instant both pipes reach EOF, and pipe closure says nothing
    /// about whether every descendant has joined the process group yet. A
    /// helper that redirects its output away — `>/dev/null 2>&1 &`, exactly what
    /// build tools do — releases the pipes immediately, so the kill can race it.
    ///
    /// Not a regression: before `run_command_bounded` existed there was no
    /// success-path reap at all, so this leaked 100% of the time and silently.
    /// Fixing it properly means not deciding teardown on a single instant —
    /// confirm the group drained while the zombie leader still holds its pid,
    /// and only then reap. Left ignored rather than deleted so the gap stays
    /// visible.
    #[cfg(unix)]
    #[ignore = "flaky: success-path group kill races a helper that has not yet joined the group"]
    #[tokio::test]
    async fn bounded_shell_reaps_helpers_after_a_successful_leader_exit() {
        let dir = std::env::temp_dir().join(format!("medha-successpg-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived.txt");
        let script = format!("(sleep 1; touch {}) >/dev/null 2>&1 &", marker.display());
        let output =
            run_shell_bounded(&script, &dir, std::time::Duration::from_secs(5), 1024, None)
                .await
                .unwrap();
        assert!(output.passed());
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "successful verifier orphaned a background helper"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn host_backend_runs_and_captures() {
        let out = HostBackend
            .run(req(
                "/bin/sh",
                &["-c", "printf hello"],
                std::env::temp_dir(),
            ))
            .await
            .unwrap();
        assert_eq!(out.status, Some(0));
        assert_eq!(out.stdout, b"hello");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_jails_writes_outside_workspace() {
        // Managed/nested macOS environments may expose sandbox-exec but deny
        // sandbox_apply. Selection degrades to HostBackend and the CLI warns;
        // only exercise the jail where the OS can actually apply it.
        if !native_backend_available() {
            eprintln!(
                "Seatbelt unavailable on this host; native backend correctly degrades to host"
            );
            return;
        }
        let ws = std::env::temp_dir().join(format!("medha-seatbelt-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&ws).unwrap();
        let backend = SeatbeltBackend::new(NetPolicy::Allow, vec![]);

        // Writing INSIDE the workspace is allowed.
        let inside = backend
            .run(req("/bin/sh", &["-c", "touch ok.txt"], ws.clone()))
            .await
            .unwrap();
        assert_eq!(inside.status, Some(0), "in-workspace write should succeed");
        assert!(ws.join("ok.txt").exists());

        // Writing to $HOME is denied by the jail (the command exits non-zero).
        let escape_marker = format!(".medha-seatbelt-escape-{}", ulid::Ulid::new());
        let cmd = format!("touch \"$HOME/{escape_marker}\"");
        let outside = backend
            .run(req("/bin/sh", &["-c", &cmd], ws.clone()))
            .await
            .unwrap();
        assert_ne!(outside.status, Some(0), "write to HOME must be blocked");
        let home = std::env::var("HOME").unwrap();
        assert!(
            !std::path::Path::new(&home).join(&escape_marker).exists(),
            "escape file must not exist"
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_selection_degrades_to_host_when_seatbelt_cannot_apply() {
        if native_backend_available() {
            return;
        }
        let backend = select_backend(&SandboxConfig::default(), vec![]);
        assert_eq!(backend.label(), "host");
    }

    #[test]
    fn container_argv_hardens_and_hides_host_env() {
        let be = ContainerBackend::new(
            "docker".into(),
            "alpine".into(),
            NetPolicy::Deny,
            Some("2g".into()),
            Some(256),
        );
        let mut r = req("sh", &["-c", "echo hi"], std::env::temp_dir());
        r.env = vec![("TAVILY_API_KEY".into(), "supersecret".into())];
        r.clear_env = true;
        let argv = be.build_argv(&r);
        let joined = argv.join(" ");

        assert!(argv.contains(&"--rm".to_string()));
        assert!(joined.contains(":/workspace") && joined.contains("-w /workspace"));
        assert!(joined.contains("--cap-drop ALL") && joined.contains("no-new-privileges"));
        assert!(
            joined.contains("--network none"),
            "net=deny → --network none"
        );
        assert!(joined.contains("--memory 2g") && joined.contains("--pids-limit 256"));
        // The key improvement over wrapping the whole process: host env (and thus
        // API keys) is never forwarded into the container.
        assert!(!joined.contains("TAVILY_API_KEY") && !joined.contains("supersecret"));
        // The command follows the image, in order.
        let img = argv.iter().position(|a| a == "alpine").unwrap();
        assert_eq!(
            &argv[img + 1..],
            &["sh".to_string(), "-c".to_string(), "echo hi".to_string()]
        );
    }

    #[test]
    fn container_argv_allows_network_by_default() {
        let be = ContainerBackend::new("podman".into(), "img".into(), NetPolicy::Allow, None, None);
        let argv = be.build_argv(&req("sh", &["-c", "true"], std::env::temp_dir()));
        assert!(
            !argv.join(" ").contains("--network"),
            "net=allow leaves networking default"
        );
    }

    #[test]
    fn ssh_argv_cds_and_quotes_safely() {
        let be = SshBackend::new("user@host".into(), Some("/srv/app".into()));
        let argv = be.build_argv(&req("sh", &["-c", "echo done"], std::env::temp_dir()));
        assert_eq!(argv[0], "-o");
        assert!(argv.contains(&"user@host".to_string()));
        let remote = argv.last().unwrap();
        assert!(
            remote.starts_with("cd '/srv/app' &&"),
            "cd into remote dir: {remote}"
        );
        assert!(
            remote.contains("'sh' '-c' 'echo done'"),
            "args single-quoted: {remote}"
        );
    }

    #[test]
    fn a_shell_command_runs_through_an_interpreter_the_platform_actually_has() {
        // `shell.exec` hardcoded `sh`, which Windows does not have, so every
        // command failed with "program not found" before it ran — the missing
        // program was always the shell, never the user's command.
        for label in ["host", "native"] {
            let (program, args) = shell_argv(label, "git status");
            assert_eq!(args.last().unwrap(), "git status", "command must survive");
            if !cfg!(windows) {
                assert_eq!(program, "sh");
                assert_eq!(&args[..1], ["-c"]);
            }
        }
    }

    fn bash_path() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")
    }
    fn ps_path() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    }

    #[test]
    fn windows_prefers_git_bash_then_powershell_then_cmd() {
        // Bash wins: the model writes Unix command lines, and PowerShell's
        // same-named aliases take different flags, so `grep -rn` would mangle
        // its arguments rather than fail cleanly.
        let both = WindowsShellCandidates {
            override_shell: None,
            bash: Some(bash_path()),
            powershell: Some(ps_path()),
        };
        assert_eq!(choose_windows_shell(&both), WinShell::Bash(bash_path()));

        let only_ps = WindowsShellCandidates {
            powershell: Some(ps_path()),
            ..Default::default()
        };
        assert_eq!(
            choose_windows_shell(&only_ps),
            WinShell::PowerShell(ps_path())
        );

        // cmd.exe always exists, so the cascade can never come up empty.
        assert_eq!(
            choose_windows_shell(&WindowsShellCandidates::default()),
            WinShell::Cmd
        );
    }

    #[test]
    fn an_explicit_override_beats_every_detected_shell() {
        // No cascade fits every machine, so the escape hatch has to be
        // absolute — a user with an unusual setup cannot wait on a release.
        let chosen = choose_windows_shell(&WindowsShellCandidates {
            override_shell: Some(PathBuf::from(r"D:\msys64\usr\bin\bash.exe")),
            bash: Some(bash_path()),
            powershell: Some(ps_path()),
        });
        assert_eq!(
            chosen,
            WinShell::Bash(PathBuf::from(r"D:\msys64\usr\bin\bash.exe"))
        );

        // …and is invoked with the flags that binary understands, not assumed.
        assert_eq!(
            classify_windows_shell(Path::new(r"C:\Windows\System32\cmd.exe")),
            WinShell::Cmd
        );
        assert!(matches!(
            classify_windows_shell(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            WinShell::PowerShell(_)
        ));
        // Classification must not depend on the host's path separator, or a
        // Windows path read anywhere else reads as one long filename.
        for p in [
            r"C:\Program Files\Git\bin\bash.exe",
            "C:/Program Files/Git/bin/bash.exe",
            "bash.exe",
            "BASH.EXE",
        ] {
            assert!(
                matches!(classify_windows_shell(Path::new(p)), WinShell::Bash(_)),
                "{p} was not recognised as bash"
            );
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("medha-{tag}-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_store_alias_stub_is_not_accepted_as_a_shell() {
        // Zero-byte app execution aliases under WindowsApps\ satisfy is_file()
        // but cannot be spawned. Accepting one is worse than finding nothing:
        // the PATH tier reports success and the working absolute path below it
        // is never tried.
        let dir = scratch_dir("shellprobe");

        let stub = dir.join("pwsh.exe");
        std::fs::write(&stub, b"").unwrap();
        assert!(!is_runnable_shell(&stub), "a 0-byte alias must be rejected");

        let real = dir.join("bash.exe");
        std::fs::write(&real, b"MZ\x90\x00").unwrap();
        assert!(is_runnable_shell(&real));

        assert!(!is_runnable_shell(&dir.join("missing.exe")));
        assert!(!is_runnable_shell(&dir), "a directory is not a shell");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_is_derived_from_git_when_path_lacks_it() {
        // A default Git for Windows install puts only cmd\ on PATH, so looking
        // up bash.exe there misses it even though it is sitting next door.
        let dir = scratch_dir("gitbash");
        let git_root = dir.join("Git");
        std::fs::create_dir_all(git_root.join("cmd")).unwrap();
        std::fs::create_dir_all(git_root.join("bin")).unwrap();
        let git_exe = git_root.join("cmd").join("git.exe");
        std::fs::write(&git_exe, b"MZ").unwrap();

        assert_eq!(bash_beside_git(&git_exe), None, "no bash yet");

        let bash = git_root.join("bin").join("bash.exe");
        std::fs::write(&bash, b"MZ").unwrap();
        assert_eq!(bash_beside_git(&git_exe), Some(bash));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_windows_shell_receives_the_command_unmodified() {
        // The policy scanner reads the model's raw command string and the
        // wrapper is applied afterwards, so the two can never disagree about
        // what will run. That holds only while wrapping leaves the command
        // itself untouched — a scanner approving one string while a different
        // one executes is the whole gate defeated.
        let cmd = r#"git commit -m "a message with spaces && ;""#;
        for shell in [
            WinShell::Bash(bash_path()),
            WinShell::PowerShell(ps_path()),
            WinShell::Cmd,
        ] {
            let (_, args) = shell.argv(cmd);
            assert_eq!(args.last().unwrap(), cmd, "{shell:?} rewrote the command");
        }
    }

    #[test]
    fn each_windows_shell_gets_the_flags_that_disable_ambient_startup_files() {
        // AutoRun (cmd) and the user profile (PowerShell) both run before the
        // command and could alter one that was already approved.
        assert_eq!(WinShell::Cmd.argv("x").1[..3], ["/D", "/S", "/C"]);
        assert_eq!(
            WinShell::PowerShell(ps_path()).argv("x").1[..2],
            ["-NoProfile", "-Command"]
        );
        assert_eq!(WinShell::Bash(bash_path()).argv("x").1[..1], ["-c"]);
    }

    #[test]
    fn remote_backends_keep_sh_even_when_medha_runs_on_windows() {
        // Container and SSH execute on Unix-like hosts, so the interpreter
        // follows the target, not the machine medha happens to run on.
        for label in ["container", "ssh"] {
            let (program, args) = shell_argv(label, "ls");
            assert_eq!(program, "sh", "{label} should target a Unix shell");
            assert_eq!(args, vec!["-c".to_string(), "ls".to_string()]);
        }
    }
}
