//! Runs child agents as durable kernel sessions with narrowed executors and
//! task-local cancellation.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kernel::{
    Event, EventKind, EventLog, Kernel, Message, Provider, Role, Session, StopReason, TrustLabel,
};
use orchestrator::{AgentStatus, ChildOutcome, ChildRun, ChildRunner};

/// Publishes a child's liveness as it runs.
///
/// Children used to stream into [`kernel::NullSink`], which discarded every
/// token, tool call and phase change the moment it was produced. That is why a
/// running child could only ever be described by scraping timestamps back out of
/// the event log — and why a wedged one looked exactly like a busy one.
struct AgentSink {
    progress: kernel::ProgressHandle,
}

impl kernel::StreamSink for AgentSink {
    fn phase(&self, phase: kernel::Phase) {
        self.progress.enter(phase);
    }

    fn phase_ticked(&self) {
        self.progress.ticked();
    }

    fn tool_call(&self, _tool: &str, _args: &serde_json::Value) {
        self.progress.tool_dispatched();
    }

    fn usage(&self, _prompt_tokens: u32, total_tokens: u32) {
        self.progress.metered(u64::from(total_tokens));
    }

    fn cost(&self, total_usd: f64, _indicative: bool) {
        // `cost` reports the running total, not a delta, so it is recorded as an
        // absolute rather than added to what is already there.
        self.progress.priced(total_usd);
    }
}

fn child_prompt(run: &ChildRun) -> String {
    let mut prompt = format!(
        "You are a focused sub-agent. You have been delegated one task and you \
         cannot see the conversation that produced it, so work only from what is \
         written here.\n\nTask:\n{}\n",
        run.spec.objective
    );
    if let Some(contract) = &run.spec.contract {
        prompt.push_str(&format!("\nYour answer must be: {contract}\n"));
    }
    match &run.workspace {
        Some(_) => prompt.push_str(
            "\nYou can modify code. You are working in a private checkout of the \
             repository that nobody else can see: your edits affect nothing \
             outside it, so make them directly rather than describing them. Do \
             not commit, and do not use git to branch, stash or reset — your \
             changes are collected as a diff exactly as you leave them in the \
             working tree.\n\n\
             Build or test what you changed before you finish, and say what you \
             ran. Your patch is reviewed by a human alongside that evidence, so \
             an honest \"this compiles but I could not run the tests\" is worth \
             far more than a confident claim that does not hold.\n",
        ),
        None => prompt.push_str(
            "\nYou are read-only: you cannot modify anything. Investigate, then \
             finish with your findings as your final message.\n\
             \n\
             If the task above asks you to CHANGE anything — write, edit, add, \
             fix, rename, create a file — you cannot do it, and you must say so \
             as the first line of your answer: state plainly that you were sent \
             read-only and the task needs write access. Do not describe the \
             changes you would have made and leave it at that: a plan written \
             out in full reads like completed work, and whoever sent you will \
             believe the job is done and find the files untouched. Reporting \
             the refusal is the useful outcome; describing the work instead of \
             doing it is the harmful one.\n",
        ),
    }
    let holds = |name: &str| run.executor.specs().iter().any(|spec| spec.name == name);
    prompt.push_str(match holds("agent.spawn") {
        true => {
            "\nYou may delegate a bounded piece of this to your own sub-agent, which \
             reports back to you. Do that only for work that is genuinely separable — \
             you are already a delegate, and a chain of agents each passing the task \
             on costs more at every link.\n"
        }
        false => "\nYou cannot delegate: do this work yourself.\n",
    });
    if holds("agent.message") {
        prompt.push_str(
            "\nYou can send a message to the agent that sent you here, or to another \
             running agent, when you find something it needs and cannot see. That is \
             for passing on findings, not for asking questions — nobody is waiting to \
             answer you.\n",
        );
    }
    prompt.push_str(
        "\nYou cannot ask a question and wait for an answer. Where the task is \
         ambiguous, choose the most reasonable reading, say which you chose, and \
         continue.\n\
         \n\
         Actions that touch anything outside your own reasoning may stop for the \
         user's approval, and the request will name you. That is normal — wait for \
         it rather than looking for a way around it.\n\
         \n\
         A further message may arrive from whoever sent you here — a correction, \
         a constraint, a narrowing of scope. Treat it as authoritative and \
         current: it supersedes the task above where the two conflict, and it \
         was sent because something you were doing was wrong or unnecessary. \
         Adjust and carry on with what you have already found; do not start \
         over.\n",
    );
    prompt.push_str(&format!(
        "\nYou have {} turns. That is a hard stop — when it runs out you are cut \
         off wherever you are, and whatever you had said last is what gets \
         returned. So do not save the answer for the end: once you are around \
         two thirds through, stop exploring and write what you have, marking \
         anything you could not confirm. A partial answer delivered is worth \
         more than a complete one you never reached.\n\n\
         Your final message is the only thing returned, and it lands in the \
         parent's context window — so keep it tight. Lead with the answer, use \
         bullets over paragraphs, cite concrete file paths and line numbers, and \
         do not replay how you got there. If you could not answer, say so plainly \
         and say what you ruled out.",
        run.budget.max_turns.unwrap_or(1)
    ));
    prompt
}

