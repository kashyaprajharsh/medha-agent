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
use std::path::PathBuf;

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

fn to_output(o: std::process::Output) -> ExecOutput {
    ExecOutput {
        status: o.status.code(),
        stdout: o.stdout,
        stderr: o.stderr,
    }
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
}

impl Drop for GroupReaper {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed {
            if let Some(pid) = self.pid {
                // Negative pid = the process GROUP led by `pid`.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}

/// Put a command in its own process group (unix) and pipe stdout/stderr, so a
/// spawned child can be supervised and group-killed. `kill_on_drop` is a
/// per-caller choice: foreground runs set it (backstop for the leader);
/// background tasks clear it (they must survive the handle being dropped).
fn configure_for_spawn(cmd: &mut tokio::process::Command, kill_on_drop: bool) {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(kill_on_drop);
}

/// SIGKILL an entire process group by its leader pid (unix). No-op elsewhere.
#[allow(unused_variables)]
fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Spawn a command in its own process group, capture stdout/stderr, and wait for
/// it. If the returned future is dropped before completion — which is exactly
/// what an outer `tokio::time::timeout` or a cancellation does — the
/// [`GroupReaper`] SIGKILLs the whole process tree, so nothing is orphaned.
async fn spawn_and_wait(mut cmd: tokio::process::Command) -> Result<ExecOutput, ExecError> {
    configure_for_spawn(&mut cmd, true); // reaper backstops the group; kill_on_drop the leader
    let child = cmd.spawn().map_err(|e| ExecError::Spawn(e.to_string()))?;
    let mut reaper = GroupReaper::new(child.id());
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| ExecError::Spawn(e.to_string()))?;
    reaper.disarm(); // finished on its own — nothing to tear down
    Ok(to_output(out))
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
            kill_group(pid);
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

/// True if `program` resolves on the current PATH (or is an existing absolute
/// path). Used to detect an installed container runtime.
pub fn program_on_path(program: &str) -> bool {
    let p = std::path::Path::new(program);
    if p.is_absolute() {
        return p.exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).exists()))
        .unwrap_or(false)
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
}
