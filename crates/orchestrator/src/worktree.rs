//! Writer isolation (§6.4) — a child that modifies code gets its own checkout.
//!
//! Read-only children may share the parent's tree; writers may not. Two agents
//! editing one working tree collide through files, the index and cwd, and the
//! damage is silent: whichever wrote last wins and neither knows. So a writer is
//! given a `git worktree` cut from the parent's HEAD, works only there, and
//! returns a patch. Nothing it does touches the parent's tree until a human
//! approves the merge.
//!
//! Two writers cannot share a worktree *structurally*: the path is derived from
//! the child's session ULID, and [`WorktreePool`] refuses a second lease on a
//! path it already owns. Reaping is a [`Drop`] responsibility plus a sweep at
//! startup — the same reap-on-drop discipline the MCP and LSP managers use,
//! because an orphaned worktree wedges the next `git worktree add` on that path.

use std::collections::HashSet;
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fd_lock::RwLock as FileRwLock;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use ulid::Ulid;

/// Branch prefix for agent worktrees. Namespaced so a sweep can recognise its
/// own leftovers without guessing, and so `git branch` reads intelligibly.
pub const BRANCH_PREFIX: &str = "medha/agent";

/// Where the owning process is recorded for a checkout: a *sibling* of the
/// worktree, never a file inside it. Anything inside is part of the child's
/// tree and lands in its diff — an ownership marker would make every patch
/// non-empty, including an idle child's.
fn owner_marker(worktree: &Path) -> PathBuf {
    let mut marker = worktree.as_os_str().to_os_string();
    marker.push(".owner");
    PathBuf::from(marker)
}

/// Rust's Windows `canonicalize` returns an extended-length (`\\?\\...`)
/// spelling. The Win32 APIs accept that spelling, but Git for Windows does not
/// consistently accept it as a `worktree add` destination on hosted runners.
/// Keep the canonicalization (it prevents aliasing) while handing Git the
/// ordinary drive/UNC spelling for paths that are within the normal path range.
#[cfg(windows)]
fn git_worktree_path(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    if let Some(local) = raw.strip_prefix(r"\\?\") {
        return PathBuf::from(local);
    }
    path
}

/// Marks a checkout whose work was never captured as a patch. A sibling, like
/// the owner marker, so it stays out of the child's diff.
fn keep_marker(worktree: &Path) -> PathBuf {
    let mut marker = worktree.as_os_str().to_os_string();
    marker.push(".keep");
    PathBuf::from(marker)
}

/// Whether this checkout holds work that exists nowhere else.
///
/// On disk rather than in memory, because the process that decided to keep it
/// is usually gone by the time anything sweeps: the in-memory lease dies with
/// it, and the next launch would read a live rescue as an abandoned directory
/// and force it away — losing exactly what the rescue was for.
fn keeps_work(worktree: &Path) -> bool {
    keep_marker(worktree).exists()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnerRecord {
    pid: u32,
    /// Windows process creation time in 100ns FILETIME ticks. Pairing this
    /// with the PID distinguishes the process that created the checkout from a
    /// later, unrelated process which inherited the recycled PID.
    creation_time: Option<u64>,
}

fn parse_owner_record(text: &str) -> Option<OwnerRecord> {
    let mut fields = text.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let creation_time = fields.next().and_then(|field| field.parse().ok());
    Some(OwnerRecord { pid, creation_time })
}

#[cfg(windows)]
fn windows_process_creation_time(process: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    (ok != 0).then_some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

#[cfg(windows)]
fn owner_record_for_current_process() -> OwnerRecord {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    OwnerRecord {
        pid: std::process::id(),
        creation_time: windows_process_creation_time(unsafe { GetCurrentProcess() }),
    }
}

#[cfg(not(windows))]
fn owner_record_for_current_process() -> OwnerRecord {
    OwnerRecord {
        pid: std::process::id(),
        creation_time: None,
    }
}

fn owner_record_text() -> String {
    let record = owner_record_for_current_process();
    match record.creation_time {
        Some(creation_time) => format!("{} {creation_time}", record.pid),
        None => record.pid.to_string(),
    }
}

#[cfg(windows)]
fn windows_owner_alive(record: OwnerRecord) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    // SYNCHRONIZE is a standard access right shared by every securable object,
    // and windows-sys files those under Storage::FileSystem — it is not in the
    // Threading module despite being needed to wait on a process handle.
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            record.pid,
        )
    };
    if process.is_null() {
        return match unsafe { GetLastError() } {
            ERROR_INVALID_PARAMETER => false,
            // Protected processes can refuse a query even while alive. An
            // uncertain owner is retained; only a proven-dead or mismatched
            // identity may be reaped.
            ERROR_ACCESS_DENIED => true,
            _ => true,
        };
    }

    let creation_matches = record.creation_time.is_none_or(|expected| {
        windows_process_creation_time(process).is_none_or(|actual| actual == expected)
    });
    let wait = unsafe { WaitForSingleObject(process, 0) };
    unsafe {
        CloseHandle(process);
    }
    if !creation_matches {
        return false;
    }
    match wait {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 => false,
        // WAIT_FAILED/unknown is not evidence that the owner died.
        _ => true,
    }
}

/// Whether the process that claimed `worktree` is still running.
///
/// A worktree is only abandoned if its owner is gone. Without this check a
/// second Medha in the same repository reads another's live checkout as stale
/// — it holds no lease for it — and removes it while a child is editing.
fn owner_alive(worktree: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(owner_marker(worktree)) else {
        // No marker: either a pre-existing checkout from before this mechanism,
        // or a crash between `worktree add` and the write. Treated as abandoned,
        // which is the same behaviour as before and safe for both.
        return false;
    };
    let Some(record) = parse_owner_record(&text) else {
        return false;
    };
    #[cfg(unix)]
    {
        // Signal 0 tests for existence without delivering anything.
        unsafe { libc::kill(record.pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        windows_owner_alive(record)
    }
    #[cfg(not(any(unix, windows)))]
    {
        // No cheap portable liveness test; err towards keeping a checkout that
        // might be live, since the cost of a leaked directory is far below the
        // cost of deleting work in progress.
        true
    }
}

/// Cap on the diff handed back to the parent, in bytes. A patch past this is
/// still returned whole to the caller — which owns the artifact store — but
/// [`Patch::is_large`] flags it so the summary path can spill instead of
/// flooding a context window.
pub const LARGE_PATCH_BYTES: usize = 64 * 1024;
/// Default hard ceiling for an extracted patch. A diff is duplicated into the
/// result and durable event, so allowing it to grow with the checkout makes a
/// single generated file an OOM vector.
pub const DEFAULT_MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;
/// A credential helper, hook, filesystem, or Git itself must never hold agent
/// settlement forever. This is intentionally long enough for a large local
/// diff while still being a hard operational ceiling.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Once a process tree has been killed, pipes and the direct child should
/// settle promptly. A second bound prevents cleanup from becoming a new hang.
const GIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ordinary Git metadata is bounded independently from patches. Patch output
/// uses the caller's stricter configured ceiling.
const GIT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const GIT_STDERR_LIMIT: usize = 64 * 1024;
/// A structural checkout/sweep lock is normally held for milliseconds, but it
/// shares Git's outer ceiling so a crashed or wedged peer cannot block forever.
const STRUCTURE_LOCK_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("writer agents need a git repository; '{0}' is not inside one")]
    NotARepo(PathBuf),
    #[error("the repository has no commits yet — commit once before delegating writes")]
    NoCommits,
    #[error("worktree path '{0}' is already leased to another agent")]
    AlreadyLeased(PathBuf),
    #[error("git {command} failed: {message}")]
    Git { command: String, message: String },
    #[error("could not run git: {0}")]
    Spawn(String),
    #[error("git {command} timed out after {seconds}s and its process tree was stopped")]
    GitTimeout { command: String, seconds: u64 },
    #[error("git {command} was cancelled and its process tree was stopped")]
    GitCancelled { command: String },
    #[error("git {command} produced more than the configured {limit}-byte output limit")]
    GitOutputTooLarge { command: String, limit: usize },
    #[error("timed out waiting for the repository worktree lock")]
    WorktreeLockTimeout,
    #[error("io error: {0}")]
    Io(String),
    #[error("patch exceeds the configured {limit}-byte limit; the checkout was preserved instead")]
    PatchTooLarge { limit: usize },
}

