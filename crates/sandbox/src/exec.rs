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
use permissions::ApprovedRoots;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// True when the retained stdout is only a tail of the complete stream.
    pub stdout_truncated: bool,
    /// True when the retained stderr is only a tail of the complete stream.
    pub stderr_truncated: bool,
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
        // Default: OS-native containment where available, with exfiltration
        // closed. Projects that genuinely need downloads opt in explicitly.
        Self {
            backend: BackendKind::Native,
            net: NetPolicy::Deny,
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

/// Fires a `SIGKILL` at a whole process group when dropped while still armed.
///
/// The owned supervisor keeps this guard until it has observed the leader exit
/// without reaping it, quiesced the group, reaped the leader, and joined both
/// output pumps. If the runtime aborts that supervisor, the guard is the final
/// synchronous backstop.
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
}

impl Drop for GroupReaper {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pid) = self.pid {
                quiesce_process_tree(pid);
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
    cmd: tokio::process::Command,
    limit: std::time::Duration,
    max_output: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ShellOutcome, ExecError> {
    // `max_output` is both the independent per-stream cap and the aggregate cap
    // for this combined-output API. Capture happens under the cap while the
    // process runs; it is never an after-the-fact truncation.
    let process = spawn_background_with_limits(cmd, max_output, max_output, max_output)?;
    let ended = match cancel {
        Some(token) => {
            let done = process.done_receiver();
            tokio::select! {
                finished = wait_done(done, limit) => {
                    if finished { Ok(()) } else { Err(false) }
                }
                _ = token.cancelled() => Err(true),
            }
        }
        None => {
            if process.wait_until(limit).await {
                Ok(())
            } else {
                Err(false)
            }
        }
    };

    if ended.is_err() {
        process.kill();
        process.wait().await;
    }
    let (stdout, stderr) = process.snapshot();
    let mut captured = Rolling::new(max_output);
    captured.push(stdout.as_bytes());
    if !stdout.is_empty() && !stderr.is_empty() {
        captured.push(b"\n");
    }
    captured.push(stderr.as_bytes());
    let text = captured.text();

    Ok(match ended {
        Ok(()) => ShellOutcome {
            status: process.exit_code(),
            output: text,
            timed_out: false,
            cancelled: false,
        },
        Err(cancelled) => ShellOutcome {
            status: None,
            output: format!(
                "{}\n[{}]",
                text,
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

/// Independent and aggregate foreground/background capture ceilings. The
/// aggregate is lower than the sum of the two independent ceilings so a child
/// flooding both descriptors cannot double the memory budget.
const EXEC_STDOUT_CAP: usize = 1_000_000;
const EXEC_STDERR_CAP: usize = 1_000_000;
const EXEC_AGGREGATE_CAP: usize = 1_500_000;

#[derive(Clone, Copy)]
enum CapturedStream {
    Stdout,
    Stderr,
}

/// A fixed-memory tail buffer. `VecDeque` avoids repeatedly moving a megabyte
/// of retained output for every 8 KiB read after the cap has been reached.
struct TailBuf {
    data: VecDeque<u8>,
    cap: usize,
    truncated: bool,
}

impl TailBuf {
    fn new(cap: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            self.truncated |= !bytes.is_empty();
            return;
        }
        if bytes.len() >= self.cap {
            self.data.clear();
            self.data.extend(
                bytes[bytes.len().saturating_sub(self.cap)..]
                    .iter()
                    .copied(),
            );
            self.truncated = true;
            return;
        }
        self.data.extend(bytes.iter().copied());
        if self.data.len() > self.cap {
            self.discard_oldest(self.data.len() - self.cap);
        }
    }

    fn discard_oldest(&mut self, amount: usize) {
        let amount = amount.min(self.data.len());
        if amount != 0 {
            self.data.drain(..amount);
            self.truncated = true;
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }

    fn text(&self) -> String {
        let bytes = self.bytes();
        let body = String::from_utf8_lossy(&bytes);
        if self.truncated {
            format!("[…earlier output dropped…]\n{body}")
        } else {
            body.into_owned()
        }
    }
}

/// Both streams live behind one lock so the aggregate ceiling is enforced at
/// the moment bytes are retained, not after an unbounded capture has completed.
struct CapturePair {
    stdout: TailBuf,
    stderr: TailBuf,
    aggregate_cap: usize,
}

impl CapturePair {
    fn new(stdout_cap: usize, stderr_cap: usize, aggregate_cap: usize) -> Self {
        Self {
            stdout: TailBuf::new(stdout_cap),
            stderr: TailBuf::new(stderr_cap),
            aggregate_cap,
        }
    }

    fn push(&mut self, stream: CapturedStream, bytes: &[u8]) {
        match stream {
            CapturedStream::Stdout => self.stdout.push(bytes),
            CapturedStream::Stderr => self.stderr.push(bytes),
        }
        let excess = self
            .stdout
            .data
            .len()
            .saturating_add(self.stderr.data.len())
            .saturating_sub(self.aggregate_cap);
        if excess == 0 {
            return;
        }
        // Preserve useful tails from both streams: trim the currently larger
        // retained stream first, then the other if one alone was insufficient.
        let stdout_first = self.stdout.data.len() >= self.stderr.data.len();
        let first_len = if stdout_first {
            self.stdout.data.len()
        } else {
            self.stderr.data.len()
        };
        let first_drop = excess.min(first_len);
        if stdout_first {
            self.stdout.discard_oldest(first_drop);
            self.stderr.discard_oldest(excess - first_drop);
        } else {
            self.stderr.discard_oldest(first_drop);
            self.stdout.discard_oldest(excess - first_drop);
        }
    }
}

type SharedCapture = std::sync::Arc<std::sync::Mutex<CapturePair>>;

/// An owned command task: stdout/stderr stream into rolling buffers while it
/// runs, and it can be polled, awaited, or killed as a whole process group.
/// `shell.exec` uses this ownership even for foreground runs so cancellation
/// has a synchronous process-tree kill handle before its future is dropped.
pub struct BgProc {
    pub pid: Option<u32>,
    capture: SharedCapture,
    done_rx: tokio::sync::watch::Receiver<bool>,
    code: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
}

impl BgProc {
    /// Current buffered stdout / stderr (tails, with a marker if truncated).
    pub fn snapshot(&self) -> (String, String) {
        self.capture
            .lock()
            .map(|capture| (capture.stdout.text(), capture.stderr.text()))
            .unwrap_or_default()
    }
    /// Whether either returned stream is a bounded tail rather than complete.
    pub fn truncation(&self) -> (bool, bool) {
        self.capture
            .lock()
            .map(|capture| (capture.stdout.truncated, capture.stderr.truncated))
            .unwrap_or_default()
    }
    fn raw_snapshot(&self) -> (Vec<u8>, Vec<u8>, bool, bool) {
        self.capture
            .lock()
            .map(|capture| {
                (
                    capture.stdout.bytes(),
                    capture.stderr.bytes(),
                    capture.stdout.truncated,
                    capture.stderr.truncated,
                )
            })
            .unwrap_or_default()
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
    /// Wait without a deadline for the child waiter to confirm settlement.
    /// Callers use this only after sending an unconditional tree kill.
    pub async fn wait(&self) {
        wait_done_unbounded(self.done_rx.clone()).await;
    }
    /// SIGKILL the whole process group.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            quiesce_process_tree(pid);
        }
    }
}

impl Drop for BgProc {
    fn drop(&mut self) {
        if self.is_running() {
            self.kill();
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
                return *rx.borrow();
            }
        }
        true
    })
    .await
    .unwrap_or(false)
}

async fn wait_done_unbounded(mut rx: tokio::sync::watch::Receiver<bool>) -> bool {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return *rx.borrow();
        }
    }
    true
}

#[cfg(unix)]
fn leader_exited_without_reap(pid: u32) -> std::io::Result<bool> {
    // WNOWAIT is the crucial part: it lets the supervisor observe exit while
    // the zombie leader continues to reserve the process-group id. Descendants
    // therefore cannot race a recycled id before teardown.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == 0 {
        Ok(unsafe { info.si_pid() } != 0)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

trait PumpPipe: std::io::Read {
    fn make_nonblocking(&self);
}

macro_rules! impl_pump_pipe {
    ($pipe:ty) => {
        impl PumpPipe for $pipe {
            fn make_nonblocking(&self) {
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let fd = self.as_raw_fd();
                    unsafe {
                        let flags = libc::fcntl(fd, libc::F_GETFL);
                        if flags >= 0 {
                            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                        }
                    }
                }
            }
        }
    };
}

impl_pump_pipe!(std::process::ChildStdout);
impl_pump_pipe!(std::process::ChildStderr);

#[cfg(target_os = "macos")]
fn process_snapshot() -> Vec<(i32, i32, i32)> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; count as usize + 64];
    let listed = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(pids.as_slice()) as i32,
        )
    };
    if listed <= 0 {
        return Vec::new();
    }
    pids.truncate((listed as usize).min(pids.len()));
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            (read == size).then_some((pid, info.pbi_ppid as i32, info.pbi_pgid as i32))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_snapshot() -> Vec<(i32, i32, i32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // `comm` may contain spaces and parentheses. Everything after its
            // final ')' starts at field 3: state, ppid, process-group.
            let tail = stat.get(stat.rfind(')')? + 1..)?.trim();
            let mut fields = tail.split_whitespace();
            fields.next()?; // state
            let ppid = fields.next()?.parse::<i32>().ok()?;
            let pgid = fields.next()?.parse::<i32>().ok()?;
            Some((pid, ppid, pgid))
        })
        .collect()
}