/// Rebuilds sandbox-bound services at a child root without widening permissions.
#[derive(Clone)]
pub struct SandboxTemplate {
    pub trust: PathBuf,
    pub audit: PathBuf,
    pub gate: Arc<dyn kernel::HumanGate>,
    pub exec: Arc<dyn sandbox::ExecBackend>,
    pub snapshots: PathBuf,
    pub readable: Vec<PathBuf>,
    /// Session-wide approval roots shared with the parent sandbox.
    pub approved: sandbox::ApprovedRoots,
    /// Session-wide network grant shared with the parent sandbox, so a grant in
    /// one writer worktree is honoured everywhere.
    pub net_grant: sandbox::NetworkGrant,
}

/// Deferred weak handle that avoids the agent-control ownership cycle.
pub type RegistryHandle = Arc<Mutex<Option<std::sync::Weak<tools::ToolRegistry>>>>;

const VERIFY_MAX_OUTPUT: usize = 8_192;

pub struct WorktreeWorkspaces {
    pool: orchestrator::WorktreePool,
    repo: PathBuf,
    /// Bounds verification commands that may run edited build scripts.
    verify_timeout: std::time::Duration,
    registry: RegistryHandle,
    template: SandboxTemplate,
    verify: Option<String>,
}

impl WorktreeWorkspaces {
    /// Returns `None` outside Git, causing writers to be refused rather than
    /// run without isolation.
    pub async fn discover(
        repo: &Path,
        dir: PathBuf,
        registry: RegistryHandle,
        template: SandboxTemplate,
        verify: Option<String>,
        verify_timeout: std::time::Duration,
        max_patch_bytes: usize,
    ) -> Option<Self> {
        let pool = orchestrator::WorktreePool::discover(repo, dir)
            .await
            .ok()?
            .with_max_patch_bytes(max_patch_bytes);
        // Clear abandoned paths that would block the next `git worktree add`.
        pool.sweep().await;
        Some(Self {
            pool,
            repo: repo.to_path_buf(),
            verify_timeout,
            registry,
            template,
            verify,
        })
    }

    pub fn registry_handle() -> RegistryHandle {
        Arc::new(Mutex::new(None))
    }
}

#[async_trait::async_trait]
impl orchestrator::Workspaces for WorktreeWorkspaces {
    async fn checkout(&self, session: ulid::Ulid) -> Result<orchestrator::Workspace, String> {
        let registry = self
            .registry
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref()?.upgrade())
            .ok_or("this session is shutting down")?;
        let worktree = self
            .pool
            .checkout(session)
            .await
            .map_err(|error| error.to_string())?;
        let sandbox = sandbox::WorkspaceSandbox::new_with_roots(
            worktree.path(),
            self.template.trust.clone(),
            self.template.audit.clone(),
            Some(Arc::clone(&self.template.gate)),
            self.template.approved.clone(),
        )
        .map_err(|error| error.to_string())?
        .with_exec_backend(Arc::clone(&self.template.exec))
        .with_network_grant(self.template.net_grant.clone())
        .map_err(|error| error.to_string())?
        .with_readable_roots(&self.template.readable)
        .with_snapshots_dir(self.template.snapshots.clone());

        let executor = registry
            .rebase(Arc::new(sandbox))
            .ok_or("this session's tools are not rooted in a workspace")?;
        Ok(orchestrator::Workspace {
            worktree,
            executor: Arc::new(executor),
        })
    }

    async fn verify(
        &self,
        root: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<orchestrator::Verification> {
        let command = self.verify.clone()?;
        // Verification is bounded and group-reaped because edited build scripts
        // may hang or leave compiler jobs holding locks. Retain the output tail,
        // where build failures are normally reported.
        match sandbox::run_shell_bounded_with(
            self.template.exec.as_ref(),
            &command,
            root,
            self.verify_timeout,
            VERIFY_MAX_OUTPUT,
            Some(cancel),
        )
        .await
        {
            Ok(outcome) => Some(orchestrator::Verification {
                command,
                passed: outcome.passed(),
                output: outcome.output,
            }),
            // A configured verifier that could not run is a *failure*, not an
            // absence. Reporting `None` here would read as "this project has no
            // verifier" and wave the patch straight through the merge gate.
            Err(error) => Some(orchestrator::Verification {
                command,
                passed: false,
                output: format!("could not run the verify command: {error}"),
            }),
        }
    }

    fn repo(&self) -> PathBuf {
        self.repo.clone()
    }
}

enum LockAttempt {
    Acquired,
    Busy,
}

/// One OS-backed process/recovery lease. The lock, not the file's existence,
/// carries liveness; stale files from a killed process are safe to acquire.
struct LeaseFile {
    path: PathBuf,
    file: Option<File>,
}