#[derive(Debug)]
struct BoundedCapture {
    bytes: Vec<u8>,
    overflowed: bool,
}

async fn drain_bounded<R>(mut reader: R, limit: usize) -> std::io::Result<BoundedCapture>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if bytes.len() < limit {
            let keep = (limit - bytes.len()).min(read);
            bytes.extend_from_slice(&chunk[..keep]);
        }
    }
    Ok(BoundedCapture {
        bytes,
        overflowed: total > limit,
    })
}

/// A whole process group is the cancellation unit. `kill_on_drop` only reaches
/// the leader, so this synchronous guard covers every abnormal future/task
/// drop; the owned supervisor performs the awaited reap on ordinary paths.
struct GitGroupReaper {
    pid: Option<u32>,
    armed: bool,
}

impl GitGroupReaper {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GitGroupReaper {
    fn drop(&mut self) {
        if self.armed
            && let Some(pid) = self.pid
        {
            kill_git_process_tree(pid);
        }
    }
}

fn kill_git_process_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let taskkill = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("taskkill.exe");
        if let Ok(mut child) = std::process::Command::new(taskkill)
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = std::time::Instant::now() + GIT_SETTLE_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
    }
}

fn configure_git_process(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command
            .as_std_mut()
            .creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
}

struct CancelGitOnDrop(Option<CancellationToken>);