// Only the Unix group-kill path reads this; Windows has no process groups to
// enumerate, so defining it there is dead code.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_snapshot() -> Vec<(i32, i32, i32)> {
    Vec::new()
}

/// Snapshot both the process group and descendants, including descendants that
/// created a nested group. The root is still alive (or an unreaped zombie) when
/// this runs, so parent links have not yet been lost to orphan reparenting.
#[cfg(unix)]
fn process_tree_members(root: u32) -> Vec<(i32, i32)> {
    let snapshot = process_snapshot();
    let root = root as i32;
    let mut selected = std::collections::HashSet::from([root]);
    loop {
        let before = selected.len();
        for (pid, parent, group) in &snapshot {
            if *group == root || selected.contains(parent) {
                selected.insert(*pid);
            }
        }
        if selected.len() == before {
            break;
        }
    }
    snapshot
        .into_iter()
        .filter(|(pid, _, _)| *pid != root && selected.contains(pid))
        .map(|(pid, parent, _)| (pid, parent))
        .collect()
}

/// Stop the leader first so it cannot launch another command, snapshot and
/// signal every known descendant parent-first, then signal the original group
/// as a whole. This ordering avoids waking a shell after only its `sleep`
/// child was killed and also closes cancellation races before the supervisor
/// can observe and reap the leader.
#[cfg(unix)]
fn kill_group_parent_first(group: u32) {
    unsafe {
        libc::kill(group as i32, libc::SIGSTOP);
    }
    std::thread::sleep(std::time::Duration::from_millis(1));
    let mut members = process_tree_members(group);
    let snapshot = members.clone();
    let depth = |pid: i32| {
        let mut current = pid;
        let mut depth = 0usize;
        for _ in 0..snapshot.len() {
            let Some((_, parent)) = snapshot.iter().find(|(candidate, _)| *candidate == current)
            else {
                break;
            };
            depth += 1;
            if *parent == group as i32 {
                break;
            }
            current = *parent;
        }
        depth
    };
    members.sort_by_key(|(pid, _)| depth(*pid));
    unsafe {
        libc::kill(group as i32, libc::SIGKILL);
    }
    for (pid, _) in members {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    kill_process_tree(group);
}

/// Signal more than once while the unreaped leader pins the group id. Once
/// `waitid(WNOWAIT)` reports the leader dead it cannot create another child,
/// but an already-running descendant can be concurrent with the first signal.
fn quiesce_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        for delay_ms in [0, 2, 5, 10, 20] {
            if delay_ms != 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            kill_group_parent_first(pid);
        }
    }
    #[cfg(not(unix))]
    kill_process_tree(pid);
}

/// Spawn an owned command task with bounded output and an independent lifecycle
/// supervisor. Completion means: leader exit observed, process group quiesced
/// while its id was still pinned, leader reaped, and both pipe pumps joined.
fn spawn_background_with_limits(
    mut cmd: tokio::process::Command,
    stdout_cap: usize,
    stderr_cap: usize,
    aggregate_cap: usize,
) -> Result<BgProc, ExecError> {
    use std::sync::{Arc, Mutex};
    configure_for_spawn(&mut cmd, false);
    // Keep a std Child rather than a tokio Child. The leader must remain
    // unreaped (and therefore keep its PID reserved) until we have killed any
    // helpers left in its process group. Reaping first makes a later group kill
    // race PID reuse and, on Windows, loses taskkill's parent-tree anchor.
    let mut child = cmd
        .as_std_mut()
        .spawn()
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    let pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let capture: SharedCapture = Arc::new(Mutex::new(CapturePair::new(
        stdout_cap,
        stderr_cap,
        aggregate_cap,
    )));
    let code = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let stop_pumps = Arc::new(AtomicBool::new(false));
    let (status_tx, status_rx) = tokio::sync::oneshot::channel();

    fn pump<R: PumpPipe>(
        mut pipe: R,
        capture: SharedCapture,
        stream: CapturedStream,
        stop: std::sync::Arc<AtomicBool>,
    ) {
        pipe.make_nonblocking();
        let mut chunk = [0u8; 8192];
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut pair) = capture.lock() {
                        pair.push(stream, &chunk[..n]);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }
    let out_handle = out_pipe.map(|out| {
        let capture = capture.clone();
        let stop = stop_pumps.clone();
        tokio::task::spawn_blocking(move || {
            pump(out, capture, CapturedStream::Stdout, stop);
        })
    });
    let err_handle = err_pipe.map(|err| {
        let capture = capture.clone();
        let stop = stop_pumps.clone();
        tokio::task::spawn_blocking(move || {
            pump(err, capture, CapturedStream::Stderr, stop);
        })
    });
    // Lifecycle observation must not compete with the async runtime that is
    // executing the command's caller. Under high fan-out a runtime worker can
    // be starved long enough for a freshly orphaned helper to perform work
    // before an async poll notices the leader died. A small-stack native
    // supervisor begins monitoring immediately and owns the unreaped child.
    let child_slot = Arc::new(Mutex::new(Some(child)));
    let supervisor_child = child_slot.clone();
    let supervisor = std::thread::Builder::new()
        .name(format!("medha-proc-{pid}"))
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut child = supervisor_child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("supervisor owns child");
            let mut reaper = GroupReaper::new(Some(pid));
            let pre_reaped_status = loop {
                #[cfg(unix)]
                {
                    match leader_exited_without_reap(pid) {
                        Ok(true) => break None,
                        Ok(false) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        // Conservative portability fallback: `try_wait` can
                        // reap the leader, so use it only if WNOWAIT failed.
                        Err(_) => match child.try_wait() {
                            Ok(Some(status)) => break Some(status),
                            Ok(None) => {}
                            Err(_) => break None,
                        },
                    }
                }
                #[cfg(not(unix))]
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            };

            // Do not infer process completion from pipe EOF. Redirected helpers
            // can close both pipes before the group leader has finished.
            quiesce_process_tree(pid);
            let status = match pre_reaped_status {
                Some(status) => Some(status),
                None => child.wait().ok(),
            };
            reaper.disarm();
            let _ = status_tx.send(status.and_then(|status| status.code()));
        });
    if let Err(error) = supervisor {
        quiesce_process_tree(pid);
        if let Some(mut child) = child_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = child.wait();
        }
        stop_pumps.store(true, Ordering::Release);
        return Err(ExecError::Spawn(format!(
            "failed to start process supervisor: {error}"
        )));
    }

    let code2 = code.clone();
    tokio::spawn(async move {
        let status = status_rx.await.unwrap_or(None);

        // A killed group should close both pipes promptly. If an escaped fd
        // holder does not, ask the nonblocking pumps to close their descriptors
        // and then explicitly join them before publishing `done`.
        let joins = async {
            if let Some(handle) = out_handle {
                let _ = handle.await;
            }
            if let Some(handle) = err_handle {
                let _ = handle.await;
            }
        };
        tokio::pin!(joins);
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut joins)
            .await
            .is_err()
        {
            stop_pumps.store(true, Ordering::Release);
            joins.await;
        }
        if let Ok(mut c) = code2.lock() {
            *c = status;
        }
        let _ = done_tx.send(true);
    });

    Ok(BgProc {
        pid: Some(pid),
        capture,
        done_rx,
        code,
    })
}