impl LeaseFile {
    fn open(path: &Path, create_new: bool) -> std::io::Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if create_new {
            options.create_new(true);
        } else {
            options.create(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        match try_lock_file(&file)? {
            LockAttempt::Acquired => Ok(Some(Self {
                path: path.to_path_buf(),
                file: Some(file),
            })),
            LockAttempt::Busy => Ok(None),
        }
    }

    fn open_existing(path: &Path) -> std::io::Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        match try_lock_file(&file)? {
            LockAttempt::Acquired => Ok(Some(Self {
                path: path.to_path_buf(),
                file: Some(file),
            })),
            LockAttempt::Busy => Ok(None),
        }
    }
}

impl Drop for LeaseFile {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            unlock_file(&file);
            drop(file);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn try_lock_file(file: &File) -> std::io::Result<LockAttempt> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(LockAttempt::Acquired);
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    ) {
        Ok(LockAttempt::Busy)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
fn try_lock_file(file: &File) -> std::io::Result<LockAttempt> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        return Ok(LockAttempt::Acquired);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(LockAttempt::Busy)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let _ = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as _,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
}

struct ProcessLease {
    instance: ulid::Ulid,
    _lock: LeaseFile,
}

impl ProcessLease {
    fn acquire(directory: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        loop {
            let instance = ulid::Ulid::new();
            let path = directory.join(format!("{instance}.live"));
            match LeaseFile::open(&path, true) {
                Ok(Some(lock)) => {
                    return Ok(Self {
                        instance,
                        _lock: lock,
                    });
                }
                Ok(None) => continue,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

/// Durable child delivery projected from the append-only session log.
pub struct LogOutbox<L: EventLog> {
    log: Arc<L>,
    lease_directory: PathBuf,
    /// Recovery requires proving this process lease is no longer held.
    lease: ProcessLease,
}

impl<L: EventLog> LogOutbox<L> {
    pub fn new(log: Arc<L>, lease_directory: impl Into<PathBuf>) -> std::io::Result<Self> {
        let lease_directory = lease_directory.into();
        let lease = ProcessLease::acquire(&lease_directory)?;
        Ok(Self {
            log,
            lease_directory,
            lease,
        })
    }

    fn owner_is_live(&self, instance: &str) -> std::io::Result<bool> {
        let Ok(instance) = instance.parse::<ulid::Ulid>() else {
            return Ok(false);
        };
        let path = self.lease_directory.join(format!("{instance}.live"));
        match LeaseFile::open_existing(&path) {
            Ok(Some(_dead_owner_claim)) => Ok(false),
            Ok(None) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn claim_recovery(&self, dispatch: &str) -> std::io::Result<Option<LeaseFile>> {
        let dispatch = dispatch
            .parse::<ulid::Ulid>()
            .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "invalid dispatch id"))?;
        LeaseFile::open(
            &self.lease_directory.join(format!("{dispatch}.recovery")),
            false,
        )
    }
}

fn field(event: &Event, key: &str) -> Option<String> {
    event
        .payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The handout an outbox row belongs to. Rows without dispatch ids use the
/// child session for compatibility.
fn handout(event: &Event) -> String {
    field(event, "dispatch")
        .or_else(|| field(event, "child"))
        .unwrap_or_default()
}

fn chain(id: ulid::Ulid) -> Session {
    Session {
        id,
        done: false,
        autonomy: kernel::AutonomyLevel::Careful,
    }
}

#[async_trait::async_trait]
impl<L: EventLog + 'static> orchestrator::Transcripts for LogOutbox<L> {
    async fn history(&self, session: ulid::Ulid) -> Vec<Message> {
        kernel::project_messages(&self.log.events(session).await)
    }
}

#[async_trait::async_trait]
impl<L: EventLog + 'static> orchestrator::Outbox for LogOutbox<L> {
    async fn dispatched(&self, dispatch: &orchestrator::Dispatch) -> bool {
        // On the parent's chain, and written before the child starts: a crash
        // in between leaves a dispatch with no terminal event, which reads as an
        // orphan rather than as nothing having happened.
        self.log
            .append(Event::agent_dispatched(
                &chain(dispatch.parent),
                dispatch.id,
                &dispatch.agent,
                dispatch.child,
                &dispatch.objective,
                self.lease.instance,
            ))
            .await
            .is_ok()
    }

    async fn finished(
        &self,
        dispatch: &orchestrator::Dispatch,
        result: &orchestrator::AgentResult,
    ) -> bool {
        let kind = match result.status {
            orchestrator::AgentStatus::Cancelled => EventKind::AgentCancelled,
            orchestrator::AgentStatus::Failed => EventKind::AgentFailed,
            _ => EventKind::AgentCompleted,
        };
        let payload = serde_json::to_value(result).unwrap_or_default();
        self.log
            .append(Event::agent_report(
                &chain(dispatch.parent),
                kind,
                dispatch.id,
                dispatch.child,
                payload,
                result.trust,
            ))
            .await
            .is_ok()
    }

    async fn delivered(&self, parent: ulid::Ulid, dispatch: ulid::Ulid) {
        let _ = self
            .log
            .append(Event::agent_delivered(&chain(parent), dispatch))
            .await;
    }

    async fn transcript(&self, child: ulid::Ulid) -> Vec<String> {
        // The first user message is the objective; later ones are steers.
        let mut objective_seen = false;
        self.log
            .events(child)
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::UserMessage => {
                    let text = event.payload["text"].as_str().unwrap_or_default();
                    let label = if objective_seen { "steer" } else { "objective" };
                    objective_seen = true;
                    Some(format!("{label}: {text}"))
                }
                EventKind::ModelText => Some(format!(
                    "said: {}",
                    event.payload["text"].as_str().unwrap_or_default()
                )),
                EventKind::ModelIntent => Some(format!(
                    "called {}({})",
                    event.payload["tool"].as_str().unwrap_or("?"),
                    event.payload["args"]
                )),
                EventKind::ToolObs => Some(format!(
                    "  → {}",
                    event.payload["status"].as_str().unwrap_or("ok")
                )),
                _ => None,
            })
            .collect()
    }

    async fn recorded(
        &self,
        parent: ulid::Ulid,
        dispatch: ulid::Ulid,
        agent: &str,
        child: ulid::Ulid,
        patch: &orchestrator::Patch,
    ) -> bool {
        // On the owner's chain, like every other outbox row. This event *is* the
        // work: the caller keeps the worktree when it fails, so the answer has
        // to be honest rather than swallowed.
        let payload = match serde_json::to_value(patch) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(target: "medha_agents", %error, "could not serialize a patch");
                return false;
            }
        };
        match self
            .log
            .append(Event::agent_patch(
                &chain(parent),
                dispatch,
                agent,
                child,
                payload,
            ))
            .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::error!(target: "medha_agents", %error, "could not record a patch");
                false
            }
        }
    }

    async fn applied(&self, parent: ulid::Ulid, dispatch: ulid::Ulid) {
        let _ = self
            .log
            .append(Event::agent_applied(&chain(parent), dispatch))
            .await;
    }

    async fn unapplied(&self, parent: ulid::Ulid) -> Vec<orchestrator::Pending> {
        let mut recorded: Vec<orchestrator::Pending> = Vec::new();
        let mut applied: Vec<String> = Vec::new();
        for event in self.log.events(parent).await {
            match event.kind {
                EventKind::AgentPatch => {
                    if let Some(patch) = event.payload.get("patch").cloned()
                        && let Ok(patch) = serde_json::from_value(patch)
                    {
                        recorded.push(orchestrator::Pending {
                            agent: field(&event, "agent").unwrap_or_else(|| "agent".into()),
                            session: field(&event, "child").unwrap_or_default(),
                            dispatch: handout(&event),
                            patch,
                        });
                    }
                }
                EventKind::AgentApplied => applied.push(handout(&event)),
                _ => {}
            }
        }
        recorded
            .into_iter()
            .filter(|pending| !applied.contains(&pending.dispatch))
            .collect()
    }

    async fn last_activity(&self, child: ulid::Ulid) -> Option<f64> {
        self.log.events(child).await.last().map(|event| event.ts)
    }

    async fn reap_abandoned(&self, parent: ulid::Ulid) -> usize {
        // One pass over the owner's chain: every dispatch, every terminal event.
        // A foreign instance is only a candidate. Its OS lease must be
        // provably unlocked before recovery can claim the dispatch.
        let mut open: Vec<(String, String, String, String, String)> = Vec::new();
        let mut closed: Vec<String> = Vec::new();
        for event in self.log.events(parent).await {
            let Some(child) = field(&event, "child") else {
                continue;
            };
            match event.kind {
                EventKind::AgentSpawned => {
                    let instance = event.payload["instance"].as_str().unwrap_or_default();
                    if instance != self.lease.instance.to_string() {
                        let agent = event.payload["agent"].as_str().unwrap_or("agent");
                        let objective = event.payload["objective"].as_str().unwrap_or_default();
                        open.push((
                            handout(&event),
                            child,
                            agent.to_string(),
                            objective.to_string(),
                            instance.to_string(),
                        ));
                    }
                }
                EventKind::AgentCompleted | EventKind::AgentFailed | EventKind::AgentCancelled => {
                    closed.push(handout(&event))
                }
                _ => {}
            }
        }
        open.retain(|(dispatch, _, _, _, _)| !closed.contains(dispatch));

        let mut recovered = 0;
        for (dispatch, child, agent, objective, instance) in &open {
            match self.owner_is_live(instance) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "medha_agents",
                        %error, %instance,
                        "could not validate agent process lease; leaving dispatch open"
                    );
                    continue;
                }
            }
            // Only one process may recover this dispatch. A second reaper
            // either sees this lock busy or, after it is released, re-folds
            // the terminal event written below.
            let Some(_claim) = (match self.claim_recovery(dispatch) {
                Ok(claim) => claim,
                Err(error) => {
                    tracing::warn!(
                        target: "medha_agents",
                        %error, %dispatch,
                        "could not claim abandoned-agent recovery"
                    );
                    continue;
                }
            }) else {
                continue;
            };
            let now_closed = self.log.events(parent).await.into_iter().any(|event| {
                matches!(
                    event.kind,
                    EventKind::AgentCompleted | EventKind::AgentFailed | EventKind::AgentCancelled
                ) && handout(&event) == *dispatch
            });
            if now_closed {
                continue;
            }

            let (Ok(id), Ok(handout)) =
                (child.parse::<ulid::Ulid>(), dispatch.parse::<ulid::Ulid>())
            else {
                continue;
            };
            // "Unknown", not "failed": the child may have completed its work
            // and died before recording it. Claiming it failed would be a
            // guess, and the parent would act on it as if it were a finding.
            let result = orchestrator::AgentResult {
                agent: agent.clone(),
                session: child.clone(),
                dispatch: dispatch.clone(),
                status: orchestrator::AgentStatus::Failed,
                summary: format!(
                    "[outcome unknown — Medha exited while this agent was running, so it never \
                     recorded a result. It may have finished its work, or none of it. Its \
                     objective was: {objective}]\n\nRead what it actually did with \
                     agent.transcript('{child}'), or re-run it."
                ),
                artifact: None,
                turns: 0,
                tool_calls: 0,
                duration_ms: 0,
                trust: kernel::TrustLabel::Tool,
                patch: None,
            };
            let payload = serde_json::to_value(&result).unwrap_or_default();
            // Writing the terminal event is what makes this idempotent: the
            // next pass sees the dispatch as closed.
            if self
                .log
                .append(Event::agent_report(
                    &chain(parent),
                    EventKind::AgentFailed,
                    handout,
                    id,
                    payload,
                    kernel::TrustLabel::Tool,
                ))
                .await
                .is_ok()
            {
                recovered += 1;
            }
        }
        recovered
    }

    async fn undelivered(&self, parent: ulid::Ulid) -> Vec<orchestrator::AgentResult> {
        let mut ready: Vec<(String, orchestrator::AgentResult)> = Vec::new();
        let mut delivered: Vec<String> = Vec::new();
        for event in self.log.events(parent).await {
            let dispatch = handout(&event);
            match event.kind {
                EventKind::AgentCompleted | EventKind::AgentFailed | EventKind::AgentCancelled => {
                    if let Ok(mut result) =
                        serde_json::from_value::<orchestrator::AgentResult>(event.payload.clone())
                    {
                        // Rows without dispatch ids remain keyed by child session.
                        result.dispatch = dispatch.clone();
                        ready.push((dispatch, result));
                    }
                }
                EventKind::AgentDelivered => delivered.push(dispatch),
                EventKind::ToolObs => {
                    if let Some(dispatches) = event
                        .payload
                        .get("payload")
                        .and_then(|payload| payload.get(orchestrator::REPORT_ACKS_FIELD))
                        .and_then(serde_json::Value::as_array)
                    {
                        delivered.extend(
                            dispatches
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string),
                        );
                    }
                }
                _ => {}
            }
        }
        ready.sort_by(|(a, _), (b, _)| a.cmp(b));
        ready
            .into_iter()
            .filter(|(dispatch, _)| !delivered.contains(dispatch))
            .map(|(_, result)| result)
            .collect()
    }
}