impl CancelGitOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelGitOnDrop {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

#[derive(Debug)]
struct GitOutput {
    status: std::process::ExitStatus,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
}

/// Run one fixed Git subprocess under an owned, cancellation-safe supervisor.
/// The supervisor outlives a dropped caller long enough to kill/reap the whole
/// process group and drain bounded pipes.
async fn run_git_program(
    program: PathBuf,
    dir: PathBuf,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    env: Option<(OsString, OsString)>,
    stdout_limit: usize,
    deadline: Duration,
) -> Result<GitOutput, WorktreeError> {
    run_git_program_observed(program, dir, args, stdin, env, stdout_limit, deadline, None).await
}

#[allow(clippy::too_many_arguments)]
async fn run_git_program_observed(
    program: PathBuf,
    dir: PathBuf,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    env: Option<(OsString, OsString)>,
    stdout_limit: usize,
    deadline: Duration,
    completion: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<GitOutput, WorktreeError> {
    let command_name = args.join(" ");
    let mut command = tokio::process::Command::new(program);
    command.args(&args).current_dir(dir);
    if let Some((key, value)) = env {
        command.env(key, value);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    configure_git_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| WorktreeError::Spawn(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorktreeError::Spawn("git stdout was not captured".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorktreeError::Spawn("git stderr was not captured".into()))?;
    let child_stdin = child.stdin.take();
    let pid = child.id();
    let reaper = GitGroupReaper::new(pid);
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task_command = command_name.clone();

    let supervisor = tokio::spawn(async move {
        struct NotifyCompletion(Option<Arc<tokio::sync::Semaphore>>);
        impl Drop for NotifyCompletion {
            fn drop(&mut self) {
                if let Some(semaphore) = self.0.take() {
                    semaphore.add_permits(1);
                }
            }
        }
        let _completion = NotifyCompletion(completion);
        let mut reaper = reaper;
        // If any child-wait/settle branch returns early, dropping a bare
        // JoinHandle would detach these pipe pumps forever. Keep them
        // abort-on-drop so the supervisor owns their complete lifetime.
        let mut io_task = AbortOnDropHandle::new(tokio::spawn(async move {
            let stdout = drain_bounded(stdout, stdout_limit);
            let stderr = drain_bounded(stderr, GIT_STDERR_LIMIT);
            let write_stdin = async move {
                if let (Some(mut writer), Some(body)) = (child_stdin, stdin) {
                    if let Err(error) = writer.write_all(&body).await {
                        // Git may reject a patch and close stdin before the
                        // producer finishes. Preserve its exit status/stderr;
                        // BrokenPipe is the expected consequence, not the root
                        // diagnostic.
                        if error.kind() != ErrorKind::BrokenPipe {
                            return Err(error);
                        }
                    }
                    if let Err(error) = writer.shutdown().await
                        && error.kind() != ErrorKind::BrokenPipe
                    {
                        return Err(error);
                    }
                }
                Ok::<(), std::io::Error>(())
            };
            let (stdout, stderr, stdin) = tokio::join!(stdout, stderr, write_stdin);
            Ok::<_, std::io::Error>((stdout?, stderr?, stdin?))
        }));

        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        let end = tokio::select! {
            status = child.wait() => End::Exited(status),
            _ = tokio::time::sleep(deadline) => End::TimedOut,
            _ = task_cancellation.cancelled() => End::Cancelled,
        };

        // Only tear down the process tree when the command did not finish
        // normally. On Windows `git.exe` can be a launcher with helper
        // processes still completing the operation after the leader exits;
        // taskkill /T against the just-exited PID can terminate those helpers
        // and turn a successful `git worktree add` into a partial failure.
        if matches!(&end, End::TimedOut | End::Cancelled) {
            if let Some(pid) = pid {
                kill_git_process_tree(pid);
            }
            let _ = child.start_kill();
        }

        let (status, terminal_error) = match end {
            End::Exited(status) => (
                Some(status.map_err(|e| WorktreeError::Spawn(e.to_string()))?),
                None,
            ),
            End::TimedOut => (
                None,
                Some(WorktreeError::GitTimeout {
                    command: task_command.clone(),
                    seconds: deadline.as_secs(),
                }),
            ),
            End::Cancelled => (
                None,
                Some(WorktreeError::GitCancelled {
                    command: task_command.clone(),
                }),
            ),
        };

        let status = match status {
            Some(status) => status,
            None => tokio::time::timeout(GIT_SETTLE_TIMEOUT, child.wait())
                .await
                .map_err(|_| {
                    WorktreeError::Spawn("git did not settle after process-tree kill".into())
                })?
                .map_err(|error| WorktreeError::Spawn(error.to_string()))?,
        };
        let (stdout, stderr, ()) = tokio::time::timeout(GIT_SETTLE_TIMEOUT, &mut io_task)
            .await
            .map_err(|_| {
                WorktreeError::Spawn("git pipes did not settle after process-tree kill".into())
            })?
            .map_err(|error| WorktreeError::Spawn(format!("git pipe task failed: {error}")))?
            .map_err(|error| WorktreeError::Io(error.to_string()))?;

        // All output pipes have settled, so a normally exited command has no
        // descendants that can still hold inherited handles. Keep the guard
        // armed until this point so an early error still tears down the tree.
        reaper.disarm();

        if let Some(error) = terminal_error {
            return Err(error);
        }
        Ok(GitOutput {
            status,
            stdout,
            stderr,
        })
    });

    let mut cancel_on_drop = CancelGitOnDrop(Some(cancellation));
    let result = supervisor
        .await
        .map_err(|error| WorktreeError::Spawn(format!("git supervisor failed: {error}")))?;
    cancel_on_drop.disarm();
    result
}

async fn run_git(
    dir: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    env: Option<(OsString, OsString)>,
    stdout_limit: usize,
) -> Result<GitOutput, WorktreeError> {
    run_git_program(
        PathBuf::from("git"),
        dir.to_path_buf(),
        args.iter().map(|arg| (*arg).to_string()).collect(),
        stdin,
        env,
        stdout_limit,
        GIT_COMMAND_TIMEOUT,
    )
    .await
}

fn git_failure(args: &[&str], output: &GitOutput) -> WorktreeError {
    let mut message = String::from_utf8_lossy(&output.stderr.bytes)
        .trim()
        .to_string();
    if output.stderr.overflowed {
        message.push_str("\n[git stderr truncated at 65536 bytes]");
    }
    WorktreeError::Git {
        command: args.join(" "),
        message: if message.is_empty() {
            format!("exit {:?}", output.status.code())
        } else {
            message
        },
    }
}

/// Run a fixed git command while draining all output and retaining at most the
/// patch limit. A patch is either complete or rejected; it is never truncated.
async fn git_bounded(dir: &Path, args: &[&str], limit: usize) -> Result<String, WorktreeError> {
    let output = run_git(dir, args, None, None, limit).await?;
    if output.stdout.overflowed {
        return Err(WorktreeError::PatchTooLarge { limit });
    }
    if !output.status.success() {
        return Err(git_failure(args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_string())
}

/// Run git in `dir` with bounded output, a deadline, and process-tree teardown.
async fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = run_git(dir, args, None, None, GIT_OUTPUT_LIMIT).await?;
    if output.stdout.overflowed {
        return Err(WorktreeError::GitOutputTooLarge {
            command: args.join(" "),
            limit: GIT_OUTPUT_LIMIT,
        });
    }
    if !output.status.success() {
        return Err(git_failure(args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_string())
}

async fn repository_structure_lock_path(repo: &Path) -> Result<PathBuf, WorktreeError> {
    let common = PathBuf::from(git(repo, &["rev-parse", "--git-common-dir"]).await?);
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    };
    let common = common
        .canonicalize()
        .map_err(|error| WorktreeError::Io(error.to_string()))?;
    Ok(common.join("medha-worktrees.lock"))
}

/// Serialize Git's worktree registry and the matching owner markers across
/// every Medha process using this repository, even when two instances use
/// different `MEDHA_HOME` state roots. The lock lives in Git's common metadata
/// directory, is crash-released by the OS, and is polled asynchronously.
async fn with_structure_lock<T, F, Fut>(lock_path: &Path, operation: F) -> Result<T, WorktreeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, WorktreeError>>,
{
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| WorktreeError::Io(error.to_string()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| WorktreeError::Io(error.to_string()))?;
    let mut lock = FileRwLock::new(file);
    let started = tokio::time::Instant::now();
    let guard = loop {
        match lock.try_write() {
            Ok(guard) => break guard,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() >= STRUCTURE_LOCK_TIMEOUT {
                    return Err(WorktreeError::WorktreeLockTimeout);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(WorktreeError::Io(error.to_string())),
        }
    };
    let result = operation().await;
    drop(guard);
    result
}

/// What a writer child hands back instead of a prose summary of its edits.
///
/// A summary of a change cannot be reviewed, verified or applied; a diff can.
/// The verification evidence travels with it because a patch that has not been
/// built is not a finished patch — §6.4 requires both, and the merge gate reads
/// `verified` rather than trusting the child's account of itself.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Patch {
    /// Unified diff against `base`, empty when the child changed nothing.
    pub diff: String,
    /// The commit the worktree was cut from. A patch is only meaningful
    /// relative to its base, and three-way merge needs it by name.
    pub base: String,
    /// Paths touched, for the merge preview and for conflict reporting.
    pub files: Vec<String>,
    /// What the child ran to prove the change works, and what came back.
    /// `None` means it never ran anything — which the gate must show, because
    /// "no evidence" and "evidence of success" are not the same answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.diff.trim().is_empty()
    }

    /// Whether this diff is too big to hand to a model inline.
    pub fn is_large(&self) -> bool {
        self.diff.len() > LARGE_PATCH_BYTES
    }

    /// Whether a merge gate should treat this as verified. Unverified is the
    /// default: absence of evidence is never evidence of success.
    pub fn verified(&self) -> bool {
        self.verification.as_ref().is_some_and(|v| v.passed)
    }
}

/// Evidence that a patch works, captured from the child's own build/test run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verification {
    pub command: String,
    pub passed: bool,
    /// Tail of the output — enough to see the failure, bounded so a noisy build
    /// cannot flood the parent.
    pub output: String,
}

/// A checkout leased to exactly one writer child.
///
/// Removal happens in [`Drop`] as well as [`Worktree::reap`], so a cancelled or
/// panicking child cannot leave one behind. `git worktree add` fails on a path
/// that already exists, so a leak is not merely untidy — it breaks the next run.
pub struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    base: String,
    structure_lock: PathBuf,
    /// Shared with the pool, so the lease is released by the same guard that
    /// removes the directory and cannot drift out of step with it.
    leases: Leases,
    /// Set when the child's work could not be captured as a patch. Both `reap`
    /// and the `Drop` guard then leave the checkout alone: the directory is the
    /// only remaining copy of the work, and removing it is unrecoverable.
    preserve: std::sync::atomic::AtomicBool,
    max_patch_bytes: usize,
}

type Leases = Arc<Mutex<HashSet<PathBuf>>>;

struct LeaseReservation {
    leases: Leases,
    path: PathBuf,
    committed: bool,
}

impl LeaseReservation {
    fn acquire(leases: &Leases, path: PathBuf) -> Result<Self, WorktreeError> {
        let mut table = leases
            .lock()
            .map_err(|_| WorktreeError::Io("lease table poisoned".into()))?;
        if !table.insert(path.clone()) {
            return Err(WorktreeError::AlreadyLeased(path));
        }
        drop(table);
        Ok(Self {
            leases: Arc::clone(leases),
            path,
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for LeaseReservation {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut leases) = self.leases.lock()
        {
            leases.remove(&self.path);
        }
    }
}

async fn cleanup_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    lock_path: &Path,
) -> Result<(), WorktreeError> {
    with_structure_lock(lock_path, || async {
        if path.exists() {
            git(
                repo,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            )
            .await?;
        }
        // A missing checkout may still be registered; prune before deleting
        // the private branch. Every command has its own lifecycle deadline.
        let _ = git(repo, &["worktree", "prune"]).await;
        let _ = git(repo, &["branch", "-D", branch]).await;
        let _ = std::fs::remove_file(owner_marker(path));
        Ok(())
    })
    .await
}

impl Worktree {
    /// Where the child works. Both its cwd and its sandbox root must be this —
    /// if either still points at the parent, the isolation is decorative.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The commit this checkout was cut from.
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Everything the child changed, as a patch against its base.
    ///
    /// Untracked files are staged with `--intent-to-add` first: without it a
    /// newly created file is invisible to `git diff`, and a child whose whole
    /// job was to add a module would return an empty patch and look like it had
    /// done nothing.
    pub async fn patch(&self) -> Result<Patch, WorktreeError> {
        git(&self.path, &["add", "--all", "--intent-to-add", "."]).await?;
        let diff = git_bounded(
            &self.path,
            // Binary files cannot be applied from a textual patch, so a marker
            // beats a corrupt hunk. `--no-color` because a configured
            // `color.ui = always` would otherwise embed escape codes in a diff
            // that has to survive being parsed and re-applied.
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--binary",
                &self.base,
            ],
            self.max_patch_bytes,
        )
        .await?;
        let files = git(
            &self.path,
            &["diff", "--name-only", "--no-ext-diff", &self.base],
        )
        .await?
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect();
        Ok(Patch {
            diff,
            base: self.base.clone(),
            files,
            verification: None,
        })
    }

    /// Remove the checkout and its branch. Idempotent, and safe to call before
    /// the [`Drop`] guard runs — the pool lease is what makes the second call a
    /// no-op rather than a spurious git failure.
    /// Lay a previous patch into this checkout, so a follow-up continues from
    /// where the agent left off.
    ///
    /// Without it a resumed writer starts from a clean HEAD and has to redo —
    /// or silently discard — everything it had already done, which is the one
    /// outcome a follow-up exists to avoid.
    pub async fn restore(&self, patch: &Patch) -> Result<(), WorktreeError> {
        if patch.is_empty() {
            return Ok(());
        }
        apply(&self.path, patch, &["--3way"], None).await
    }

    /// Keep this checkout: its work was never captured, so the directory is the
    /// only copy left. Survives `reap`, the `Drop` guard, and — because the mark
    /// is on disk — the next process's sweep.
    pub fn preserve(&self) {
        self.preserve
            .store(true, std::sync::atomic::Ordering::Release);
        let marker = keep_marker(&self.path);
        if let Err(error) = std::fs::write(
            &marker,
            format!(
                "Medha kept this checkout: an agent's changes could not be captured as a patch.\n\
                 Recover them from here, then delete this file and the directory.\n\
                 branch: {}\nbase: {}\n",
                self.branch, self.base
            ),
        ) {
            // Nothing else can carry this across a restart, so say so loudly
            // rather than leaving a rescue that the next sweep will undo.
            tracing::error!(
                target: "medha_orchestrator",
                path = %self.path.display(), %error,
                "could not mark a checkout as preserved; it may be swept on restart"
            );
        }
    }

    fn preserved(&self) -> bool {
        self.preserve.load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn reap(&self) {
        if self.preserved() || keeps_work(&self.path) {
            if let Ok(mut leases) = self.leases.lock() {
                leases.remove(&self.path);
            }
            return;
        }
        if !self
            .leases
            .lock()
            .map(|l| l.contains(&self.path))
            .unwrap_or(false)
        {
            return;
        }
        // `--force` because the child is expected to leave uncommitted work:
        // the patch has already been taken. Keep the lease on failure so Drop
        // can make one final bounded cleanup attempt.
        match cleanup_worktree(&self.repo, &self.path, &self.branch, &self.structure_lock).await {
            Ok(()) => {
                if let Ok(mut leases) = self.leases.lock() {
                    leases.remove(&self.path);
                }
            }
            Err(error) => tracing::warn!(
                target: "medha_orchestrator",
                path = %self.path.display(), %error,
                "worktree cleanup did not complete; leaving it for the drop/startup sweep"
            ),
        }
    }
}

impl Drop for Worktree {
    /// Last-resort cleanup for the paths `reap` never reaches — a panic, or a
    /// run future dropped mid-cancellation. `Drop` cannot await, so it hands
    /// removal to an owned Tokio task whose Git children retain the same
    /// deadline/process-tree guarantees. Without a runtime it leaves the owner
    /// marker intact for the next startup sweep instead of launching an
    /// unbounded blocking subprocess.
    fn drop(&mut self) {
        // Preserved: hold the lease as well as the directory. Releasing it would
        // let this process's own sweep treat the checkout as abandoned and
        // force it away — the exact loss the flag exists to prevent.
        if self.preserved() || keeps_work(&self.path) {
            return;
        }
        let held = self
            .leases
            .lock()
            .map(|mut leases| leases.remove(&self.path))
            .unwrap_or(false);
        if !held {
            return;
        }
        let repo = self.repo.clone();
        let path = self.path.clone();
        let branch = self.branch.clone();
        let structure_lock = self.structure_lock.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = cleanup_worktree(&repo, &path, &branch, &structure_lock).await {
                    tracing::warn!(
                        target: "medha_orchestrator",
                        path = %path.display(), %error,
                        "drop-time worktree cleanup did not complete; startup sweep will retry"
                    );
                }
            });
        }
    }
}

/// Hands out isolated checkouts, one per writer child.
///
/// Owns a directory outside the working tree — a worktree inside the repo would
/// show up in the parent's own `git status`, its greps and its builds, which is
/// the collision this exists to prevent.
pub struct WorktreePool {
    repo: PathBuf,
    dir: PathBuf,
    leases: Leases,
    max_patch_bytes: usize,
    #[cfg(test)]
    checkout_hook: Option<CheckoutHook>,
}

#[cfg(test)]
#[derive(Clone)]
struct CheckoutHook {
    after_add: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
}

impl WorktreePool {
    /// `dir` must be Medha-owned state (the per-workspace state directory), not
    /// a path inside the repository.
    pub fn new(repo: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            dir: dir.into(),
            leases: Arc::new(Mutex::new(HashSet::new())),
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
            #[cfg(test)]
            checkout_hook: None,
        }
    }