/// Spawn an owned command task using the standard independent and aggregate
/// output ceilings.
pub fn spawn_background(cmd: tokio::process::Command) -> Result<BgProc, ExecError> {
    spawn_background_with_limits(cmd, EXEC_STDOUT_CAP, EXEC_STDERR_CAP, EXEC_AGGREGATE_CAP)
}

/// Spawn, supervise, and capture a foreground command using fixed-memory
/// rolling tails. Dropping this future drops its `BgProc`, which immediately
/// signals the group; the detached owner still reaps and joins every resource.
async fn spawn_and_wait(cmd: tokio::process::Command) -> Result<ExecOutput, ExecError> {
    let process = spawn_background(cmd)?;
    process.wait().await;
    let (stdout, stderr, stdout_truncated, stderr_truncated) = process.raw_snapshot();
    Ok(ExecOutput {
        status: process.exit_code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
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

/// A credential-free HOME/TMP tree for native-sandbox children. It is separate
/// from the process HOME even when a caller passes the latter in `ExecRequest`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct IsolatedHome {
    path: PathBuf,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl IsolatedHome {
    /// The one isolated HOME for this process, shared by every native backend.
    ///
    /// Shared deliberately, and never dropped while the process lives: a
    /// backend can be dropped seconds after it spawns a long-lived child
    /// (LSP/MCP servers), and a per-backend home died with its backend — the
    /// still-running server's toolchain proxies then found `$HOME/.rustup`
    /// missing, tried to recreate it under the system temp dir, and the jail
    /// correctly denied that write, so every `cargo`/`rustc` invocation
    /// failed and workspaces silently never loaded.
    fn shared() -> std::sync::Arc<Self> {
        static SHARED: std::sync::OnceLock<std::sync::Arc<IsolatedHome>> =
            std::sync::OnceLock::new();
        std::sync::Arc::clone(SHARED.get_or_init(Self::new))
    }

    fn new() -> std::sync::Arc<Self> {
        let requested = std::env::temp_dir().join(format!(
            "medha-native-home-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let _ = std::fs::create_dir_all(&requested);
        let path = requested.canonicalize().unwrap_or(requested);
        let home = std::sync::Arc::new(Self { path });
        home.prepare();
        home
    }

    fn prepare(&self) {
        for relative in [
            "",
            "tmp",
            ".cache",
            ".config",
            ".local/share",
            ".cargo",
            ".rustup",
            ".npm",
        ] {
            let _ = std::fs::create_dir_all(self.path.join(relative));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700));
            if let Some(real_home) = home_dir_from_env() {
                // Read-only toolchain payloads remain usable. Native policies
                // grant their real targets read/execute, never write; credential
                // files such as ~/.cargo/credentials are not linked or allowed.
                for (source, destination) in [
                    (
                        real_home.join(".cargo/registry"),
                        self.path.join(".cargo/registry"),
                    ),
                    (real_home.join(".cargo/git"), self.path.join(".cargo/git")),
                    (
                        real_home.join(".rustup/toolchains"),
                        self.path.join(".rustup/toolchains"),
                    ),
                    (
                        real_home.join(".rustup/update-hashes"),
                        self.path.join(".rustup/update-hashes"),
                    ),
                    (
                        real_home.join(".rustup/settings.toml"),
                        self.path.join(".rustup/settings.toml"),
                    ),
                ] {
                    if source.exists() && !destination.exists() {
                        let _ = symlink(source, destination);
                    }
                }
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // Exact generated child beneath the system temp directory; no glob or
        // user-controlled path participates in cleanup.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn home_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.canonicalize().unwrap_or(path))
}

fn native_sensitive_paths() -> Vec<PathBuf> {
    let Some(home) = home_dir_from_env() else {
        return Vec::new();
    };
    [
        ".ssh",
        ".aws",
        ".azure",
        ".medha",
        ".gnupg",
        ".docker",
        ".kube",
        ".config/gcloud",
        ".config/gh",
        ".config/pip",
        ".config/pnpm",
        ".git-credentials",
        ".gitconfig",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".yarnrc",
        ".yarnrc.yml",
        ".gem/credentials",
        ".gradle/gradle.properties",
        ".m2/settings.xml",
        ".nuget/NuGet.Config",
        ".bash_history",
        ".zsh_history",
        ".python_history",
        ".node_repl_history",
        ".local/share/fish/fish_history",
        ".cargo/credentials",
        ".cargo/credentials.toml",
    ]
    .iter()
    .map(|relative| home.join(relative))
    .collect()
}

/// Resolve an absolute policy path through its deepest existing ancestor.
///
/// `canonicalize()` alone is insufficient for write roots that have not been
/// created yet, while a lexical-only comparison misses aliases such as a
/// symlink to `~/.ssh`. Combining both keeps future paths usable and makes
/// security comparisons against their physical parent identity.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_native_policy_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
    let mut resolved = ancestor.canonicalize().ok()?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn safe_extra_writable_against(paths: &[PathBuf], sensitive: &[PathBuf]) -> Vec<PathBuf> {
    let sensitive: Vec<PathBuf> = sensitive
        .iter()
        .filter_map(|path| resolve_native_policy_path(path))
        .collect();
    paths
        .iter()
        .filter_map(|path| resolve_native_policy_path(path))
        .filter(|path| {
            path.parent().is_some()
                && !sensitive
                    .iter()
                    .any(|secret| secret.starts_with(path) || path.starts_with(secret))
        })
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn safe_extra_writable(paths: &[PathBuf]) -> Vec<PathBuf> {
    safe_extra_writable_against(paths, &native_sensitive_paths())
}

/// Absolute-path tokens in a line of tool output or an argv entry. Utilities
/// report denials as `prog: /path: message`, so split on the separators that
/// bound a path and keep what still looks absolute.
fn absolute_path_tokens(text: &str) -> impl Iterator<Item = PathBuf> + '_ {
    text.split([' ', '\t', ':', '\'', '"', '`'])
        .map(str::trim)
        .filter(|token| token.len() > 1 && token.starts_with('/'))
        .map(PathBuf::from)
}

/// Out-of-workspace roots a failed sandboxed command was plausibly denied on —
/// the input to the exec escalation prompt. Denial lines in stderr name the
/// actual target, so they are preferred; argv is the fallback. Files widen to
/// their parent directory (one approval covers the sibling files the same task
/// touches next), credential paths are never offered, and already-approved
/// roots are excluded because they cannot be the cause.
pub(crate) fn escalation_candidates(
    output: &ExecOutput,
    args: &[String],
    workspace: &Path,
    approved: &ApprovedRoots,
) -> Vec<PathBuf> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let denial_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            line.contains("Operation not permitted") || line.contains("Permission denied")
        })
        .collect();
    if denial_lines.is_empty() {
        return Vec::new();
    }
    let mut tokens: Vec<PathBuf> = denial_lines
        .iter()
        .flat_map(|line| absolute_path_tokens(line))
        .collect();
    if tokens.is_empty() {
        tokens = args
            .iter()
            .flat_map(|arg| absolute_path_tokens(arg))
            .collect();
    }
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let sensitive = native_sensitive_paths();
    let mut candidates = Vec::new();
    for token in tokens {
        let Ok(resolved) = token.canonicalize() else {
            continue;
        };
        let root = if resolved.is_file() {
            match resolved.parent() {
                Some(parent) => parent.to_path_buf(),
                None => continue,
            }
        } else {
            resolved
        };
        if root.starts_with(&workspace)
            || approved.is_allowed(&root, permissions::PermissionType::Read)
            || sensitive
                .iter()
                .any(|secret| secret.starts_with(&root) || root.starts_with(secret))
            || candidates.contains(&root)
        {
            continue;
        }
        candidates.push(root);
    }
    candidates
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn workspace_contains_native_credentials(workspace: &Path) -> bool {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    native_sensitive_paths()
        .iter()
        .any(|secret| secret.starts_with(&workspace))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn apply_isolated_environment(cmd: &mut tokio::process::Command, home: &IsolatedHome) {
    let home_path = &home.path;
    cmd.env("HOME", home_path)
        .env("TMPDIR", home_path.join("tmp"))
        .env("TMP", home_path.join("tmp"))
        .env("TEMP", home_path.join("tmp"))
        .env("XDG_CACHE_HOME", home_path.join(".cache"))
        .env("XDG_CONFIG_HOME", home_path.join(".config"))
        .env("XDG_DATA_HOME", home_path.join(".local/share"))
        .env("CARGO_HOME", home_path.join(".cargo"))
        .env("RUSTUP_HOME", home_path.join(".rustup"))
        .env("NPM_CONFIG_USERCONFIG", home_path.join(".npmrc"))
        .env("NPM_CONFIG_CACHE", home_path.join(".npm"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_toolchain_read_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home_dir_from_env() {
        for relative in [
            ".cargo/bin",
            ".cargo/registry",
            ".cargo/git",
            ".rustup/toolchains",
            ".rustup/update-hashes",
            ".rustup/settings.toml",
            ".npm/_cacache",
            ".gradle/caches",
            ".m2/repository",
            "go/pkg",
        ] {
            let path = home.join(relative);
            if path.exists() {
                roots.push(path);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        let sensitive = native_sensitive_paths();
        roots.extend(std::env::split_paths(&path).filter(|directory| {
            directory.is_absolute()
                && !sensitive
                    .iter()
                    .any(|secret| secret.starts_with(directory) || directory.starts_with(secret))
        }));
    }
    roots
}

/// Escape a path for embedding in an SBPL string literal (macOS Seatbelt only).
#[cfg(target_os = "macos")]
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// macOS Seatbelt backend: confines the command with `sandbox-exec` and a
/// generated SBPL profile.
///
/// Non-filesystem syscalls remain compatible, while file reads and writes are
/// deny-by-default and reopened only for the workspace, an isolated HOME/TMP,
/// system runtimes, and narrowly selected read-only toolchain payloads.
#[cfg(target_os = "macos")]
pub struct SeatbeltBackend {
    net: NetPolicy,
    extra_writable: Vec<PathBuf>,
    approved: ApprovedRoots,
    home: std::sync::Arc<IsolatedHome>,
}

#[cfg(target_os = "macos")]
impl SeatbeltBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>, approved: ApprovedRoots) -> Self {
        Self {
            net,
            extra_writable: safe_extra_writable(&extra_writable),
            approved,
            home: IsolatedHome::shared(),
        }
    }

    fn readable_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut readable = vec![
            ws,
            self.home.path.clone(),
            PathBuf::from("/System"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/opt/homebrew"),
            PathBuf::from("/usr/local"),
            PathBuf::from("/private/etc/ssl"),
            // `/bin/sh` resolves its real interpreter through this indirection;
            // without it every command spews "Operation not permitted" noise.
            PathBuf::from("/private/var/select"),
            // Apple developer toolchain: /usr/bin/clang etc. are shims that
            // re-exec the selected toolchain through this link and root.
            PathBuf::from("/private/var/db/xcode_select_link"),
            PathBuf::from("/Library/Developer"),
            PathBuf::from("/Applications/Xcode.app"),
        ];
        readable.extend(native_toolchain_read_roots());
        readable.extend(self.extra_writable.iter().cloned());
        // Live user approvals: a write grant implies read, or editing under it
        // would be impossible. Sensitive-path denies appended later still win.
        readable.extend(self.approved.read_roots());
        readable.extend(self.approved.write_roots());
        readable.sort();
        readable.dedup();
        readable
    }

    fn profile(&self, cwd: &std::path::Path) -> String {
        // Canonicalize so the subpath match survives /var → /private/var etc.
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut writable: Vec<PathBuf> = vec![ws, self.home.path.clone()];
        writable.extend(self.extra_writable.iter().cloned());
        writable.extend(safe_extra_writable(&self.approved.write_roots()));
        writable.sort();
        writable.dedup();

        let mut p = String::from(
            "(version 1)\n(allow default)\n\
             (deny file-read*)\n(allow file-read*\n",
        );
        for path in self.readable_paths(cwd) {
            let filter = if path.is_file() { "literal" } else { "subpath" };
            p.push_str(&format!(
                "    ({filter} \"{}\")\n",
                sbpl_escape(&path.to_string_lossy())
            ));
        }
        for device in [
            "/dev/null",
            "/dev/zero",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
        ] {
            p.push_str(&format!("    (literal \"{device}\")\n"));
        }
        p.push_str(")\n");
        // Deny-by-default reads still require path *traversal*: the kernel
        // stats every component during resolution, and a subpath rule does not
        // cover the root inode or the ancestors of an allowed root. Grant the
        // literal root plus directory/symlink metadata (stat/lstat and link
        // resolution — /var, /tmp and /etc are symlinks on macOS — but not
        // readdir or file contents) so resolution works while directory
        // listings stay denied.
        p.push_str(
            "(allow file-read* (literal \"/\"))\n\
             (allow file-read-metadata (vnode-type DIRECTORY) (vnode-type SYMLINK))\n",
        );
        p.push_str("(deny file-write*)\n(allow file-write*\n");
        for w in &writable {
            p.push_str(&format!(
                "    (subpath \"{}\")\n",
                sbpl_escape(&w.to_string_lossy())
            ));
        }
        for device in ["/dev/null", "/dev/zero", "/dev/tty"] {
            p.push_str(&format!("    (literal \"{device}\")\n"));
        }
        p.push_str(")\n");
        // A workspace that is nested near HOME cannot accidentally broaden a
        // more-specific credential path through the workspace subpath rule.
        for secret in native_sensitive_paths() {
            let secret = sbpl_escape(&secret.to_string_lossy());
            p.push_str(&format!(
                "(deny file-read* (literal \"{secret}\") (subpath \"{secret}\"))\n\
                 (deny file-write* (literal \"{secret}\") (subpath \"{secret}\"))\n"
            ));
        }
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
        if workspace_contains_native_credentials(&req.cwd) {
            return Err(ExecError::Unavailable(
                "native sandbox workspace contains host credential directories; choose a narrower workspace"
                    .into(),
            ));
        }
        let profile = self.profile(&req.cwd);
        // sandbox-exec -p <profile> <program> <args...>
        let mut wrapped = Vec::with_capacity(req.args.len() + 3);
        wrapped.push("-p".to_string());
        wrapped.push(profile);
        wrapped.push(req.program.clone());
        wrapped.extend(req.args.iter().cloned());
        let mut command = base_command("/usr/bin/sandbox-exec", &wrapped, req);
        apply_isolated_environment(&mut command, &self.home);
        Ok(command)
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
/// the agent. Reads and writes are both allowlisted. The ruleset is built in
/// the parent — only the (allocation-free) `restrict_self` syscall runs in the
/// post-fork child, which is the safe pattern in a threaded runtime.
#[cfg(target_os = "linux")]
pub struct LandlockBackend {
    net: NetPolicy,
    extra_writable: Vec<PathBuf>,
    approved: ApprovedRoots,
    home: std::sync::Arc<IsolatedHome>,
}

#[cfg(target_os = "linux")]
impl LandlockBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>, approved: ApprovedRoots) -> Self {
        Self {
            net,
            extra_writable: safe_extra_writable(&extra_writable),
            approved,
            home: IsolatedHome::shared(),
        }
    }

    fn writable_paths(&self, cwd: &std::path::Path) -> Vec<PathBuf> {
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut v = vec![ws, self.home.path.clone()];
        v.extend(self.extra_writable.iter().cloned());
        v.extend(safe_extra_writable(&self.approved.write_roots()));
        // Common shell redirections need a sink, but granting all of `/dev`
        // would expose unrelated devices. A file-scoped Landlock rule is
        // added for this exact node.
        if Path::new("/dev/null").exists() {
            v.push(PathBuf::from("/dev/null"));
        }
        v.sort();
        v.dedup();
        v
    }

    fn readable_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        let ws = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut paths = vec![
            ws,
            self.home.path.clone(),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/nix/store"),
            PathBuf::from("/run/current-system/sw"),
            PathBuf::from("/etc/ssl"),
            PathBuf::from("/etc/ca-certificates"),
            // Toolchains locate themselves through procfs: rustc resolves its
            // sysroot from /proc/self/exe, so without this `cargo metadata`
            // fails, no workspace loads, and every Rust/Go/Python toolchain
            // degrades silently. /sys carries the cgroup limits runtimes size
            // their pools from.
            //
            // CAVEAT: procfs is process-wide, so a sandboxed child can read
            // /proc/<pid>/environ of other same-UID processes — including this
            // agent's own API keys. Landlock is allowlist-only and hierarchical,
            // so a narrower grant is not expressible. This is strictly tighter
            // than the read-everything policy it replaced, and the credential
            // *files* AUD-006 targets stay denied, but closing the environ path
            // needs either PR_SET_DUMPABLE on the agent or a PID namespace with
            // a private /proc (the bubblewrap direction noted in AUD-006).
            PathBuf::from("/proc"),
            PathBuf::from("/sys"),
        ];
        for file in [
            "/etc/ld.so.cache",
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/localtime",
            "/etc/passwd",
            "/etc/group",
            "/dev/null",
            "/dev/zero",
            "/dev/random",
            "/dev/urandom",
        ] {
            if Path::new(file).exists() {
                paths.push(PathBuf::from(file));
            }
        }
        paths.extend(native_toolchain_read_roots());
        paths.extend(self.extra_writable.iter().cloned());
        paths.extend(self.approved.read_roots());
        paths.extend(self.approved.write_roots());
        paths.retain(|path| path.exists());
        paths.sort();
        paths.dedup();
        paths
    }
}

/// Build a deny-by-default Landlock ruleset in the parent. Failure is closed:
/// selecting a native jail must never silently turn into host execution.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(
    readable: &[PathBuf],
    writable: &[PathBuf],
    net: NetPolicy,
) -> Result<landlock::RulesetCreated, ExecError> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr,
    };
    let abi = ABI::V5;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|error| ExecError::Unavailable(error.to_string()))?;
    // Deny network by *handling* net access and then adding no net rules — with
    // Landlock, a handled access with no matching rule is denied. Best-effort:
    // silently a no-op on kernels < 6.7 (Landlock ABI < v4), so it never breaks
    // the run; enforcement is real only where the kernel supports it.
    if net == NetPolicy::Deny {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|error| ExecError::Unavailable(error.to_string()))?;
    }
    let mut created = ruleset
        .create()
        .map_err(|error| ExecError::Unavailable(error.to_string()))?;
    for path in readable {
        let fd = PathFd::new(path).map_err(|error| {
            ExecError::Unavailable(format!(
                "cannot authorize native read root {}: {error}",
                path.display()
            ))
        })?;
        let metadata = std::fs::metadata(path).map_err(|error| {
            ExecError::Unavailable(format!(
                "cannot inspect native read root {}: {error}",
                path.display()
            ))
        })?;
        let access = if metadata.is_dir() {
            AccessFs::from_read(abi)
        } else {
            AccessFs::from_read(abi) & AccessFs::from_file(abi)
        };
        created = created
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|error| ExecError::Unavailable(error.to_string()))?;
    }
    for p in writable {
        let fd = PathFd::new(p).map_err(|error| {
            ExecError::Unavailable(format!(
                "cannot authorize native write root {}: {error}",
                p.display()
            ))
        })?;
        let metadata = std::fs::metadata(p).map_err(|error| {
            ExecError::Unavailable(format!(
                "cannot inspect native write root {}: {error}",
                p.display()
            ))
        })?;
        let access = if metadata.is_dir() {
            AccessFs::from_all(abi)
        } else {
            AccessFs::from_file(abi)
        };
        created = created
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|error| ExecError::Unavailable(error.to_string()))?;
    }
    Ok(created)
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ExecBackend for LandlockBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        use std::os::unix::process::CommandExt;

        if workspace_contains_native_credentials(&req.cwd) {
            return Err(ExecError::Unavailable(
                "native sandbox workspace contains host credential directories; choose a narrower workspace"
                    .into(),
            ));
        }
        let ruleset = build_landlock_ruleset(
            &self.readable_paths(&req.cwd),
            &self.writable_paths(&req.cwd),
            self.net,
        )?;

        let mut cmd = std::process::Command::new(&req.program);
        cmd.args(&req.args).current_dir(&req.cwd);
        if req.clear_env {
            cmd.env_clear();
        }
        cmd.envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        // Apply Landlock in the child (post-fork, pre-exec): only restrict_self
        // runs here — no allocation, so it's safe under the threaded runtime.
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

        let mut command = tokio::process::Command::from(cmd);
        apply_isolated_environment(&mut command, &self.home);
        Ok(command)
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
    /// Stronger posture for repository-authored verification: never pull,
    /// ignore an image-provided entrypoint, and make the image root read-only.
    hermetic: bool,
    /// Operator-independent name used by Gate to force-remove the daemon-owned
    /// workload if the attached runtime client is timed out or cancelled.
    container_name: Option<String>,
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
            hermetic: false,
            container_name: None,
        }
    }

    /// A fail-closed container for repository-authored checks. Unlike the
    /// interactive container backend, this never causes an implicit registry
    /// fetch and does not run an image-controlled entrypoint before `program`.
    pub fn new_hermetic(
        runtime: String,
        image: String,
        memory: Option<String>,
        pids: Option<u32>,
        container_name: String,
    ) -> Self {
        Self {
            runtime,
            image,
            net: NetPolicy::Deny,
            memory,
            pids,
            hermetic: true,
            container_name: Some(container_name),
        }
    }

    /// Options shared by interactive `run` and hermetic `create`.
    fn isolation_argv(&self, req: &ExecRequest) -> Vec<String> {
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
        let mut a = vec![
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
        a
    }

    /// Build the interactive `run …` argv. Pure, for testing.
    fn build_run_argv(&self, req: &ExecRequest) -> Vec<String> {
        debug_assert!(!self.hermetic);
        let mut a = vec!["run".into(), "--rm".into()];
        a.extend(self.isolation_argv(req));
        // Host env is intentionally NOT forwarded (no `--env`): API keys stay on
        // the host and never reach the containerized command.
        a.push(self.image.clone());
        a.push(req.program.clone());
        a.extend(req.args.iter().cloned());
        a
    }

    /// Build only the inert registration phase for a hermetic check.
    ///
    /// `create` applies every isolation option and records the unique name, but
    /// it never starts the image entrypoint or repository code. Keeping this
    /// separate from `start` lets Gate finish registration before deciding
    /// whether cancellation permits the workload to begin.
    fn build_create_argv(&self, req: &ExecRequest) -> Vec<String> {
        debug_assert!(self.hermetic);
        let mut a = vec!["create".into()];
        a.extend(self.isolation_argv(req));
        a.extend([
            "--pull".into(),
            "never".into(),
            "--read-only".into(),
            // An image-declared healthcheck is separate from ENTRYPOINT and
            // would otherwise execute image-controlled code after `start`.
            "--no-healthcheck".into(),
            "--name".into(),
            self.container_name
                .clone()
                .expect("hermetic containers always have an owned name"),
            "--entrypoint".into(),
            req.program.clone(),
            self.image.clone(),
        ]);
        a.extend(req.args.iter().cloned());
        a
    }

    /// Build the attach/start phase for the exact name registered by
    /// [`Self::build_create_command`]. Pure argv is kept separate so tests can
    /// prove no repository-controlled value can alter this command.
    fn build_start_argv(&self) -> Vec<String> {
        debug_assert!(self.hermetic);
        vec![
            "start".into(),
            "-a".into(),
            self.container_name
                .clone()
                .expect("hermetic containers always have an owned name"),
        ]
    }

    /// Build the bounded, non-starting registration command for a hermetic
    /// container.
    pub fn build_create_command(
        &self,
        req: &ExecRequest,
    ) -> Result<tokio::process::Command, ExecError> {
        if !self.hermetic {
            return Err(ExecError::Unavailable(
                "container create lifecycle is only available in hermetic mode".into(),
            ));
        }
        let mut command = tokio::process::Command::new(&self.runtime);
        command.args(self.build_create_argv(req));
        Ok(command)
    }

    /// Build `runtime start -a <owned-name>` for a previously registered
    /// hermetic container.
    pub fn build_start_command(&self) -> Result<tokio::process::Command, ExecError> {
        if !self.hermetic {
            return Err(ExecError::Unavailable(
                "container start lifecycle is only available in hermetic mode".into(),
            ));
        }
        let mut command = tokio::process::Command::new(&self.runtime);
        command.args(self.build_start_argv());
        Ok(command)
    }
}

