//! Swappable host, native, container, and SSH command-execution backends.

use async_trait::async_trait;
use permissions::{ApprovedRoots, NetworkGrant};
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
    /// Read roots granted for this invocation only. These are deliberately
    /// carried by the request rather than published through [`ApprovedRoots`],
    /// so an "allow once" answer cannot leak into a concurrent command.
    pub read_roots: Vec<PathBuf>,
    /// Write roots granted for this invocation only. A write root also implies
    /// read access in native backends, but disappears with this request.
    pub write_roots: Vec<PathBuf>,
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
    /// Build the fully jail-configured command without spawning it, so isolation
    /// is applied in one place for both foreground and background runs.
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
    /// Whether this backend was configured to deny network for `req`. Deliberately
    /// not `containment()`, which Landlock under-reports by design: this answers
    /// only "did we ask for deny", which the failure-driven retry needs and which
    /// is knowable from config without probing enforcement. Defaults to false, so
    /// unsandboxed backends never offer a grant they cannot honour.
    fn denies_network(&self, _req: &ExecRequest) -> bool {
        false
    }
}

/// Resolver/routing failure text every runtime on this platform ultimately
/// prints, because it comes from this platform's own libc. Hand-listing these
/// strings is what left macOS uncovered: the list carried glibc's wording, so
/// `pip` and `cargo` — which surface `gai_strerror` verbatim — were invisible on
/// darwin while `npm` was not. Asking libc removes the asymmetry by
/// construction rather than by remembering.
static NETWORK_DENIAL_MARKERS: std::sync::LazyLock<Vec<String>> =
    std::sync::LazyLock::new(build_network_denial_markers);

/// Text the local `strerror` cannot supply. Two kinds only, and each has a
/// reason it cannot be derived:
///
/// 1. Wording invented *above* libc by a language runtime or tool. No API
///    enumerates these; node decided to print `ENOTFOUND` and curl decided to
///    write its own sentence.
/// 2. glibc's resolver text, which a macOS host still meets through the
///    container backend — the image is Linux even when we are not. The
///    derivation covers the host's own libc; it cannot cover the image's.
///
/// Everything else comes from [`build_network_denial_markers`]. Adding a line
/// here should feel like a defeat: it means something was learned by hand that
/// the platform could not be asked.
const NETWORK_DENIAL_SUPPLEMENT: &[&str] = &[
    // glibc, reachable from any host through a Linux container image.
    "name or service not known",
    "temporary failure in name resolution",
    // Symbolic codes: node prints these instead of the libc sentence.
    "enotfound",
    "eai_again",
    "enetunreach",
    "ehostunreach",
    // A named resolver call in the failure is itself the signal.
    "getaddrinfo",
    "could not resolve host",
    "could not resolve proxy",
    // libgit2 (cargo, git via libgit2) prefixes the libc text with its own.
    "failed to resolve address",
    // Trailing quote is load-bearing: it separates urllib3's "Failed to resolve
    // 'host'" from rustc's "failed to resolve: use of undeclared crate".
    "failed to resolve '",
    "temporary failure resolving",
    "no such host",
    "no such host is known",
];

/// Codes whose text names a resolver or routing failure specifically. Kept
/// narrow on purpose: `EAI_SYSTEM` ("System error") and the memory/service
/// codes describe a caller mistake, and admitting their generic wording as a
/// substring would fire this signature on unrelated output.
#[cfg(unix)]
const NETWORK_DENIAL_ERRNOS: &[i32] = &[
    libc::ENETUNREACH,
    libc::EHOSTUNREACH,
    libc::ENETDOWN,
    libc::EHOSTDOWN,
];

#[cfg(unix)]
const NETWORK_DENIAL_GAI_CODES: &[i32] = &[libc::EAI_NONAME, libc::EAI_AGAIN, libc::EAI_FAIL];

/// Shortest marker admitted from libc. A terse translation ("Down") would match
/// far more than a denied socket, and no genuine resolver sentence is this brief.
#[cfg(unix)]
const MIN_DERIVED_MARKER_LEN: usize = 10;

fn build_network_denial_markers() -> Vec<String> {
    let mut markers: Vec<String> = NETWORK_DENIAL_SUPPLEMENT
        .iter()
        .map(|marker| marker.to_ascii_lowercase())
        .collect();
    #[cfg(unix)]
    {
        for &code in NETWORK_DENIAL_ERRNOS {
            let text = std::io::Error::from_raw_os_error(code).to_string();
            // `io::Error` appends " (os error N)"; the libc sentence is the part
            // a program actually prints.
            let text = text.split(" (os error").next().unwrap_or_default();
            push_derived_marker(&mut markers, text);
        }
        for &code in NETWORK_DENIAL_GAI_CODES {
            // SAFETY: `gai_strerror` returns a pointer to a static, NUL-terminated
            // string for any input, and never one the caller owns.
            let text = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(code)) };
            push_derived_marker(&mut markers, &text.to_string_lossy());
        }
    }
    markers.sort();
    markers.dedup();
    markers
}

#[cfg(unix)]
fn push_derived_marker(markers: &mut Vec<String>, text: &str) {
    let marker = text.trim().to_ascii_lowercase();
    if marker.len() >= MIN_DERIVED_MARKER_LEN {
        markers.push(marker);
    }
}

// Deliberately absent: "operation not permitted", which is the filesystem-denial
// signature and would offer a network grant for an fs jail block; and bare
// "failed to connect", which a down local service produces just as readily as a
// denied socket. Resolver and routing text keeps the two escalations disjoint.

/// True when output under a net-denying jail looks like a policy-blocked network
/// attempt. `denies_network` is the backend's config intent (did we ask for
/// deny), not proof of enforcement — the failure itself is the proof, so no
/// kernel probe is needed.
///
/// The exit code is deliberately ignored: agents routinely mask it behind pipes
/// (`| tail`), a trailing `echo`, or `|| true`, so a zero exit is no evidence the
/// command succeeded. A resolver/socket-failure marker under a net-denying box is
/// the signal on its own.
pub fn network_denial_signature(stdout: &str, stderr: &str, denies_network: bool) -> bool {
    if !denies_network {
        return false;
    }
    let output = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    matches_network_denial(&output)
}