    /// Configure the hard extraction ceiling. Exceeding it preserves the
    /// checkout; it never returns a truncated patch.
    pub fn with_max_patch_bytes(mut self, max_patch_bytes: usize) -> Self {
        self.max_patch_bytes = max_patch_bytes;
        self
    }

    /// Resolve the repository root for `path`, so a pool created from a
    /// subdirectory still cuts worktrees from the real repo.
    pub async fn discover(
        path: impl AsRef<Path>,
        dir: impl Into<PathBuf>,
    ) -> Result<Self, WorktreeError> {
        let path = path.as_ref();
        let root = git(path, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| WorktreeError::NotARepo(path.to_path_buf()))?;
        if root.is_empty() {
            return Err(WorktreeError::NotARepo(path.to_path_buf()));
        }
        Ok(Self::new(PathBuf::from(root), dir))
    }

    /// Cut a checkout for `session` from the repository's current HEAD.
    ///
    /// The base is resolved to a concrete commit rather than left as `HEAD`: the
    /// parent keeps working while the child runs, so a symbolic base would move
    /// underneath the diff and the returned patch would describe changes nobody
    /// made.
    pub async fn checkout(&self, session: Ulid) -> Result<Worktree, WorktreeError> {
        let base = git(&self.repo, &["rev-parse", "HEAD"])
            .await
            .map_err(|_| WorktreeError::NoCommits)?;
        // Canonicalize the parent directory *before* git ever sees the path.
        // The sandbox jail compares canonical paths (on macOS `/var` is a
        // symlink to `/private/var`), but git remembers a worktree by the exact
        // string it was registered with — so handing git one spelling and the
        // sandbox another leaves a worktree that cannot be removed by name.
        std::fs::create_dir_all(&self.dir).map_err(|e| WorktreeError::Io(e.to_string()))?;
        let dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
        #[cfg(windows)]
        let dir = git_worktree_path(dir);
        let name = session.to_string().to_lowercase();
        let path = dir.join(&name);
        let branch = format!("{BRANCH_PREFIX}/{name}");
        // Structural, not conventional: reserve the path before it exists.
        // The RAII reservation releases itself if cancellation/error occurs at
        // any await before the Worktree guard takes ownership.
        let reservation = LeaseReservation::acquire(&self.leases, path.clone())?;
        let lock_path = repository_structure_lock_path(&self.repo).await?;
        with_structure_lock(&lock_path, || async {
            git(
                &self.repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &path.to_string_lossy(),
                    &base,
                ],
            )
            .await?;

            #[cfg(test)]
            if let Some(hook) = &self.checkout_hook {
                // Deterministic coverage of the exact historical race: another
                // pool starts sweeping after Git publishes the checkout but
                // before this process publishes its owner marker.
                hook.after_add.wait().await;
                hook.release.wait().await;
            }

            // The cross-process lock spans Git registration through marker
            // publication, so a sweep can never observe the old dangerous gap.
            if let Err(error) = std::fs::write(owner_marker(&path), owner_record_text()) {
                let _ = git(
                    &self.repo,
                    &["worktree", "remove", "--force", &path.to_string_lossy()],
                )
                .await;
                let _ = git(&self.repo, &["worktree", "prune"]).await;
                let _ = git(&self.repo, &["branch", "-D", &branch]).await;
                return Err(WorktreeError::Io(format!(
                    "could not record the worktree owner: {error}"
                )));
            }
            Ok(())
        })
        .await?;