/// Prefixes approval scope with child identity to prevent cross-agent grants.
struct AttributedGate {
    inner: Arc<dyn kernel::HumanGate>,
    agent: String,
}

#[async_trait::async_trait]
impl kernel::HumanGate for AttributedGate {
    async fn confirm(
        &self,
        action: &str,
        detail: Option<&str>,
        escalated: bool,
    ) -> kernel::Approval {
        self.inner
            .confirm(
                &format!("agent '{}' · {action}", self.agent),
                detail,
                escalated,
            )
            .await
    }
}

pub struct KernelRunner<P: Provider, L: EventLog> {
    /// Weak to break the kernel/executor/control-plane ownership cycle.
    kernel: std::sync::Weak<Kernel<P, L>>,
}

impl<P: Provider, L: EventLog> KernelRunner<P, L> {
    pub fn new(kernel: &Arc<Kernel<P, L>>) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
        }
    }
}

#[async_trait::async_trait]
impl<P: Provider + 'static, L: EventLog + 'static> ChildRunner for KernelRunner<P, L> {
    async fn run(&self, run: ChildRun) -> Result<ChildOutcome, String> {
        let Some(kernel) = self.kernel.upgrade() else {
            return Err("this session is shutting down".into());
        };
        // Derived, not rebuilt: pricing, tool parallelism and progressive
        // context are inherited, so a child meters real cost against the tree's
        // shared ceiling and behaves like the session that spawned it.
        let child = kernel
            .derive(
                Arc::clone(&run.executor),
                // Its own: token accounting, the compaction latches and the last
                // summary all describe one conversation, and children run
                // concurrently with the parent and each other.
                kernel
                    .context
                    .fork()
                    .unwrap_or_else(|| Arc::clone(&kernel.context)),
                // Attributed: the approval card names the agent, so an unexpected
                // prompt is answerable.
                Arc::new(AttributedGate {
                    inner: Arc::clone(&kernel.gate),
                    agent: run.spec.name.clone(),
                }),
            )
            // The parent's verifier is rooted at the parent's checkout. The
            // orchestrator verifies this writer's extracted patch in its own
            // worktree, so inheriting that verifier both checks the wrong tree
            // and runs the build twice.
            .with_verifier(Arc::new(kernel::NoVerify));

        let session = Session {
            id: run.session,
            done: false,
            autonomy: kernel::AutonomyLevel::Careful,
        };
        let budget = run.budget.clone();
        let mut messages = run.history.clone();
        messages.push(Message::new(Role::User, child_prompt(&run)));

        // The child's chain opens with what it was asked to do, so its session
        // stands on its own when read back later.
        let tools: Vec<String> = run.executor.specs().into_iter().map(|s| s.name).collect();
        let _ = kernel
            .log
            .append(Event::agent_spawned(
                &session,
                &run.spec.name,
                &run.spec.objective,
                &tools,
            ))
            .await;

        // The queue owns cancellation, allowing `run_session` to settle in-flight
        // tools and the transcript. Pass the steer queue through so accepted text
        // reaches the child's next turn boundary.
        let sink = AgentSink {
            progress: run.progress.clone(),
        };
        let outcome = child
            .run_session(&session, messages, budget, &sink, Some(run.interrupts))
            .await;
        let (transcript, stop) = match outcome {
            Ok(result) => result,
            Err(error) => {
                let _ = kernel
                    .log
                    .append(Event::agent_finished(
                        &session,
                        EventKind::AgentFailed,
                        &run.spec.name,
                        &error.to_string(),
                        TrustLabel::Tool,
                    ))
                    .await;
                return Err(error.to_string());
            }
        };

        let last = transcript
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
            .map(|message| message.content.clone());
        let summary = match (&stop, last) {
            (StopReason::Finished, Some(text)) => text,
            (StopReason::Finished, None) => "the agent finished without reporting anything".into(),
            (_, Some(text)) => format!(
                "[incomplete — the agent was stopped before it reported. \
                 Treat the following as where it had got to, not as an answer. \
                 Re-run with a narrower objective or a higher turn budget.]\n\n{text}"
            ),
            (_, None) => "the agent was stopped before it produced anything".into(),
        };
        let tool_calls = transcript
            .iter()
            .map(|message| message.tool_calls.len() as u32)
            .sum();
        let turns = transcript
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count() as u32;

        // Every observation the child made is already labelled, so the weakest
        // one is what its summary is worth.
        let trust = orchestrator::least_trusted(
            kernel
                .log
                .events(run.session)
                .await
                .into_iter()
                .filter(|event| event.kind == EventKind::ToolObs)
                .map(|event| event.trust),
        );
        let status = match stop {
            StopReason::Finished => AgentStatus::Completed,
            StopReason::Budget(_) => AgentStatus::Exhausted,
            StopReason::Interrupted => AgentStatus::Cancelled,
        };
        let _ = kernel
            .log
            .append(Event::agent_finished(
                &session,
                match status {
                    AgentStatus::Cancelled => EventKind::AgentCancelled,
                    AgentStatus::Failed => EventKind::AgentFailed,
                    _ => EventKind::AgentCompleted,
                },
                &run.spec.name,
                &format!("{turns} turn(s), {tool_calls} tool call(s)"),
                trust,
            ))
            .await;

        Ok(ChildOutcome {
            status,
            summary,
            turns,
            tool_calls,
            trust,
        })
    }