/// Match already-lowercased output. Split out so the streaming scanner and the
/// post-hoc check cannot drift apart.
fn matches_network_denial(lowercased: &str) -> bool {
    if NETWORK_DENIAL_MARKERS
        .iter()
        .any(|marker| lowercased.contains(marker.as_str()))
    {
        return true;
    }
    lowercased.lines().any(refused_socket_line)
}

/// Socket operations whose refusal is the *only* thing a Landlock net-deny
/// produces. Landlock covers TCP `bind`/`connect` and nothing else, so DNS
/// (UDP) still resolves and the failure arrives as `connect … EACCES` — text
/// containing no resolver wording whatsoever. Without this rule, Linux is the
/// platform where the grant card almost never appears.
const REFUSED_SOCKET_OPS: &[&str] = &["connect", "sendto", "sendmsg"];

/// Paired with an operation above, on the same line. The conjunction is what
/// keeps this disjoint from a filesystem denial: an fs refusal names `open` or
/// a path, never a socket call. Matching either token alone would make every
/// blocked file read look like a network problem.
const REFUSED_SOCKET_ERRORS: &[&str] = &[
    "eacces",
    "eperm",
    "permission denied",
    "operation not permitted",
];

fn refused_socket_line(line: &str) -> bool {
    REFUSED_SOCKET_OPS.iter().any(|op| line.contains(op))
        && REFUSED_SOCKET_ERRORS
            .iter()
            .any(|error| line.contains(error))
}

/// Incremental form of [`network_denial_signature`], fed every captured byte as
/// it arrives so a stalled command can be caught while it is still running
/// rather than autopsied at its deadline.
struct DenialScan {
    /// Tail of the previous chunk, so a marker split across a pipe read is still
    /// matched.
    carry: Vec<u8>,
    window: usize,
    hit: bool,
}

impl DenialScan {
    fn new() -> Self {
        let longest = NETWORK_DENIAL_MARKERS
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0);
        Self {
            carry: Vec::new(),
            window: longest.saturating_sub(1),
            hit: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if self.hit {
            return;
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend(bytes.iter().map(u8::to_ascii_lowercase));
        let text = String::from_utf8_lossy(&buf);
        self.hit = matches_network_denial(&text);
        if self.hit {
            return;
        }
        // Carry whichever is longer: the unterminated final line, or a marker's
        // worth of bytes. The line rule needs a whole line to judge, and a
        // resolver failure is routinely longer than the longest single marker,
        // so a marker-sized window alone would let a split line through.
        let line_start = buf
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let keep = (buf.len() - line_start)
            .max(self.window)
            .min(MAX_SCAN_CARRY)
            .min(buf.len());
        buf.drain(..buf.len() - keep);
        self.carry = buf;
    }
}

/// Ceiling on carried bytes, so a command emitting one enormous unbroken line
/// cannot grow the scanner without bound.
const MAX_SCAN_CARRY: usize = 8 * 1024;

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

/// Fires a `SIGKILL` at a whole process group when dropped while still armed —
/// the synchronous backstop if the runtime aborts the supervisor.
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

/// Kill the command and descendants that still belong to it. Windows has no
/// `killpg`, so `taskkill /T` is invoked by absolute path to resist shadowing.
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

/// Put a command in its own process group (unix) and pipe stdout/stderr so it
/// can be supervised and group-killed. Background tasks clear `kill_on_drop`.
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
    // These are noninteractive tool processes. Inheriting stdin lets a command
    // consume TUI/REPL input, or stop on SIGTTIN in its separate process group.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
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
/// Group-reaped on drop, so a timeout takes the whole tree rather than leaving
/// grandchildren holding locks and ports.
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

/// Run a shell command through a configured backend with the same guarantees as
/// [`run_shell_bounded`]. The verifier path: build scripts run under the same jail.
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
        read_roots: Vec::new(),
        write_roots: Vec::new(),
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
    /// `/D` and `-NoProfile` stop AutoRun hooks and user profiles injecting into
    /// an approved command. The command string itself is passed through untouched
    /// so it matches exactly what the policy scanner read.
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

/// Pick the interpreter. `MEDHA_SHELL` wins outright, so an unusual setup has an
/// escape hatch that does not wait on a release.
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
    // Split on both separators: `file_stem` only knows the host's, so a Windows
    // path classified elsewhere would read as one filename and match nothing.
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

/// Git for Windows puts only `cmd\` on PATH, so bash is derived from `git.exe`:
/// `…\Git\cmd\git.exe` → `…\Git\bin\bash.exe`.
pub fn bash_beside_git(git_exe: &Path) -> Option<PathBuf> {
    let git_root = git_exe.parent()?.parent()?;
    ["bin", "usr/bin"]
        .iter()
        .map(|d| git_root.join(d).join("bash.exe"))
        .find(|p| p.is_file())
}

/// Whether a path is a shell that can actually be spawned. `is_file()` alone
/// accepts Windows Store zero-byte app execution aliases, which cannot be run.
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

/// Choose a local Windows shell or `sh` for Unix-like and remote backends.
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
    let process = spawn_background_with_limits(cmd, max_output, max_output, max_output, false)?;
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
    /// Every byte ever captured, not what is currently retained. A rolling tail
    /// makes the buffer length useless as a progress measure.
    written: u64,
    scan: Option<DenialScan>,
}

impl CapturePair {
    fn new(
        stdout_cap: usize,
        stderr_cap: usize,
        aggregate_cap: usize,
        watch_network: bool,
    ) -> Self {
        Self {
            stdout: TailBuf::new(stdout_cap),
            stderr: TailBuf::new(stderr_cap),
            aggregate_cap,
            written: 0,
            scan: watch_network.then(DenialScan::new),
        }
    }