        let worktree = Worktree {
            preserve: std::sync::atomic::AtomicBool::new(false),
            repo: self.repo.clone(),
            path,
            branch,
            base,
            structure_lock: lock_path,
            leases: Arc::clone(&self.leases),
            max_patch_bytes: self.max_patch_bytes,
        };
        reservation.commit();
        Ok(worktree)
    }

    /// Remove agent worktrees left by a previous process.
    ///
    /// Called at startup: a crash between `worktree add` and reaping leaves both
    /// a registration and a directory, and `git worktree add` refuses a path
    /// that already exists — so without this the *next* run of the same agent
    /// fails for a reason that has nothing to do with it.
    pub async fn sweep(&self) {
        if let Err(error) = self.sweep_locked().await {
            tracing::warn!(
                target: "medha_orchestrator",
                %error,
                "worktree sweep did not complete; keeping uncertain checkouts"
            );
        }
    }

    async fn sweep_locked(&self) -> Result<(), WorktreeError> {
        let lock_path = repository_structure_lock_path(&self.repo).await?;
        with_structure_lock(&lock_path, || async { self.sweep_under_lock().await }).await
    }

    async fn sweep_under_lock(&self) -> Result<(), WorktreeError> {
        let listed = git(&self.repo, &["worktree", "list", "--porcelain"]).await?;
        let mut current: Option<String> = None;
        let mut stale: Vec<String> = Vec::new();
        for line in listed.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current = Some(path.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                // Only ours. Someone else's worktree in the same repo is not
                // Medha's to remove, however stale it looks.
                let prefix = format!("refs/heads/{BRANCH_PREFIX}/");
                if branch.starts_with(&prefix)
                    && let Some(path) = current.take()
                {
                    stale.push(path);
                }
            }
        }
        for path in stale {
            // A live lease means this process owns it and it is not stale.
            if self
                .leases
                .lock()
                .map(|leases| leases.contains(Path::new(&path)))
                .unwrap_or(false)
            {
                continue;
            }
            // …and another live Medha's checkout is not this one's to remove.
            // The lease table only knows about this process.
            if owner_alive(Path::new(&path)) {
                continue;
            }
            // …and a checkout holding uncaptured work is never abandoned, however
            // dead its owner. The marker outlives the process that wrote it,
            // which is the whole point: the rescue has to survive the restart.
            if keeps_work(Path::new(&path)) {
                tracing::info!(
                    target: "medha_orchestrator",
                    %path,
                    "keeping a checkout whose work was never captured as a patch"
                );
                continue;
            }
            // Missing markers are not permission to destroy uncommitted work:
            // the owner/keep write may have failed or this may be a checkout
            // from an older Medha. Fail closed on both dirty output and an
            // inability to inspect it.
            match git_bounded(Path::new(&path), &["status", "--porcelain"], 0).await {
                Ok(_) => {}
                Err(WorktreeError::PatchTooLarge { .. }) => {
                    tracing::warn!(
                        target: "medha_orchestrator",
                        %path,
                        "keeping an unmarked checkout because it has uncommitted work"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "medha_orchestrator",
                        %path, %error,
                        "keeping an unmarked checkout because its state could not be inspected"
                    );
                    continue;
                }
            }
            let _ = git(&self.repo, &["worktree", "remove", "--force", &path]).await;
            let _ = std::fs::remove_file(owner_marker(Path::new(&path)));
        }
        let _ = git(&self.repo, &["worktree", "prune"]).await;
        // Branches outlive their worktree's removal, so prune them too or the
        // namespace grows without bound across sessions.
        let branches = git(
            &self.repo,
            &["branch", "--list", &format!("{BRANCH_PREFIX}/*")],
        )
        .await
        .unwrap_or_default();
        for branch in branches.lines() {
            let branch = branch.trim_start_matches('*').trim();
            if !branch.is_empty() {
                let _ = git(&self.repo, &["branch", "-D", branch]).await;
            }
        }
        Ok(())
    }
}

/// How a patch stands against the tree it would be applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeCheck {
    /// Applies cleanly.
    Clean,
    /// Applies with three-way resolution — the base moved but the edits do not
    /// overlap.
    ThreeWay,
    /// Overlapping edits. §6.4: this goes to reconciliation, never to
    /// last-writer-wins.
    Conflict,
    /// Nothing to apply.
    Empty,
}

/// Test a patch against the current tree *without* touching it.
///
/// Three-way, because by the time a child finishes its base is usually behind —
/// the parent kept working — and a strict apply would reject patches that merge
/// perfectly well.
///
/// `git apply --3way --check` looks like the dry run for this and is not: under
/// `--3way` git performs the merge and writes conflict markers into the working
/// tree, then exits **0**. Using it would corrupt the user's files while
/// reporting success. So the three-way attempt runs `--cached` against a
/// *scratch copy of the index*: git resolves the merge against real blobs, the
/// exit status tells the truth about conflicts, and neither the working tree nor
/// the real index is touched.
pub async fn check(repo: &Path, patch: &Patch) -> MergeCheck {
    if patch.is_empty() {
        return MergeCheck::Empty;
    }
    // Plain `--check` has no `--3way`, so it really is a dry run.
    if apply(repo, patch, &["--check"], None).await.is_ok() {
        return MergeCheck::Clean;
    }
    let Ok(scratch) = ScratchIndex::new(repo).await else {
        // Without a scratch index there is no non-destructive way to ask, and
        // guessing "mergeable" would be guessing in the direction that damages
        // the tree.
        return MergeCheck::Conflict;
    };
    match apply(repo, patch, &["--3way", "--cached"], Some(scratch.path())).await {
        Ok(()) => MergeCheck::ThreeWay,
        Err(_) => MergeCheck::Conflict,
    }
}