    fn settles_cancellation(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::{Dispatch, Outbox};
    use ulid::Ulid;

    fn log_at(name: &str) -> (Arc<store::SqliteLog>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("medha-{name}-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = store::SqliteLog::open(dir.join("events.db")).unwrap();
        (Arc::new(log), dir)
    }

    fn dispatch(parent: Ulid, agent: &str) -> Dispatch {
        Dispatch {
            id: Ulid::new(),
            agent: agent.into(),
            child: Ulid::new(),
            parent,
            objective: format!("{agent}'s objective"),
        }
    }

    fn outbox(log: &Arc<store::SqliteLog>, directory: &Path) -> LogOutbox<store::SqliteLog> {
        LogOutbox::new(log.clone(), directory.join("agent-leases")).unwrap()
    }

    fn report(dispatch: &Dispatch, summary: &str) -> orchestrator::AgentResult {
        orchestrator::AgentResult {
            agent: "reporter".into(),
            session: dispatch.child.to_string(),
            dispatch: dispatch.id.to_string(),
            status: orchestrator::AgentStatus::Completed,
            summary: summary.into(),
            artifact: None,
            turns: 3,
            tool_calls: 2,
            duration_ms: 10,
            trust: TrustLabel::Tool,
            patch: None,
        }
    }

    #[tokio::test]
    async fn live_process_lease_child() {
        if std::env::var_os("MEDHA_AGENT_LEASE_CHILD").is_none() {
            return;
        }
        let directory = PathBuf::from(std::env::var_os("MEDHA_AGENT_LOG_DIR").unwrap());
        let parent = std::env::var("MEDHA_AGENT_PARENT")
            .unwrap()
            .parse()
            .unwrap();
        let dispatch_id = std::env::var("MEDHA_AGENT_DISPATCH")
            .unwrap()
            .parse()
            .unwrap();
        let child = std::env::var("MEDHA_AGENT_CHILD").unwrap().parse().unwrap();
        let ready = PathBuf::from(std::env::var_os("MEDHA_AGENT_READY").unwrap());
        let stop = PathBuf::from(std::env::var_os("MEDHA_AGENT_STOP").unwrap());
        let log = Arc::new(store::SqliteLog::open(directory.join("events.db")).unwrap());
        let outbox = outbox(&log, &directory);
        let dispatch = Dispatch {
            id: dispatch_id,
            agent: "live-child".into(),
            child,
            parent,
            objective: "stay alive while another process reaps".into(),
        };
        assert!(outbox.dispatched(&dispatch).await);
        std::fs::write(&ready, b"ready").unwrap();
        while !stop.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_child_abandoned_by_a_dead_process_is_reported_not_forgotten() {
        let (log, dir) = log_at("orphan");
        let parent = Ulid::new();

        let died = outbox(&log, &dir);
        let abandoned = dispatch(parent, "surveyor");
        died.dispatched(&abandoned).await;
        assert!(died.undelivered(parent).await.is_empty());
        drop(died);

        let restarted = outbox(&log, &dir);
        assert_eq!(restarted.reap_abandoned(parent).await, 1);

        let reported = restarted.undelivered(parent).await;
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].session, abandoned.child.to_string());
        assert!(
            reported[0].summary.contains("outcome unknown"),
            "must not claim to know the child failed: {}",
            reported[0].summary
        );
    }

    #[tokio::test]
    async fn reaping_is_idempotent_and_leaves_live_children_alone() {
        let (log, dir) = log_at("orphan-idem");
        let parent = Ulid::new();

        let died = outbox(&log, &dir);
        died.dispatched(&dispatch(parent, "abandoned")).await;
        drop(died);

        let live = outbox(&log, &dir);
        live.dispatched(&dispatch(parent, "still-running")).await;

        assert_eq!(live.reap_abandoned(parent).await, 1);
        assert_eq!(live.reap_abandoned(parent).await, 0);

        let reported = live.undelivered(parent).await;
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].agent, "abandoned");
    }

    #[tokio::test]
    async fn a_live_foreign_process_is_never_reaped_and_a_killed_one_is_recovered_once() {
        let directory =
            std::env::temp_dir().join(format!("medha-agent-two-process-{}", Ulid::new()));
        std::fs::create_dir_all(&directory).unwrap();
        let parent = Ulid::new();
        let dispatch_id = Ulid::new();
        let child_id = Ulid::new();
        let ready = directory.join("ready");
        let stop = directory.join("stop");
        let executable = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(executable)
            .arg("--exact")
            .arg("agents::tests::live_process_lease_child")
            .arg("--nocapture")
            .env("MEDHA_AGENT_LEASE_CHILD", "1")
            .env("MEDHA_AGENT_LOG_DIR", &directory)
            .env("MEDHA_AGENT_PARENT", parent.to_string())
            .env("MEDHA_AGENT_DISPATCH", dispatch_id.to_string())
            .env("MEDHA_AGENT_CHILD", child_id.to_string())
            .env("MEDHA_AGENT_READY", &ready)
            .env("MEDHA_AGENT_STOP", &stop)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !ready.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("lease-holder child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "lease-holder child never became ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let log = Arc::new(store::SqliteLog::open(directory.join("events.db")).unwrap());
        let reaper = outbox(&log, &directory);
        assert_eq!(
            reaper.reap_abandoned(parent).await,
            0,
            "a second live Medha instance marked the first one's child abandoned"
        );
        assert!(reaper.undelivered(parent).await.is_empty());

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(reaper.reap_abandoned(parent).await, 1);
        assert_eq!(reaper.reap_abandoned(parent).await, 0);
        let reports = reaper.undelivered(parent).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].dispatch, dispatch_id.to_string());
        assert!(reports[0].summary.contains("outcome unknown"));
    }

    #[tokio::test]
    async fn concurrent_reapers_claim_one_terminal_result() {
        let (log, directory) = log_at("concurrent-reapers");
        let parent = Ulid::new();
        let abandoned = dispatch(parent, "abandoned");
        let owner = outbox(&log, &directory);
        owner.dispatched(&abandoned).await;
        drop(owner);

        let first = outbox(&log, &directory);
        let second = outbox(&log, &directory);
        let (left, right) =
            tokio::join!(first.reap_abandoned(parent), second.reap_abandoned(parent));
        assert_eq!(left + right, 1);
        let reports = first.undelivered(parent).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].dispatch, abandoned.id.to_string());
    }

    #[tokio::test]
    async fn a_child_that_did_report_is_never_reaped() {
        let (log, dir) = log_at("orphan-finished");
        let parent = Ulid::new();

        let first = outbox(&log, &dir);
        let finished = dispatch(parent, "reporter");
        first.dispatched(&finished).await;
        first
            .finished(&finished, &report(&finished, "the real answer"))
            .await;

        let restarted = outbox(&log, &dir);
        assert_eq!(restarted.reap_abandoned(parent).await, 0);
        let reported = restarted.undelivered(parent).await;
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].summary, "the real answer");
    }

    #[tokio::test]
    async fn an_abandoned_child_is_delivered_once_like_any_other() {
        let (log, dir) = log_at("orphan-once");
        let parent = Ulid::new();
        outbox(&log, &dir)
            .dispatched(&dispatch(parent, "surveyor"))
            .await;

        let restarted = outbox(&log, &dir);
        restarted.reap_abandoned(parent).await;
        let handout: Ulid = restarted.undelivered(parent).await[0]
            .dispatch
            .parse()
            .unwrap();
        restarted.delivered(parent, handout).await;
        assert!(restarted.undelivered(parent).await.is_empty());
    }

    #[tokio::test]
    async fn a_follow_up_on_the_same_session_still_reports() {
        let (log, dir) = log_at("followup-delivery");
        let parent = Ulid::new();
        let outbox = outbox(&log, &dir);

        let first = dispatch(parent, "writer");
        outbox.dispatched(&first).await;
        outbox
            .finished(&first, &report(&first, "first answer"))
            .await;
        outbox.delivered(parent, first.id).await;
        assert!(outbox.undelivered(parent).await.is_empty());

        let again = Dispatch {
            id: Ulid::new(),
            child: first.child,
            ..first
        };
        outbox.dispatched(&again).await;
        outbox
            .finished(&again, &report(&again, "second answer"))
            .await;

        let waiting = outbox.undelivered(parent).await;
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].summary, "second answer");
    }
}