    fn push(&mut self, stream: CapturedStream, bytes: &[u8]) {
        self.written = self.written.saturating_add(bytes.len() as u64);
        // Scan before the aggregate trim below: a marker must not be lost to the
        // rolling tail that a later, noisier stage of the same command overruns.
        if let Some(scan) = self.scan.as_mut() {
            scan.feed(bytes);
        }
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

/// An owned command task: output streams into rolling buffers and the whole
/// process group can be killed. Foreground runs use it too, so cancellation has
/// a synchronous kill handle before the future is dropped.
pub struct BgProc {
    pub pid: Option<u32>,
    capture: SharedCapture,
    done_rx: tokio::sync::watch::Receiver<bool>,
    code: std::sync::Arc<std::sync::Mutex<Option<i32>>>,
    /// Set by the seccomp watcher the instant this command tried to reach an IP
    /// address under a net-denying jail. `None` where no such watcher exists.
    net_flag: Option<std::sync::Arc<AtomicBool>>,
}

impl BgProc {
    /// Current buffered stdout / stderr (tails, with a marker if truncated).
    pub fn snapshot(&self) -> (String, String) {
        self.capture
            .lock()
            .map(|capture| (capture.stdout.text(), capture.stderr.text()))
            .unwrap_or_default()
    }
    /// Whether this command has been seen trying, and failing, to reach the
    /// network. The kernel watcher is exact and fires before any output exists;
    /// the output scanner is the fallback where no watcher could be installed.
    /// Always false unless the task was spawned under a net-denying backend.
    pub fn network_denial_seen(&self) -> bool {
        if self
            .net_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return true;
        }
        self.capture
            .lock()
            .map(|capture| capture.scan.as_ref().is_some_and(|scan| scan.hit))
            .unwrap_or(false)
    }
    /// Monotonic count of bytes captured so far — a progress measure that a
    /// rolling tail cannot make go backwards.
    pub fn bytes_seen(&self) -> u64 {
        self.capture
            .lock()
            .map(|capture| capture.written)
            .unwrap_or(0)
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

/// Leader first, then descendants parent-first, then the group as a whole. The
/// ordering stops a shell waking after only its child was killed.
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
    watch_network: bool,
) -> Result<BgProc, ExecError> {
    use std::sync::{Arc, Mutex};
    configure_for_spawn(&mut cmd, false);
    // Arm kernel-level detection before the fork. Armed here rather than by the
    // caller so the filter and the handle that reads it cannot be wired up
    // separately and drift.
    #[cfg(target_os = "linux")]
    let pending = watch_network
        .then(|| crate::netnotify::arm(cmd.as_std_mut()))
        .flatten();
    #[cfg(target_os = "linux")]
    let net_flag = pending.as_ref().map(crate::netnotify::Pending::flag);
    #[cfg(not(target_os = "linux"))]
    let net_flag: Option<Arc<AtomicBool>> = None;
    // std Child, not tokio: the leader must stay unreaped (PID reserved) until
    // its group helpers are killed, or a later group kill races PID reuse.
    let mut child = cmd
        .as_std_mut()
        .spawn()
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    let finished = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    if let Some(pending) = pending {
        crate::netnotify::watch(pending, finished.clone());
    }
    let pid = child.id();
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let capture: SharedCapture = Arc::new(Mutex::new(CapturePair::new(
        stdout_cap,
        stderr_cap,
        aggregate_cap,
        watch_network,
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
    // A native supervisor thread, not an async poll: under high fan-out a
    // starved runtime worker lets an orphaned helper work on after the leader dies.
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
        // Release the network watcher before publishing completion, so it does
        // not outlive the command it was watching.
        finished.store(true, Ordering::Release);
        let _ = done_tx.send(true);
    });

    Ok(BgProc {
        pid: Some(pid),
        capture,
        done_rx,
        code,
        net_flag,
    })
}

/// Spawn an owned command task using the standard independent and aggregate
/// output ceilings. `watch_network` arms live resolver-failure detection, which
/// only a caller running under a net-denying backend has any use for.
pub fn spawn_background(
    cmd: tokio::process::Command,
    watch_network: bool,
) -> Result<BgProc, ExecError> {
    spawn_background_with_limits(
        cmd,
        EXEC_STDOUT_CAP,
        EXEC_STDERR_CAP,
        EXEC_AGGREGATE_CAP,
        watch_network,
    )
}

/// Spawn, supervise, and capture a foreground command using fixed-memory
/// rolling tails. Dropping this future drops its `BgProc`, which immediately
/// signals the group; the detached owner still reaps and joins every resource.
async fn spawn_and_wait(cmd: tokio::process::Command) -> Result<ExecOutput, ExecError> {
    // Foreground: the complete output is checked post-hoc, so live detection
    // would only duplicate work already bounded by this call's own deadline.
    let process = spawn_background(cmd, false)?;
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
    /// The one isolated HOME for this process, shared by every native backend and
    /// never dropped while it lives — a per-backend home died under long-lived
    /// LSP/MCP children and broke their toolchains.
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

/// Resolve an absolute policy path through its deepest existing ancestor, so
/// not-yet-created write roots stay usable and symlink aliases still resolve.
///
/// Deliberately not gated to the native-sandbox platforms: it is ordinary path
/// normalization (it already handles `Component::Prefix`, which only Windows
/// has), and [`escalation_candidates`] — which every platform compiles — calls it.
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn safe_request_readable(paths: &[PathBuf]) -> Vec<PathBuf> {
    let sensitive: Vec<PathBuf> = native_sensitive_paths()
        .into_iter()
        .filter_map(|path| resolve_native_policy_path(&path))
        .collect();
    paths
        .iter()
        .filter_map(|path| resolve_native_policy_path(path))
        .filter(|path| {
            !sensitive
                .iter()
                .any(|secret| secret.starts_with(path) || path.starts_with(secret))
        })
        .collect()
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

/// Roots already present in every native read profile. Error messages often
/// prefix a denied target with the reporting executable (`/bin/sh: ...`); that
/// executable path is evidence context, not a blocked candidate to prompt for.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_intrinsic_read_roots() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let roots = [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/opt/homebrew",
        "/usr/local",
        "/private/etc/ssl",
        "/private/var/select",
        "/private/var/db/xcode_select_link",
        "/Library/Developer",
        "/Applications/Xcode.app",
        // Resolver config. `curl` resolves through getaddrinfo/mDNSResponder and
        // needs none of this, but standalone resolvers (dig, nslookup, host)
        // read the nameserver list from disk and fail on a network-allowed box
        // if it is unreadable. Verified sufficient on its own — the wider
        // SystemConfiguration store is not required, so it stays denied.
        // `/etc/resolv.conf` is a symlink chain to the var/run target, which is
        // the vnode Seatbelt actually matches.
        "/private/var/run/resolv.conf",
        "/private/etc/resolv.conf",
    ];
    #[cfg(target_os = "linux")]
    let roots = [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/nix/store",
        "/run/current-system/sw",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/proc",
        "/sys",
    ];
    let mut paths: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|path| resolve_native_policy_path(Path::new(path)))
        .collect();
    paths.extend(
        native_toolchain_read_roots()
            .into_iter()
            .filter_map(|path| resolve_native_policy_path(&path)),
    );
    paths
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_intrinsic_read_roots() -> Vec<PathBuf> {
    Vec::new()
}

/// Out-of-workspace roots a failed command was plausibly denied on — the input
/// to the escalation prompt. Only paths named in stderr; files widen to their
/// parent; credential paths and already-approved roots are never offered.
pub(crate) fn escalation_candidates(
    output: &ExecOutput,
    workspace: &Path,
    approved: &ApprovedRoots,
    permission: permissions::PermissionType,
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
    let tokens: Vec<PathBuf> = denial_lines
        .iter()
        .flat_map(|line| absolute_path_tokens(line))
        .collect();
    // If the denial itself did not identify a path, there is no evidence that
    // approving an argv path could change the result. Falling back to argv made
    // unrelated application-level "Permission denied" failures raise cards.
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let sensitive = native_sensitive_paths();
    let intrinsic_read = native_intrinsic_read_roots();
    let mut candidates = Vec::new();
    for token in tokens {
        let Some(resolved) = resolve_native_policy_path(&token) else {
            continue;
        };
        let root = if resolved.is_dir() {
            resolved
        } else {
            match resolved.parent() {
                Some(parent) => parent.to_path_buf(),
                None => continue,
            }
        };
        if root.starts_with(&workspace)
            || intrinsic_read
                .iter()
                .any(|allowed| root.starts_with(allowed))
            || approved.is_allowed(&root, permission)
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
            // Shim directories alone are not enough: pyenv/nvm/Volta exec payloads
            // and load libraries from these version roots.
            ".pyenv/versions",
            ".nvm/versions",
            ".local/bin",
            ".local/lib",
            ".volta/bin",
            ".volta/tools",
            ".bun/bin",
            ".bun/install/cache",
            ".deno/bin",
            "miniconda3",
            "anaconda3",
            ".rye/py",
            ".rye/self",
            ".rye/shims",
            ".sdkman/candidates",
            ".asdf/installs",
            ".local/share/mise/installs",
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

/// macOS Seatbelt backend: `sandbox-exec` with a generated SBPL profile. Reads
/// and writes are deny-by-default, reopened only for the workspace, an isolated
/// HOME/TMP, system runtimes, and selected toolchain roots.
#[cfg(target_os = "macos")]
pub struct SeatbeltBackend {
    net: NetPolicy,
    extra_writable: Vec<PathBuf>,
    approved: ApprovedRoots,
    net_grant: NetworkGrant,
    home: std::sync::Arc<IsolatedHome>,
}

#[cfg(target_os = "macos")]
impl SeatbeltBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>, approved: ApprovedRoots) -> Self {
        Self {
            net,
            extra_writable: safe_extra_writable(&extra_writable),
            approved,
            net_grant: NetworkGrant::default(),
            home: IsolatedHome::shared(),
        }
    }

    pub fn with_network_grant(mut self, net_grant: NetworkGrant) -> Self {
        self.net_grant = net_grant;
        self
    }

    /// Network policy for this run: a one-shot task-local grant or a live
    /// session/persistent grant opens it; otherwise the configured default.
    fn effective_net(&self, _req: &ExecRequest) -> NetPolicy {
        if kernel::network_once_active() || self.net_grant.granted() {
            NetPolicy::Allow
        } else {
            self.net
        }
    }

    fn readable_paths(&self, req: &ExecRequest) -> Vec<PathBuf> {
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
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
        readable.extend(safe_request_readable(&req.read_roots));
        readable.extend(safe_extra_writable(&req.write_roots));
        readable.sort();
        readable.dedup();
        readable
    }

    fn profile(&self, req: &ExecRequest) -> String {
        // Canonicalize so the subpath match survives /var → /private/var etc.
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
        let mut writable: Vec<PathBuf> = vec![ws, self.home.path.clone()];
        writable.extend(self.extra_writable.iter().cloned());
        writable.extend(safe_extra_writable(&self.approved.write_roots()));
        writable.extend(safe_extra_writable(&req.write_roots));
        writable.sort();
        writable.dedup();

        let mut p = String::from(
            "(version 1)\n(allow default)\n\
             (deny file-read*)\n(allow file-read*\n",
        );
        for path in self.readable_paths(req) {
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
        // Resolution stats every component, and a subpath rule covers neither the
        // root inode nor an allowed root's ancestors. Grant the literal root plus
        // directory/symlink metadata only — not readdir or contents.
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
        if self.effective_net(req) == NetPolicy::Deny {
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
        let profile = self.profile(req);
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
        // A session/persistent grant opens the network for all later commands, so
        // trust-flow must see it. A one-shot task-local grant is deliberately not
        // reflected here — it stays scoped to the single retried command.
        if self.net_grant.granted() {
            return kernel::Containment::OsFsJail;
        }
        match self.net {
            NetPolicy::Deny => kernel::Containment::OsFsJailNoNet,
            NetPolicy::Allow => kernel::Containment::OsFsJail,
        }
    }
    fn denies_network(&self, req: &ExecRequest) -> bool {
        self.effective_net(req) == NetPolicy::Deny
    }
}

/// Linux Landlock backend (kernel ≥5.13), applied in `pre_exec` so it confines
/// the child rather than the agent. The ruleset is built in the parent; only the
/// allocation-free `restrict_self` runs post-fork.
#[cfg(target_os = "linux")]
pub struct LandlockBackend {
    net: NetPolicy,
    extra_writable: Vec<PathBuf>,
    approved: ApprovedRoots,
    net_grant: NetworkGrant,
    home: std::sync::Arc<IsolatedHome>,
}

#[cfg(target_os = "linux")]
impl LandlockBackend {
    pub fn new(net: NetPolicy, extra_writable: Vec<PathBuf>, approved: ApprovedRoots) -> Self {
        Self {
            net,
            extra_writable: safe_extra_writable(&extra_writable),
            approved,
            net_grant: NetworkGrant::default(),
            home: IsolatedHome::shared(),
        }
    }

    pub fn with_network_grant(mut self, net_grant: NetworkGrant) -> Self {
        self.net_grant = net_grant;
        self
    }

    fn effective_net(&self, _req: &ExecRequest) -> NetPolicy {
        if kernel::network_once_active() || self.net_grant.granted() {
            NetPolicy::Allow
        } else {
            self.net
        }
    }

    fn writable_paths(&self, req: &ExecRequest) -> Vec<PathBuf> {
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
        let mut v = vec![ws, self.home.path.clone()];
        v.extend(self.extra_writable.iter().cloned());
        v.extend(safe_extra_writable(&self.approved.write_roots()));
        v.extend(safe_extra_writable(&req.write_roots));
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

    fn readable_paths(&self, req: &ExecRequest) -> Vec<PathBuf> {
        let ws = req.cwd.canonicalize().unwrap_or_else(|_| req.cwd.clone());
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
            // Toolchains locate themselves through procfs (rustc reads
            // /proc/self/exe); /sys carries the cgroup limits runtimes size to.
            //
            // CAVEAT: procfs is process-wide, so a child can read other same-UID
            // processes' environ, including this agent's keys. Landlock cannot
            // express a narrower grant; closing it needs PR_SET_DUMPABLE or a PID ns.
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
        paths.extend(safe_request_readable(&req.read_roots));
        paths.extend(safe_extra_writable(&req.write_roots));
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
    // Handle net access and add no rules: in Landlock that denies it. Best-effort,
    // so a no-op below kernel 6.7 rather than a failure.
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
            &self.readable_paths(req),
            &self.writable_paths(req),
            self.effective_net(req),
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
        // Net-deny is attempted but never claimed: it needs kernel ≥6.7 and is not
        // probed here, so trust-flow keeps gating web-tainted actions on Linux.
        kernel::Containment::OsFsJail
    }
    fn denies_network(&self, req: &ExecRequest) -> bool {
        // Config intent, not enforcement proof. On kernel <6.7 the rule no-ops,
        // the command succeeds, and the failure-driven card never fires; on ≥6.7
        // the failure itself proves enforcement. Neither needs a probe.
        self.effective_net(req) == NetPolicy::Deny
    }
}

/// True if `program` exists in `dir`, including Windows `PATHEXT` resolution —
/// probing only the extensionless path reports `npm.cmd` as missing.
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

/// Opt-in heavy tier: each command runs in a throwaway `docker`/`podman`
/// container, workspace bind-mounted at `/workspace`, capabilities dropped. The
/// host environment is not forwarded, so injected API keys never enter it.
pub struct ContainerBackend {
    runtime: String,
    image: String,
    net: NetPolicy,
    net_grant: NetworkGrant,
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
            net_grant: NetworkGrant::default(),
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
            net_grant: NetworkGrant::default(),
            memory,
            pids,
            hermetic: true,
            container_name: Some(container_name),
        }
    }

    pub fn with_network_grant(mut self, net_grant: NetworkGrant) -> Self {
        self.net_grant = net_grant;
        self
    }

    /// Network policy for this run. A hermetic verification container hard-denies
    /// regardless of any grant — it runs untrusted repository code and must never
    /// be opened by a session grant meant for the interactive sandbox.
    fn effective_net(&self, _req: &ExecRequest) -> NetPolicy {
        if self.hermetic {
            return self.net;
        }
        if kernel::network_once_active() || self.net_grant.granted() {
            NetPolicy::Allow
        } else {
            self.net
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
        if self.effective_net(req) == NetPolicy::Deny {
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

    /// Inert registration only: `create` applies isolation and records the name
    /// but starts nothing, so Gate can register before deciding whether to run.
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
        if !self.hermetic && self.net_grant.granted() {
            return kernel::Containment::OsFsJail;
        }
        match self.net {
            NetPolicy::Deny => kernel::Containment::OsFsJailNoNet,
            NetPolicy::Allow => kernel::Containment::OsFsJail,
        }
    }
    fn denies_network(&self, req: &ExecRequest) -> bool {
        self.effective_net(req) == NetPolicy::Deny
    }
}

/// Single-quote an argument for safe embedding in a remote shell command.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Opt-in SSH backend: remote execution, not local isolation. Assumes the
/// workspace already exists on the remote; local policy still gates dispatch.
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

/// Pick an execution backend from config. `Native` degrades to `Host` where no
/// native sandbox exists (Windows), as do misconfigured `Container`/`Ssh`.
/// Callers validate and warn, so isolation is never silently assumed.
pub fn select_backend(
    cfg: &SandboxConfig,
    _extra_writable: Vec<PathBuf>,
    _approved: ApprovedRoots,
    net_grant: NetworkGrant,
) -> std::sync::Arc<dyn ExecBackend> {
    use std::sync::Arc;
    match cfg.backend {
        BackendKind::Host => Arc::new(HostBackend),
        BackendKind::Native => {
            #[cfg(target_os = "macos")]
            {
                if native_backend_available() {
                    Arc::new(
                        SeatbeltBackend::new(cfg.net, _extra_writable, _approved)
                            .with_network_grant(net_grant),
                    )
                } else {
                    Arc::new(HostBackend)
                }
            }
            #[cfg(target_os = "linux")]
            {
                if native_backend_available() {
                    Arc::new(
                        LandlockBackend::new(cfg.net, _extra_writable, _approved)
                            .with_network_grant(net_grant),
                    )
                } else {
                    Arc::new(HostBackend)
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = net_grant;
                Arc::new(HostBackend)
            }
        }
        BackendKind::Container => match cfg.image.as_deref() {
            Some(image) if !image.is_empty() => Arc::new(
                ContainerBackend::new(
                    detect_container_runtime(&cfg.runtime),
                    image.to_string(),
                    cfg.net,
                    cfg.memory.clone(),
                    cfg.pids,
                )
                .with_network_grant(net_grant),
            ),
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

/// True if this machine can apply an OS sandbox at all, probed with a permissive
/// profile. `false` is a platform property, not a policy decision.
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

/// True if the generated native profile applies and can run a command.
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
            let request = ExecRequest {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.clone()],
                cwd: ws,
                env: Vec::new(),
                clear_env: false,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
            };
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &backend.profile(&request), "/bin/sh", "-c", &script])
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
            read_roots: Vec::new(),
            write_roots: Vec::new(),
        }
    }

    /// Model commands must not consume input belonging to the Medha surface.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_commands_do_not_consume_surface_stdin() {
        const CHILD: &str = "MEDHA_TEST_STDIN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Explicit command input must remain usable despite detaching the
            // surface's stdin. Exercise both a pipeline and shell redirection.
            for script in ["printf supplied | cat", "cat <<'EOF'\nsupplied\nEOF"] {
                let supplied = run_shell_bounded(
                    script,
                    &std::env::temp_dir(),
                    std::time::Duration::from_secs(5),
                    1024,
                    None,
                )
                .await
                .unwrap();
                assert!(supplied.passed(), "{supplied:?}");
                assert_eq!(supplied.output.trim(), "supplied");
            }
            let output = run_shell_bounded(
                "if read -r line; then printf 'stole:%s' \"$line\"; else printf eof; fi",
                &std::env::temp_dir(),
                std::time::Duration::from_secs(5),
                1024,
                None,
            )
            .await
            .unwrap();
            assert!(output.passed(), "{output:?}");
            assert_eq!(output.output, "eof");
            return;
        }
        // Supply the test subprocess a real stdin stream. Ordinary test runners
        // often have /dev/null as stdin, which would hide accidental inheritance.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "exec::tests::shell_commands_do_not_consume_surface_stdin",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"user prompt\n")
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A timed-out command must stop its whole process tree.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = std::env::temp_dir().join(format!("medha-killpg-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived.txt");
        let script = format!("(sleep 1; touch {}) & wait", marker.display());
        let fut = HostBackend.run(req("/bin/sh", &["-c", &script], dir.clone()));
        let r = tokio::time::timeout(std::time::Duration::from_millis(150), fut).await;
        assert!(r.is_err(), "outer timeout should elapse");
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
        // Wait for startup before cancelling to avoid a scheduler race.
        let script = format!(
            "(sleep 3; touch {}) & touch {}; wait",
            marker.display(),
            started.display()
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let trigger = cancel.clone();
        let probe = started.clone();
        tokio::spawn(async move {
            for _ in 0..500 {
                if probe.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            trigger.cancel();
        });
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
        let process = spawn_background(command, false).unwrap();
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
        let profile = backend.profile(&req("/bin/true", &[], workspace.clone()));
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_sandbox_includes_installed_version_manager_payloads() {
        let Some(home) = home_dir_from_env() else {
            return;
        };
        let roots = native_toolchain_read_roots();
        for relative in [
            ".pyenv/versions",
            ".nvm/versions",
            ".volta/tools",
            ".sdkman/candidates",
        ] {
            let expected = home.join(relative);
            if expected.exists() {
                assert!(
                    roots.contains(&expected),
                    "installed version-manager payload was absent: {}",
                    expected.display()
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_can_execute_an_installed_pyenv_interpreter() {
        if !native_sandbox_supported() {
            return;
        }
        let Some(home) = home_dir_from_env() else {
            return;
        };
        let Ok(versions) = std::fs::read_dir(home.join(".pyenv/versions")) else {
            return;
        };
        let Some(python) = versions
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("bin/python3"))
            .find(|path| path.exists())
        else {
            return;
        };
        let ws = std::env::temp_dir().join(format!("medha-pyenv-sandbox-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&ws).unwrap();
        let backend = SeatbeltBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default());
        let output = backend
            .run(req(
                python.to_str().unwrap(),
                &["-c", "print('pyenv-ok')"],
                ws.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(
            output.status,
            Some(0),
            "pyenv interpreter was unreadable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "pyenv-ok");
        std::fs::remove_dir_all(&ws).ok();
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
            &ws,
            &ApprovedRoots::default(),
            permissions::PermissionType::Read,
        );
        assert_eq!(
            candidates,
            vec![outside.canonicalize().unwrap()],
            "a denied file must widen to its parent directory"
        );
        let reporter_prefixed = format!("/bin/sh: {}: Operation not permitted", target.display());
        assert_eq!(
            escalation_candidates(
                &denied_output(&reporter_prefixed),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Write,
            ),
            vec![outside.canonicalize().unwrap()],
            "a reporter executable already readable by the native profile is not a candidate"
        );

        // Success output or unrelated stderr must never produce candidates.
        assert!(
            escalation_candidates(
                &denied_output("cat: /nonexistent-dir-zz/f: No such file or directory"),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Read,
            )
            .is_empty()
        );

        // An in-workspace denial is not escalatable (nothing to approve).
        let inside = ws.join("f.txt");
        std::fs::write(&inside, "x").unwrap();
        let stderr = format!("cat: {}: Operation not permitted", inside.display());
        assert!(
            escalation_candidates(
                &denied_output(&stderr),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Read,
            )
            .is_empty()
        );

        // An already-approved root cannot be the cause; it is excluded.
        let approved = ApprovedRoots::default();
        approved.allow_read(outside.canonicalize().unwrap());
        let stderr = format!("cat: {}: Operation not permitted", target.display());
        assert!(
            escalation_candidates(
                &denied_output(&stderr),
                &ws,
                &approved,
                permissions::PermissionType::Read,
            )
            .is_empty()
        );
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
            escalation_candidates(
                &denied_output(&stderr),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Read,
            )
            .is_empty(),
            "credential paths must never reach an approval card"
        );
    }

    #[cfg(unix)]
    #[test]
    fn escalation_requires_the_denial_to_name_the_unapproved_path() {
        let ws = std::env::temp_dir();
        assert!(
            escalation_candidates(
                &denied_output("application: Permission denied"),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Write,
            )
            .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_write_targets_escalate_their_existing_parent() {
        let base = std::env::temp_dir().join(format!("medha-write-escal-{}", ulid::Ulid::new()));
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("new.txt");
        let stderr = format!("touch: {}: Operation not permitted", target.display());
        assert_eq!(
            escalation_candidates(
                &denied_output(&stderr),
                &ws,
                &ApprovedRoots::default(),
                permissions::PermissionType::Write,
            ),
            vec![outside.canonicalize().unwrap()]
        );
        std::fs::remove_dir_all(&base).ok();
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn request_scoped_reads_defensively_reject_sensitive_roots() {
        let Some(home) = home_dir_from_env() else {
            return;
        };
        assert!(safe_request_readable(&[home.join(".ssh")]).is_empty());
        assert!(safe_request_readable(&[home.join(".aws")]).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_selection_degrades_to_host_when_seatbelt_cannot_apply() {
        if native_backend_available() {
            return;
        }
        let backend = select_backend(
            &SandboxConfig::default(),
            vec![],
            ApprovedRoots::default(),
            NetworkGrant::default(),
        );
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
        // Host secrets must not enter the container.
        assert!(!joined.contains("TAVILY_API_KEY") && !joined.contains("supersecret"));
        // The command follows the image, in order.
        let img = argv.iter().position(|a| a == "alpine").unwrap();
        assert_eq!(
            &argv[img + 1..],
            &["sh".to_string(), "-c".to_string(), "echo hi".to_string()]
        );
    }

    #[test]
    fn network_denial_signature_gates_on_config_intent_and_markers() {
        // A resolver failure under a net-denying backend is a signal.
        assert!(network_denial_signature(
            "",
            "getaddrinfo ENOTFOUND registry.example",
            true,
        ));
        // Same failure when the backend is not denying network: never a signal.
        assert!(!network_denial_signature(
            "",
            "getaddrinfo ENOTFOUND registry.example",
            false,
        ));
        // A masked zero exit (`… | tail; echo`) still signals — the marker, not
        // the exit code, is the evidence.
        assert!(network_denial_signature(
            "npm error code ENOTFOUND\nexit=0",
            "",
            true,
        ));
        // An unrelated failure is not a network denial.
        assert!(!network_denial_signature("", "syntax error", true));
    }

    /// The regression that motivated deriving markers from libc: these are the
    /// verbatim failures `pip` and `cargo` produce, and on darwin neither
    /// contains a single string the hand-written list carried.
    #[test]
    fn resolver_failures_are_matched_whatever_runtime_printed_them() {
        // This platform's own libc sentence, whatever it happens to be, must be
        // covered without anyone having typed it. On darwin that is pip's
        // "nodename nor servname provided"; on Linux, "name or service not known".
        // SAFETY: as in `build_network_denial_markers` — a static string.
        #[cfg(unix)]
        {
            let native = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(libc::EAI_NONAME)) }
                .to_string_lossy()
                .into_owned();
            assert!(
                network_denial_signature(
                    "",
                    &format!("connection failed: [Errno 8] {native}"),
                    true
                ),
                "this platform's own EAI_NONAME text must match: {native}"
            );
        }
        for output in [
            "Temporary failure in name resolution",
            "warning: spurious network error: failed to resolve address for example.invalid: \
             nodename nor servname provided, or not known; class=Net (12)",
            "fatal: unable to access 'https://example.invalid/': Could not resolve host: example.invalid",
            "dial tcp: lookup proxy.golang.org: no such host",
            "getaddrinfo EAI_AGAIN registry.example",
        ] {
            assert!(
                network_denial_signature("", output, true),
                "unmatched resolver failure: {output}"
            );
        }
    }

    /// Every marker is a substring test against arbitrary command output, so a
    /// too-generic entry silently turns ordinary compiler noise into a grant card.
    /// Landlock denies TCP `connect` and leaves UDP alone, so a Linux net-deny
    /// resolves the name fine and fails with a permission error carrying no
    /// resolver wording at all. These are the shapes that reaches us.
    #[test]
    fn a_refused_socket_is_matched_even_with_no_resolver_wording() {
        for output in [
            "npm error request to https://registry.npmjs.org/x failed, reason: connect EACCES 104.16.24.35:443",
            "curl: (7) Failed to connect to example.com port 443 after 1 ms: Permission denied",
            "Failed to establish a new connection: [Errno 13] Permission denied')': /simple/x/",
            "OSError: [Errno 1] Operation not permitted: connect",
        ] {
            assert!(
                network_denial_signature("", output, true),
                "unmatched socket refusal: {output}"
            );
        }
    }

    /// The conjunction must stay disjoint from the filesystem-denial signature,
    /// or a blocked file read offers a network grant that cannot help.
    #[test]
    fn a_refused_file_is_never_a_refused_socket() {
        for output in [
            "open '/etc/shadow': Permission denied",
            "mkdir: cannot create directory '/opt/x': Permission denied",
            "EACCES: permission denied, open '/private/etc/hosts'",
            // Operation and refusal on separate lines are not one event.
            "connect to the server\nchmod: /root: Operation not permitted",
        ] {
            assert!(
                !network_denial_signature("", output, true),
                "filesystem denial misread as a network denial: {output}"
            );
        }
    }

    #[test]
    fn live_scan_matches_a_refused_socket_line_split_across_reads() {
        let mut scan = DenialScan::new();
        scan.feed(b"npm error request to https://registry.npmjs.org/pptxgenjs failed, ");
        assert!(!scan.hit, "half a line is not yet a match");
        scan.feed(b"reason: connect EACCES 104.16.24.35:443\n");
        assert!(
            scan.hit,
            "a refused-socket line spanning two reads must still match"
        );
    }

    #[test]
    fn ordinary_build_failures_do_not_look_like_a_denied_network() {
        for output in [
            "error[E0433]: failed to resolve: use of undeclared crate or module `foo`",
            "error: linking with `cc` failed: exit status: 1",
            "Operation not permitted (os error 1)",
            "curl: (7) Failed to connect to localhost port 8080",
            "System error",
            "npm ERR! 404 Not Found - GET https://registry.npmjs.org/nope",
        ] {
            assert!(
                !network_denial_signature("", output, true),
                "false network denial on: {output}"
            );
        }
    }

    #[test]
    fn derived_markers_carry_this_platform_resolver_text() {
        let markers = &*NETWORK_DENIAL_MARKERS;
        assert!(
            markers
                .iter()
                .all(|m| m.len() >= 3 && m == &m.to_ascii_lowercase()),
            "markers must be lowercase and non-trivial: {markers:?}"
        );
        #[cfg(unix)]
        {
            // SAFETY: as in `build_network_denial_markers` — a static string.
            let noname = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(libc::EAI_NONAME)) }
                .to_string_lossy()
                .to_ascii_lowercase();
            assert!(
                markers.contains(&noname),
                "the platform's own EAI_NONAME text must be a marker; had {markers:?}"
            );
        }
    }

    #[test]
    fn live_scan_matches_a_marker_split_across_two_reads() {
        let mut scan = DenialScan::new();
        scan.feed(b"npm error code ENOTF");
        assert!(!scan.hit, "half a marker is not a match");
        scan.feed(b"OUND\nnpm error syscall getaddrinfo\n");
        assert!(
            scan.hit,
            "a marker spanning two pipe reads must still match"
        );
    }

    #[test]
    fn live_scan_is_sticky_and_ignores_later_clean_output() {
        let mut scan = DenialScan::new();
        scan.feed(b"could not resolve host: registry.example\n");
        assert!(scan.hit);
        scan.feed(b"...retrying\n");
        assert!(scan.hit, "a hit must survive subsequent output");
    }

    #[tokio::test]
    async fn a_running_task_reports_its_resolver_failure_before_it_exits() {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("echo 'getaddrinfo ENOTFOUND registry.example' >&2; sleep 30");
        let process = spawn_background(command, true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !process.network_denial_seen() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            process.network_denial_seen(),
            "the failure must be visible while the command is still running"
        );
        assert!(process.is_running(), "detection must not require an exit");
        assert!(process.bytes_seen() > 0);
        process.kill();
        process.wait().await;
    }

    #[tokio::test]
    async fn detection_stays_disarmed_when_the_backend_allows_network() {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("echo 'getaddrinfo ENOTFOUND registry.example' >&2");
        let process = spawn_background(command, false).unwrap();
        process.wait().await;
        assert!(
            !process.network_denial_seen(),
            "a box that permits network must never offer a grant"
        );
    }

    #[test]
    fn container_network_grant_opens_the_box_but_hermetic_stays_denied() {
        let grant = NetworkGrant::default();
        let be = ContainerBackend::new(
            "docker".into(),
            "alpine".into(),
            NetPolicy::Deny,
            None,
            None,
        )
        .with_network_grant(grant.clone());
        let r = req("sh", &["-c", "true"], std::env::temp_dir());
        assert!(
            be.build_run_argv(&r).join(" ").contains("--network none"),
            "net=deny with no grant denies the network"
        );
        grant.grant();
        assert!(
            !be.build_run_argv(&r).join(" ").contains("--network none"),
            "a session grant opens the container network"
        );
        assert!(!be.denies_network(&r), "granted → no longer denies network");

        // A hermetic verification container ignores the grant entirely.
        let hermetic = ContainerBackend::new_hermetic(
            "docker".into(),
            "img".into(),
            None,
            None,
            "check".into(),
        )
        .with_network_grant(grant.clone());
        assert!(
            hermetic.effective_net(&r) == NetPolicy::Deny,
            "hermetic verification never opens on a shared grant"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_drops_the_deny_rule_once_network_is_granted() {
        let grant = NetworkGrant::default();
        let be = SeatbeltBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default())
            .with_network_grant(grant.clone());
        let r = req("sh", &["-c", "true"], std::env::temp_dir());
        assert!(be.profile(&r).contains("(deny network*)"));
        assert_eq!(be.containment(), kernel::Containment::OsFsJailNoNet);
        grant.grant();
        assert!(!be.profile(&r).contains("(deny network*)"));
        assert_eq!(be.containment(), kernel::Containment::OsFsJail);
    }

    /// The one-shot grant reaches the profile only because `build_command` is
    /// polled synchronously inside the task-local scope. Moving the spawn behind
    /// `tokio::spawn`/`spawn_blocking` would silently downgrade "once" to denied,
    /// so this asserts the whole path rather than the task-local alone.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn once_scope_opens_the_profile_and_does_not_outlive_the_future() {
        let be = SeatbeltBackend::new(NetPolicy::Deny, vec![], ApprovedRoots::default());
        let r = req("sh", &["-c", "true"], std::env::temp_dir());
        assert!(be.profile(&r).contains("(deny network*)"));

        let opened = kernel::network_once_scope(async { be.profile(&r) }).await;
        assert!(
            !opened.contains("(deny network*)"),
            "a once-scoped run must reach the network"
        );

        assert!(
            be.profile(&r).contains("(deny network*)"),
            "the grant must not outlive the scoped future"
        );
        assert_eq!(be.containment(), kernel::Containment::OsFsJailNoNet);
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
        // Wrapping must not change the string approved by the policy scanner.
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