/// Apply a patch to `repo` for real. Only ever reached after the human gate:
/// merging a child's work is a consequential action on the user's tree.
///
/// Returns the failure verbatim, because a merge that half-applied is exactly
/// the situation where a paraphrased error is worthless.
pub async fn merge(repo: &Path, patch: &Patch) -> Result<MergeCheck, WorktreeError> {
    let check = check(repo, patch).await;
    match check {
        MergeCheck::Empty => Ok(check),
        MergeCheck::Conflict => Err(WorktreeError::Git {
            command: "apply --3way".into(),
            message: "the patch conflicts with the current tree".into(),
        }),
        // A clean patch is applied strictly. Reaching for `--3way` here would
        // buy nothing and cost the guarantee that a merge which passed a strict
        // check cannot leave conflict markers behind.
        MergeCheck::Clean => apply(repo, patch, &[], None).await.map(|()| check),
        MergeCheck::ThreeWay => apply(repo, patch, &["--3way"], None).await.map(|()| check),
    }
}

/// A throwaway copy of the repository index, so a three-way apply can be
/// *asked about* without being performed. Removed on drop.
struct ScratchIndex {
    path: PathBuf,
}

impl ScratchIndex {
    async fn new(repo: &Path) -> Result<Self, WorktreeError> {
        let git_dir = git(repo, &["rev-parse", "--absolute-git-dir"]).await?;
        let index = Path::new(&git_dir).join("index");
        // Alongside the real index rather than in the temp dir: `git apply`
        // resolves relative worktree paths from the repository, and a scratch
        // index on another filesystem is a rename away from failing.
        let path = Path::new(&git_dir).join(format!("medha-scratch-{}.index", Ulid::new()));
        // A repository with nothing staged has no index file yet; an absent
        // scratch index is a valid empty one, so that is not an error.
        if index.exists() {
            std::fs::copy(&index, &path).map_err(|e| WorktreeError::Io(e.to_string()))?;
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Feed a patch to `git apply` on stdin. Writing it to a temp file first would
/// be one more thing to clean up, and one more path for the diff to leak to.
///
/// `index` redirects git to a scratch index; `None` uses the repository's own.
async fn apply(
    repo: &Path,
    patch: &Patch,
    flags: &[&str],
    index: Option<&Path>,
) -> Result<(), WorktreeError> {
    let mut args = Vec::with_capacity(flags.len() + 1);
    args.push("apply");
    args.extend_from_slice(flags);
    // A diff that does not end in a newline is rejected as corrupt.
    let mut body = patch.diff.clone();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let env = index.map(|path| {
        (
            OsString::from("GIT_INDEX_FILE"),
            path.as_os_str().to_os_string(),
        )
    });
    let output = run_git(repo, &args, Some(body.into_bytes()), env, 0).await?;
    if output.stdout.overflowed {
        return Err(WorktreeError::GitOutputTooLarge {
            command: args.join(" "),
            limit: 0,
        });
    }
    if output.status.success() {
        return Ok(());
    }
    Err(git_failure(&args, &output))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git for Windows may check out text files with CRLF according to the
    /// machine's `core.autocrlf` setting. These tests assert logical file
    /// content, not the platform's newline convention.
    fn read_text(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(path)
            .unwrap()
            .replace("\r\n", "\n")
    }

    /// A real repository with one commit — worktrees are a git feature, so
    /// faking git here would test nothing that matters.
    async fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "--initial-branch=main"])
            .await
            .unwrap();
        git(&root, &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        git(&root, &["config", "user.name", "t"]).await.unwrap();
        // These fixtures assert patch contents, not Git's user-selected text
        // checkout policy. Pin the repository policy so Windows' usual
        // core.autocrlf=true does not turn LF fixtures into CRLF assertions.
        git(&root, &["config", "core.autocrlf", "false"])
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "base"]).await.unwrap();
        (dir, root)
    }

    fn pool(root: &Path) -> (tempfile::TempDir, WorktreePool) {
        let state = tempfile::tempdir().unwrap();
        let pool = WorktreePool::new(root, state.path().join("worktrees"));
        (state, pool)
    }

    #[cfg(windows)]
    #[test]
    fn git_worktree_paths_drop_rusts_verbatim_prefix() {
        assert_eq!(
            git_worktree_path(PathBuf::from(r"\\?\C:\Users\runner\worktrees")),
            PathBuf::from(r"C:\Users\runner\worktrees")
        );
        assert_eq!(
            git_worktree_path(PathBuf::from(r"\\?\UNC\server\share\worktrees")),
            PathBuf::from(r"\\server\share\worktrees")
        );
    }

    #[tokio::test]
    async fn a_writer_works_in_its_own_checkout_not_the_parents() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();

        assert_ne!(tree.path(), root);
        assert!(!tree.path().starts_with(&root));
        std::fs::write(tree.path().join("a.txt"), "one\nCHANGED\nthree\n").unwrap();
        // The parent's tree is untouched — that is the whole point.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    /// The `Drop` guard force-removes any checkout it still holds a lease for,
    /// so merely declining to call `reap` was not enough to keep work whose
    /// patch could not be captured — the directory went anyway, on a path with
    /// no other copy of it.
    #[tokio::test]
    async fn a_preserved_checkout_survives_being_dropped() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let path = {
            let tree = pool.checkout(Ulid::new()).await.unwrap();
            std::fs::write(tree.path().join("a.txt"), "work nobody captured\n").unwrap();
            tree.preserve();
            tree.path().to_path_buf()
        };
        // The guard has run by now; give the detached remover a chance to as
        // well, so this fails loudly if preservation is not honoured.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(path.exists(), "preserved checkout was removed anyway");
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "work nobody captured\n"
        );
        // And the sweep must not take it either: the lease is deliberately held.
        pool.sweep().await;
        assert!(path.exists(), "sweep removed a preserved checkout");
    }

    /// The rescue has to outlive the process that made it. A restart holds no
    /// lease and reads a dead owner pid, so without an on-disk mark the next
    /// launch sweeps away exactly the work the rescue existed to save.
    #[tokio::test]
    async fn a_preserved_checkout_survives_a_restart() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let path = {
            let tree = pool.checkout(Ulid::new()).await.unwrap();
            std::fs::write(tree.path().join("a.txt"), "unrecoverable elsewhere\n").unwrap();
            tree.preserve();
            tree.path().to_path_buf()
        };
        // A second pool over the same directory is what the next launch has:
        // no lease table, and an owner marker naming a process that is gone.
        std::fs::write(owner_marker(&path), dead_pid().to_string()).unwrap();
        let restarted = WorktreePool::new(&root, state.path().join("worktrees"));
        restarted.sweep().await;
        assert!(path.exists(), "restart swept away preserved work");
        assert_eq!(
            std::fs::read_to_string(path.join("a.txt")).unwrap(),
            "unrecoverable elsewhere\n"
        );
    }

    #[tokio::test]
    async fn two_writers_can_never_share_a_checkout() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let session = Ulid::new();
        let _first = pool.checkout(session).await.unwrap();
        // Same session id is the only way to collide, and it is refused rather
        // than handing a second writer the same directory.
        assert!(matches!(
            pool.checkout(session).await,
            Err(WorktreeError::AlreadyLeased(_))
        ));
        // Distinct children always get distinct paths.
        let other = pool.checkout(Ulid::new()).await.unwrap();
        assert_ne!(other.path(), _first.path());
    }

    #[tokio::test]
    async fn a_patch_carries_edits_new_files_and_its_base() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nEDITED\nthree\n").unwrap();
        std::fs::write(tree.path().join("new.rs"), "fn added() {}\n").unwrap();

        let patch = tree.patch().await.unwrap();
        assert!(!patch.is_empty());
        assert!(patch.diff.contains("EDITED"));
        // Without intent-to-add a created file is invisible to `git diff`, and a
        // child that only added a module would report having done nothing.
        assert!(patch.diff.contains("fn added"));
        assert!(patch.files.contains(&"a.txt".to_string()));
        assert!(patch.files.contains(&"new.rs".to_string()));
        assert_eq!(patch.base.len(), 40);
    }

    #[tokio::test]
    async fn an_idle_writer_returns_an_empty_patch_not_an_error() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        let patch = tree.patch().await.unwrap();
        assert!(patch.is_empty());
        assert_eq!(check(&root, &patch).await, MergeCheck::Empty);
    }

    #[tokio::test]
    async fn a_patch_merges_back_into_the_parent() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nEDITED\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        assert_eq!(check(&root, &patch).await, MergeCheck::Clean);
        merge(&root, &patch).await.unwrap();
        assert_eq!(
            read_text(root.join("a.txt")),
            "one\nEDITED\nthree\n"
        );
    }

    #[tokio::test]
    async fn a_moved_base_still_merges_when_the_edits_do_not_overlap() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\ntwo\nthree\nfrom-child\n").unwrap();
        let patch = tree.patch().await.unwrap();

        // The parent kept working while the child ran — the usual case, not an
        // edge one. A strict apply would reject this.
        std::fs::write(root.join("b.txt"), "parent moved on\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent"]).await.unwrap();

        assert!(matches!(
            check(&root, &patch).await,
            MergeCheck::Clean | MergeCheck::ThreeWay
        ));
        merge(&root, &patch).await.unwrap();
        assert!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .contains("from-child")
        );
    }

    #[tokio::test]
    async fn overlapping_edits_are_reported_as_conflicts_not_applied() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        // The parent rewrote the same line the child did.
        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent edit"]).await.unwrap();

        assert_eq!(check(&root, &patch).await, MergeCheck::Conflict);
        // Never last-writer-wins: the merge is refused and the tree is left as
        // the parent had it.
        assert!(merge(&root, &patch).await.is_err());
        assert!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .contains("PARENT")
        );
    }

    #[tokio::test]
    async fn asking_whether_a_patch_merges_never_changes_the_tree() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();

        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent edit"]).await.unwrap();
        let before = std::fs::read_to_string(root.join("a.txt")).unwrap();

        // `git apply --3way --check` is not a dry run: it writes conflict
        // markers into the working tree and *exits zero*. Checking twice would
        // have left the file mangled and reported the patch as mergeable.
        for _ in 0..2 {
            assert_eq!(check(&root, &patch).await, MergeCheck::Conflict);
        }
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), before);
        assert!(!before.contains("<<<<<<<"));
        // The index is untouched too — the check runs against a scratch copy.
        assert!(
            git(&root, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_scratch_index_does_not_outlive_the_check() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let tree = pool.checkout(Ulid::new()).await.unwrap();
        std::fs::write(tree.path().join("a.txt"), "one\nCHILD\nthree\n").unwrap();
        let patch = tree.patch().await.unwrap();
        std::fs::write(root.join("a.txt"), "one\nPARENT\nthree\nplus\n").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "parent"]).await.unwrap();

        check(&root, &patch).await;
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("medha-scratch-")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn reaping_removes_the_checkout_and_frees_the_path() {
        let (_repo, root) = repo().await;
        let (_state, pool) = pool(&root);
        let session = Ulid::new();
        let tree = pool.checkout(session).await.unwrap();
        let path = tree.path().to_path_buf();
        tree.reap().await;

        assert!(!path.exists());
        // The lease is released with the directory, so the same id can be
        // leased again — an orphaned lease would wedge it forever.
        assert!(pool.checkout(session).await.is_ok());
    }

    #[tokio::test]
    async fn a_sweep_clears_worktrees_a_crashed_process_left_behind() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let leaked = pool.checkout(Ulid::new()).await.unwrap();
        let path = leaked.path().to_path_buf();
        // Forget the guard: exactly what a crash does — the registration and the
        // directory survive with nothing left to clean them up. The owner marker
        // has to name a dead process too, since a real crash is followed by a
        // *new* process with a different pid; leaving this one's pid there would
        // be simulating a crash that somehow kept running.
        std::mem::forget(leaked);
        std::fs::write(owner_marker(&path), dead_pid().to_string()).unwrap();
        assert!(path.exists());

        let fresh = WorktreePool::new(&root, state.path().join("worktrees"));
        fresh.sweep().await;
        assert!(!path.exists());
        // And the next run of the same agent is not blocked by the leftovers.
        assert!(
            git(&root, &["branch", "--list", &format!("{BRANCH_PREFIX}/*")])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A pid that has certainly exited: spawn something trivial and reap it.
    /// Inventing a large number would be a guess that some CI box falsifies.
    fn dead_pid() -> u32 {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("spawn a short-lived process");
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived process");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    #[cfg(windows)]
    #[test]
    fn windows_owner_liveness_rejects_dead_and_pid_reused_owners() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("agent-worktree");
        let current_creation =
            windows_process_creation_time(unsafe { GetCurrentProcess() }).unwrap();
        std::fs::write(
            owner_marker(&worktree),
            format!("{} {current_creation}", std::process::id()),
        )
        .unwrap();
        assert!(owner_alive(&worktree), "the current owner must be live");

        // A live process with a recycled PID is not the recorded owner. This
        // deterministic identity mismatch exercises PID reuse without waiting
        // for Windows to recycle a particular numeric PID.
        std::fs::write(
            owner_marker(&worktree),
            format!(
                "{} {}",
                std::process::id(),
                current_creation.wrapping_add(1)
            ),
        )
        .unwrap();
        assert!(
            !owner_alive(&worktree),
            "creation identity must reject a reused PID"
        );

        // Keep the process itself alive rather than asking `cmd.exe` to spawn
        // a helper such as ping.exe. Killing only cmd would otherwise leave the
        // helper behind until its own timer expired on the Windows CI runner.
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a live Windows owner");
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child.id()) };
        assert!(!process.is_null(), "open the child process");
        let child_creation = windows_process_creation_time(process).unwrap();
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(process);
        }
        std::fs::write(
            owner_marker(&worktree),
            format!("{} {child_creation}", child.id()),
        )
        .unwrap();
        assert!(owner_alive(&worktree), "a live foreign owner must be kept");

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            !owner_alive(&worktree),
            "a killed Windows owner must be reaped"
        );
    }

    /// Two Medha processes in one repository is ordinary — two terminals. The
    /// lease table is per-process, so without an on-disk owner the second one
    /// reads the first's live checkout as abandoned and force-removes it, taking
    /// the child's uncommitted work with it.
    #[tokio::test]
    async fn a_sweep_leaves_another_live_medhas_worktree_alone() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let theirs = pool.checkout(Ulid::new()).await.unwrap();
        let path = theirs.path().to_path_buf();
        std::fs::write(path.join("work-in-progress.txt"), "half an edit").unwrap();

        // A different process: no lease for this path, and it must still refuse
        // to touch it because the owner is alive.
        let other = WorktreePool::new(&root, state.path().join("worktrees"));
        other.sweep().await;

        assert!(
            path.exists(),
            "a live checkout was removed by another process"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("work-in-progress.txt")).unwrap(),
            "half an edit"
        );
    }

    /// A repository-wide OS lock must cover the exact interval between Git
    /// publishing a worktree and Medha publishing its owner marker. This uses a
    /// second test-harness process, not merely another pool in this process, so
    /// it proves the cross-process contract that failed in AUD-028.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cross_process_sweep_cannot_enter_the_add_marker_gap() {
        const ROOT_ENV: &str = "MEDHA_TEST_SWEEP_ROOT";
        const STATE_ENV: &str = "MEDHA_TEST_SWEEP_STATE";
        const STARTED_ENV: &str = "MEDHA_TEST_SWEEP_STARTED";
        const DONE_ENV: &str = "MEDHA_TEST_SWEEP_DONE";

        if let (Ok(root), Ok(state), Ok(started), Ok(done)) = (
            std::env::var(ROOT_ENV),
            std::env::var(STATE_ENV),
            std::env::var(STARTED_ENV),
            std::env::var(DONE_ENV),
        ) {
            std::fs::write(started, b"started").unwrap();
            WorktreePool::new(root, state).sweep().await;
            std::fs::write(done, b"done").unwrap();
            return;
        }

        let (_repo, root) = repo().await;
        let state = tempfile::tempdir().unwrap();
        let worktree_dir = state.path().join("worktrees");
        let other_state_dir = state.path().join("other-medha-home").join("worktrees");
        let after_add = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let mut pool = WorktreePool::new(&root, &worktree_dir);
        pool.checkout_hook = Some(CheckoutHook {
            after_add: Arc::clone(&after_add),
            release: Arc::clone(&release),
        });
        let pool = Arc::new(pool);
        let checkout = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move { pool.checkout(Ulid::new()).await })
        };

        // Checkout now holds the cross-process lock immediately after
        // `git worktree add`, before writing the owner marker.
        if tokio::time::timeout(Duration::from_secs(10), after_add.wait())
            .await
            .is_err()
        {
            let checkout_result = if checkout.is_finished() {
                match checkout.await {
                    Ok(Ok(_)) => " result: checkout unexpectedly completed".into(),
                    Ok(Err(error)) => format!(" result: {error}"),
                    Err(error) => format!(" result: checkout task failed: {error}"),
                }
            } else {
                " checkout task did not settle".into()
            };
            panic!(
                "checkout did not reach the add/marker barrier within 10s;{}",
                checkout_result
            );
        }
        let started = state.path().join("sweep-started");
        let done = state.path().join("sweep-done");
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "worktree::tests::a_cross_process_sweep_cannot_enter_the_add_marker_gap",
                "--exact",
                "--nocapture",
            ])
            .env(ROOT_ENV, &root)
            // A distinct state root proves the lock is repository-wide rather
            // than accidentally coordinating only one MEDHA_HOME.
            .env(STATE_ENV, &other_state_dir)
            .env(STARTED_ENV, &started)
            .env(DONE_ENV, &done)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sweep process did not start");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !done.exists(),
            "second process swept while checkout lacked its owner marker"
        );

        // Marker publication completes while the same lock is still held.
        release.wait().await;
        let tree = tokio::time::timeout(Duration::from_secs(10), checkout)
            .await
            .expect("checkout did not finish")
            .expect("checkout task failed")
            .expect("checkout failed");
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("sweep process did not finish")
            .expect("could not wait for sweep process");
        assert!(status.success());
        assert!(done.exists());
        assert!(
            tree.path().exists(),
            "sweep removed a checkout after its live owner was published"
        );
        tree.reap().await;
    }

    #[cfg(unix)]
    fn executable_script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("fake-git");
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_git_deadline_reaps_its_descendant_process_tree() {
        let temp = tempfile::tempdir().unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let survived = temp.path().join("survived");
        let script = executable_script(
            temp.path(),
            "#!/bin/sh\n\
             printf started > \"$1\"\n\
             (while [ ! -f \"$2\" ]; do sleep 0.05; done; printf survived > \"$3\") &\n\
             sleep 30\n",
        );
        let result = run_git_program(
            script,
            temp.path().to_path_buf(),
            vec![
                started.to_string_lossy().to_string(),
                release.to_string_lossy().to_string(),
                survived.to_string_lossy().to_string(),
            ],
            None,
            None,
            1024,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(WorktreeError::GitTimeout { .. })));
        assert!(started.exists());
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !survived.exists(),
            "a descendant survived the Git command deadline"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_git_future_still_reaps_its_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let survived = temp.path().join("survived");
        let script = executable_script(
            temp.path(),
            "#!/bin/sh\n\
             printf started > \"$1\"\n\
             (while [ ! -f \"$2\" ]; do sleep 0.05; done; printf survived > \"$3\") &\n\
             sleep 30\n",
        );
        let completion = Arc::new(tokio::sync::Semaphore::new(0));
        let run = tokio::spawn(run_git_program_observed(
            script,
            temp.path().to_path_buf(),
            vec![
                started.to_string_lossy().to_string(),
                release.to_string_lossy().to_string(),
                survived.to_string_lossy().to_string(),
            ],
            None,
            None,
            1024,
            Duration::from_secs(30),
            Some(Arc::clone(&completion)),
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake Git process did not start");
        run.abort();
        let _ = run.await;
        // The production supervisor itself permits a full settle grace. Give
        // this observation a scheduling margin instead of racing the exact
        // same five-second boundary under a busy all-targets test run.
        let observation_deadline = GIT_SETTLE_TIMEOUT + Duration::from_secs(5);
        let _permit = tokio::time::timeout(observation_deadline, completion.acquire())
            .await
            .expect("owned Git supervisor did not settle after caller cancellation")
            .expect("completion semaphore unexpectedly closed");
        std::fs::write(&release, b"go").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !survived.exists(),
            "a descendant survived cancellation of the Git future"
        );
    }

    #[tokio::test]
    async fn a_sweep_leaves_worktrees_that_are_not_medhas_alone() {
        let (_repo, root) = repo().await;
        let (state, pool) = pool(&root);
        let theirs = state.path().join("someone-elses");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feature/theirs",
                &theirs.to_string_lossy(),
                "HEAD",
            ],
        )
        .await
        .unwrap();

        pool.sweep().await;
        assert!(theirs.exists());
    }

    #[tokio::test]
    async fn a_non_repository_is_refused_rather_than_silently_unisolated() {
        let dir = tempfile::tempdir().unwrap();
        // Degrading to "write in the parent's tree" here would be the one
        // outcome writer isolation exists to prevent.
        assert!(matches!(
            WorktreePool::discover(dir.path(), dir.path().join("wt")).await,
            Err(WorktreeError::NotARepo(_))
        ));
    }

    #[tokio::test]
    async fn a_repository_with_no_commits_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init"]).await.unwrap();
        let (_state, pool) = pool(&root);
        assert!(matches!(
            pool.checkout(Ulid::new()).await,
            Err(WorktreeError::NoCommits)
        ));
    }

    #[test]
    fn evidence_of_success_and_absence_of_evidence_are_different_answers() {
        let mut patch = Patch {
            diff: "diff".into(),
            ..Default::default()
        };
        assert!(!patch.verified());
        patch.verification = Some(Verification {
            command: "cargo test".into(),
            passed: false,
            output: "1 failed".into(),
        });
        assert!(!patch.verified());
        patch.verification = Some(Verification {
            command: "cargo test".into(),
            passed: true,
            output: "ok".into(),
        });
        assert!(patch.verified());
    }
}