#[cfg(test)]
mod child_prompt_tests {
    use super::*;
    use kernel::{BlastRadius, Executor, Observation, ToolCategory, ToolIntent, ToolSpec};
    use orchestrator::AgentSpec;

    struct Holding(Vec<&'static str>);

    #[async_trait::async_trait]
    impl Executor for Holding {
        fn specs(&self) -> Vec<ToolSpec> {
            self.0
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_string(),
                    description: String::new(),
                    schema: serde_json::json!({}),
                    blast_radius: BlastRadius::Read,
                    category: ToolCategory::Other,
                    icon: "•".into(),
                })
                .collect()
        }
        async fn execute(&self, intent: &ToolIntent) -> Observation {
            Observation::ok(&intent.id, serde_json::Value::Null)
        }
    }

    fn prompt_for(tools: Vec<&'static str>, workspace: Option<PathBuf>) -> String {
        child_prompt(&ChildRun {
            session: ulid::Ulid::new(),
            spec: AgentSpec {
                objective: "survey the crate".into(),
                ..Default::default()
            },
            history: Vec::new(),
            executor: Arc::new(Holding(tools)),
            budget: kernel::Budget::turns(10),
            cancel: tokio_util::sync::CancellationToken::new(),
            workspace,
            interrupts: kernel::InterruptQueue::pair().1,
            progress: kernel::ProgressHandle::new().0,
        })
    }

    #[test]
    fn a_child_is_told_it_can_delegate_only_when_it_actually_can() {
        let reader = prompt_for(vec!["fs.read"], None);
        assert!(reader.contains("You cannot delegate"));

        let writer = prompt_for(
            vec!["fs.read", "agent.spawn"],
            Some(PathBuf::from("/tmp/wt")),
        );
        assert!(writer.contains("You may delegate"));
        assert!(!writer.contains("You cannot delegate"));
    }

    #[test]
    fn a_child_is_told_about_messaging_only_when_it_holds_the_tool() {
        assert!(!prompt_for(vec!["fs.read"], None).contains("send a message"));
        assert!(
            prompt_for(vec!["fs.read", "agent.message"], None).contains("send a message"),
            "a child that can reach its parent and is not told so will not"
        );
    }

    #[test]
    fn every_child_is_told_it_cannot_ask_but_may_be_stopped_for_approval() {
        let prompt = prompt_for(vec!["fs.read"], None);
        assert!(prompt.contains("cannot ask a question"));
        assert!(prompt.contains("approval"));
    }
}