#[async_trait]
impl ExecBackend for ContainerBackend {
    fn build_command(&self, req: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        if self.hermetic {
            // Returning `create` here would be dangerously ambiguous: generic
            // callers would treat its zero exit as the check having run. Gate
            // must explicitly own both lifecycle phases and cleanup.
            return Err(ExecError::Unavailable(
                "hermetic containers require the explicit create/start lifecycle".into(),
            ));
        }
        let argv = self.build_run_argv(req);
        // The runtime CLIENT runs with our host env (it needs PATH/DOCKER_HOST);
        // the containerized command gets none of it (see build_run_argv).
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
    _approved: ApprovedRoots,
) -> std::sync::Arc<dyn ExecBackend> {
    use std::sync::Arc;
    match cfg.backend {
        BackendKind::Host => Arc::new(HostBackend),
        BackendKind::Native => {
            #[cfg(target_os = "macos")]
            {
                if native_backend_available() {
                    Arc::new(SeatbeltBackend::new(cfg.net, _extra_writable, _approved))
                } else {
                    Arc::new(HostBackend)
                }
            }
            #[cfg(target_os = "linux")]
            {
                if native_backend_available() {
                    Arc::new(LandlockBackend::new(cfg.net, _extra_writable, _approved))
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

/// True if this *machine* can apply an OS sandbox at all, probed with a
/// maximally permissive profile. A `false` here is a genuine platform
/// property (managed or nested environments that reject `sandbox_apply`),
/// never a statement about our own policy.
pub fn native_sandbox_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", "(version 1)\n(allow default)\n", "/usr/bin/true"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
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

/// True if the native backend's *real* generated profile applies and can run a
/// command. Probing the production builder rather than a hand-copied replica
/// keeps one source of policy: a profile defect fails here loudly instead of
/// masquerading as a platform limitation. Given [`native_sandbox_supported`],
/// a `false` here is a bug in our profile.
pub fn native_backend_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let ws = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            let backend = SeatbeltBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default());
            // `cd` exercises path traversal through the workspace's ancestors,
            // which a broken profile fails even when plain exec succeeds.
            let script = format!("cd {} && /usr/bin/true", shell_quote(&ws.to_string_lossy()));
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &backend.profile(&ws), "/bin/sh", "-c", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
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

    /// A successful leader can leave a redirected helper behind. Completion is
    /// based on waitid(WNOWAIT), not pipe EOF, and the group is quiesced while
    /// the zombie leader still pins its id.
    #[cfg(unix)]
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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_shell_success_reap_survives_high_contention() {
        let dir =
            std::env::temp_dir().join(format!("medha-successpg-stress-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut runs = Vec::new();
        for n in 0..128 {
            let cwd = dir.clone();
            let marker = dir.join(format!("survived-{n}.txt"));
            runs.push(tokio::spawn(async move {
                let script = format!("(sleep 1; touch {}) >/dev/null 2>&1 &", marker.display());
                run_shell_bounded(
                    &script,
                    &cwd,
                    std::time::Duration::from_secs(30),
                    1024,
                    None,
                )
                .await
                .unwrap()
            }));
        }
        for run in runs {
            let outcome = run.await.unwrap();
            // The invariant under test is group reaping, not scheduler
            // throughput: under full-suite load a leader can overrun its bound
            // and be killed, and that killed group must be reaped exactly like
            // a completed one — the survivor count below is the real check.
            // Anything besides clean completion or the bounded kill is a
            // genuine failure.
            assert!(
                outcome.passed() || outcome.timed_out,
                "run neither completed nor timed out: status={:?} cancelled={}",
                outcome.status,
                outcome.cancelled
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let survivors = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("survived-"))
            .count();
        assert_eq!(survivors, 0, "redirected helpers escaped under load");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_task_waits_for_pipe_holding_descendants_to_be_reaped() {
        let dir = std::env::temp_dir().join(format!("medha-bg-pipe-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived.txt");
        let script = format!("(sleep 0.5; touch {}) & exit 0", marker.display());
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", &script]).current_dir(&dir);
        let process = spawn_background(command).unwrap();
        assert!(
            process.wait_until(std::time::Duration::from_secs(2)).await,
            "completion was held hostage by a descendant's pipe"
        );
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !marker.exists(),
            "pipe-holding descendant survived completion"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn waitid_does_not_report_a_running_leader_as_exited() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh");
        child.args(["-c", "sleep 1"]).process_group(0);
        let mut child = child.spawn().unwrap();
        assert!(!leader_exited_without_reap(child.id()).unwrap());
        kill_process_tree(child.id());
        let _ = child.wait();
    }

    #[tokio::test]
    async fn host_backend_runs_and_captures() {
        #[cfg(unix)]
        let request = req("/bin/sh", &["-c", "printf hello"], std::env::temp_dir());
        #[cfg(windows)]
        let request = req(
            "cmd.exe",
            &["/D", "/S", "/C", "echo hello"],
            std::env::temp_dir(),
        );

        let out = HostBackend.run(request).await.unwrap();
        assert_eq!(out.status, Some(0));
        // `cmd.exe echo` terminates with CRLF; the capture contract is the
        // payload, not a platform-specific shell's line ending.
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        assert!(!out.stdout_truncated);
        assert!(!out.stderr_truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_capture_has_independent_and_aggregate_limits() {
        let out = HostBackend
            .run(req(
                "/bin/sh",
                &[
                    "-c",
                    "head -c 2000000 /dev/zero; head -c 2000000 /dev/zero >&2",
                ],
                std::env::temp_dir(),
            ))
            .await
            .unwrap();
        assert_eq!(out.status, Some(0));
        assert!(out.stdout.len() <= EXEC_STDOUT_CAP);
        assert!(out.stderr.len() <= EXEC_STDERR_CAP);
        assert!(out.stdout.len() + out.stderr.len() <= EXEC_AGGREGATE_CAP);
        assert!(out.stdout_truncated);
        assert!(out.stderr_truncated);
    }

    /// The guard for the guards: wherever the OS can sandbox at all, our own
    /// generated profile must apply and run commands. Without this implication
    /// a profile defect reads as "platform unsupported", every gated security
    /// test skips, and the whole suite passes vacuously.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_profile_applies_wherever_the_platform_supports_sandboxing() {
        if !native_sandbox_supported() {
            return;
        }
        assert!(
            native_backend_available(),
            "the platform sandbox works but our generated profile does not apply — \
             the profile is broken"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_jails_writes_outside_workspace() {
        // Managed/nested macOS environments may expose sandbox-exec but deny
        // sandbox_apply. Selection degrades to HostBackend and the CLI warns;
        // only exercise the jail where the OS can actually apply it. Gating on
        // *platform* support keeps a broken profile from skipping this test.
        if !native_sandbox_supported() {
            eprintln!(
                "Seatbelt unavailable on this host; native backend correctly degrades to host"
            );
            return;
        }
        let ws = std::env::temp_dir().join(format!("medha-seatbelt-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&ws).unwrap();
        let backend = SeatbeltBackend::new(NetPolicy::Allow, vec![], ApprovedRoots::default());

        // Writing INSIDE the workspace is allowed.
        let inside = backend
            .run(req("/bin/sh", &["-c", "touch ok.txt"], ws.clone()))
            .await
            .unwrap();
        assert_eq!(
            inside.status,
            Some(0),
            "in-workspace write should succeed; stderr={}",
            String::from_utf8_lossy(&inside.stderr)
        );
        assert!(ws.join("ok.txt").exists());

        // HOME is an isolated writable tree, not the user's real home.
        let reported_home = backend
            .run(req("/bin/sh", &["-c", "printf %s \"$HOME\""], ws.clone()))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&reported_home.stdout),
            backend.home.path.to_string_lossy()
        );

        // Writing to the real HOME by absolute path is denied.
        let escape_marker = format!(".medha-seatbelt-escape-{}", ulid::Ulid::new());
        let home = std::env::var("HOME").unwrap();
        let escape = std::path::Path::new(&home).join(&escape_marker);
        let cmd = format!("touch {}", shell_quote(&escape.to_string_lossy()));
        let outside = backend
            .run(req("/bin/sh", &["-c", &cmd], ws.clone()))
            .await
            .unwrap();
        assert_ne!(outside.status, Some(0), "write to HOME must be blocked");
        assert!(!escape.exists(), "escape file must not exist");

        std::fs::remove_dir_all(&ws).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_children_cannot_read_host_credentials() {
        if !native_sandbox_supported() {
            return;
        }
        let base = std::env::temp_dir().join(format!("medha-seatbelt-read-{}", ulid::Ulid::new()));
        let ws = base.join("workspace");
        let host_home = base.join("host-home");
        std::fs::create_dir_all(&ws).unwrap();
        let credentials = [
            ".ssh/id_ed25519",
            ".aws/credentials",
            ".medha/credentials",
            ".npmrc",
            ".git-credentials",
            ".docker/config.json",
            ".kube/config",
            ".zsh_history",
        ];
        for relative in credentials {
            let path = host_home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "TOP-SECRET").unwrap();
        }
        let backend = SeatbeltBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default());
        for relative in credentials {
            let path = host_home.join(relative);
            let command = format!("cat {}", shell_quote(&path.to_string_lossy()));
            let output = backend
                .run(req("/bin/sh", &["-c", &command], ws.clone()))
                .await
                .unwrap();
            assert_ne!(
                output.status,
                Some(0),
                "native child read host credential {}",
                path.display()
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("TOP-SECRET"));
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_is_read_deny_by_default_and_filters_sensitive_writes() {
        let sensitive = home_dir_from_env()
            .unwrap_or_else(|| PathBuf::from("/Users/example"))
            .join(".ssh");
        let backend =
            SeatbeltBackend::new(NetPolicy::Deny, vec![sensitive], ApprovedRoots::default());
        assert!(backend.extra_writable.is_empty());
        let workspace =
            std::env::temp_dir().join(format!("medha-seatbelt-profile-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&workspace).unwrap();
        let profile = backend.profile(&workspace);
        assert!(profile.contains("(deny file-read*)"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow file-read* (subpath \"/\")"));
        // Traversal grants: the literal root and directory metadata only —
        // never a readable subtree.
        assert!(profile.contains("(allow file-read* (literal \"/\"))"));
        assert!(
            profile
                .contains("(allow file-read-metadata (vnode-type DIRECTORY) (vnode-type SYMLINK))")
        );
        assert!(profile.contains("/private/var/select"));
        std::fs::remove_dir_all(&workspace).ok();
    }

    /// A root approved at runtime opens the exec jail on the very next spawn —
    /// and only that root: an unapproved sibling stays denied.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_honours_runtime_approved_roots_without_restart() {
        if !native_sandbox_supported() {
            return;
        }
        let base = std::env::temp_dir().join(format!("medha-seatbelt-live-{}", ulid::Ulid::new()));
        let ws = base.join("workspace");
        let granted = base.join("granted");
        let sibling = base.join("sibling");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&granted).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(granted.join("notes.md"), "granted-content").unwrap();
        std::fs::write(sibling.join("notes.md"), "sibling-content").unwrap();

        let approved = ApprovedRoots::default();
        let backend = SeatbeltBackend::new(NetPolicy::Allow, vec![], approved.clone());
        let read_granted = format!(
            "cat {}",
            shell_quote(&granted.join("notes.md").to_string_lossy())
        );
        let read_sibling = format!(
            "cat {}",
            shell_quote(&sibling.join("notes.md").to_string_lossy())
        );

        let before = backend
            .run(req("/bin/sh", &["-c", &read_granted], ws.clone()))
            .await
            .unwrap();
        assert_ne!(before.status, Some(0), "unapproved root must start denied");

        approved.allow_read(granted.canonicalize().unwrap());

        let after = backend
            .run(req("/bin/sh", &["-c", &read_granted], ws.clone()))
            .await
            .unwrap();
        assert_eq!(
            after.status,
            Some(0),
            "approved root must open without restart; stderr={}",
            String::from_utf8_lossy(&after.stderr)
        );
        assert!(String::from_utf8_lossy(&after.stdout).contains("granted-content"));

        let still_denied = backend
            .run(req("/bin/sh", &["-c", &read_sibling], ws.clone()))
            .await
            .unwrap();
        assert_ne!(
            still_denied.status,
            Some(0),
            "an unapproved sibling must stay denied"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn native_sandbox_defaults_to_network_deny() {
        assert_eq!(SandboxConfig::default().net, NetPolicy::Deny);
    }

    #[cfg(unix)]
    fn denied_output(stderr: &str) -> ExecOutput {
        ExecOutput {
            status: Some(1),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn escalation_candidates_come_from_denial_lines_and_widen_files_to_parents() {
        let base = std::env::temp_dir().join(format!("medha-escal-{}", ulid::Ulid::new()));
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("notes.md");
        std::fs::write(&target, "x").unwrap();

        let stderr = format!("cat: {}: Operation not permitted", target.display());
        let candidates = escalation_candidates(
            &denied_output(&stderr),
            &["-c".into(), format!("cat {}", target.display())],
            &ws,
            &ApprovedRoots::default(),
        );
        assert_eq!(
            candidates,
            vec![outside.canonicalize().unwrap()],
            "a denied file must widen to its parent directory"
        );

        // Success output or unrelated stderr must never produce candidates.
        assert!(
            escalation_candidates(
                &denied_output("cat: /nonexistent-dir-zz/f: No such file or directory"),
                &[],
                &ws,
                &ApprovedRoots::default(),
            )
            .is_empty()
        );

        // An in-workspace denial is not escalatable (nothing to approve).
        let inside = ws.join("f.txt");
        std::fs::write(&inside, "x").unwrap();
        let stderr = format!("cat: {}: Operation not permitted", inside.display());
        assert!(
            escalation_candidates(&denied_output(&stderr), &[], &ws, &ApprovedRoots::default())
                .is_empty()
        );

        // An already-approved root cannot be the cause; it is excluded.
        let approved = ApprovedRoots::default();
        approved.allow_read(outside.canonicalize().unwrap());
        let stderr = format!("cat: {}: Operation not permitted", target.display());
        assert!(escalation_candidates(&denied_output(&stderr), &[], &ws, &approved).is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn escalation_candidates_never_offer_credential_paths() {
        let Some(home) = home_dir_from_env() else {
            return;
        };
        let ssh = home.join(".ssh");
        if !ssh.exists() {
            return;
        }
        let ws = std::env::temp_dir();
        let stderr = format!(
            "cat: {}: Operation not permitted",
            ssh.join("id_rsa").display()
        );
        assert!(
            escalation_candidates(&denied_output(&stderr), &[], &ws, &ApprovedRoots::default())
                .is_empty(),
            "credential paths must never reach an approval card"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_extra_write_roots_resolve_symlink_aliases_before_filtering() {
        let base = std::env::temp_dir().join(format!("medha-native-extra-{}", ulid::Ulid::new()));
        let sensitive = base.join("sensitive");
        let alias = base.join("apparently-safe");
        std::fs::create_dir_all(&sensitive).unwrap();
        std::os::unix::fs::symlink(&sensitive, &alias).unwrap();

        assert!(
            safe_extra_writable_against(
                &[alias.join("future-child")],
                std::slice::from_ref(&sensitive),
            )
            .is_empty(),
            "a symlink alias must not turn a sensitive subtree into a writable root"
        );

        let allowed = base.join("allowed/future-child");
        let expected_allowed = resolve_native_policy_path(&allowed).unwrap();
        assert_eq!(
            safe_extra_writable_against(
                std::slice::from_ref(&allowed),
                std::slice::from_ref(&sensitive),
            ),
            vec![expected_allowed]
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_selection_degrades_to_host_when_seatbelt_cannot_apply() {
        if native_backend_available() {
            return;
        }
        let backend = select_backend(&SandboxConfig::default(), vec![], ApprovedRoots::default());
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
        let argv = be.build_run_argv(&r);
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
    fn hermetic_container_never_pulls_or_runs_image_entrypoint() {
        let be = ContainerBackend::new_hermetic(
            "/usr/bin/docker".into(),
            "local-check-image".into(),
            Some("4g".into()),
            Some(256),
            "medha-gate-test".into(),
        );
        let argv = be.build_create_argv(&req(
            "env",
            &["-i", "HOME=/workspace/home", "sh", "-c", "true"],
            std::env::temp_dir(),
        ));
        let joined = argv.join(" ");
        assert_eq!(argv.first().map(String::as_str), Some("create"));
        assert!(!argv.iter().any(|argument| argument == "run"));
        assert!(!argv.iter().any(|argument| argument == "--rm"));
        assert!(joined.contains("-v ") && joined.contains(":/workspace"));
        assert!(joined.contains("-w /workspace"));
        assert!(joined.contains("--cap-drop ALL"));
        assert!(joined.contains("--security-opt no-new-privileges"));
        assert!(joined.contains("--pull never"));
        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--no-healthcheck"));
        assert!(joined.contains("--network none"));
        assert!(joined.contains("--name medha-gate-test"));
        assert!(joined.contains("--entrypoint env"));
        assert!(joined.contains("--memory 4g"));
        assert!(joined.contains("--pids-limit 256"));
        let image = argv
            .iter()
            .position(|argument| argument == "local-check-image")
            .unwrap();
        assert_eq!(
            &argv[image + 1..],
            &[
                "-i".to_string(),
                "HOME=/workspace/home".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                "true".to_string(),
            ]
        );
        assert_eq!(
            be.build_start_argv(),
            ["start", "-a", "medha-gate-test"].map(String::from)
        );
        let error = be
            .build_command(&req("sh", &["-c", "true"], std::env::temp_dir()))
            .expect_err("generic execution must not confuse create with a completed check");
        assert!(
            error
                .to_string()
                .contains("explicit create/start lifecycle")
        );
    }

    #[test]
    fn container_argv_allows_network_by_default() {
        let be = ContainerBackend::new("podman".into(), "img".into(), NetPolicy::Allow, None, None);
        let argv = be.build_run_argv(&req("sh", &["-c", "true"], std::env::temp_dir()));
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
