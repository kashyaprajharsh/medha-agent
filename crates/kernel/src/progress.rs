//! What a session is doing *right now*, as distinct from what it has done.
//!
//! The event log answers history: durable, ordered, verifiable, and slow. It is
//! the wrong instrument for liveness — a session composing a reply writes
//! nothing, so "no recent event" reads identically for a model that is thinking
//! and a connection that has died. Inferring one from the other is how a wedged
//! agent becomes indistinguishable from a busy one.
//!
//! So liveness gets its own channel: in-memory, latest-wins, lossy on purpose.
//! Nothing here is durable and nothing here is a source of truth about the past.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// What a session is doing at this instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Between turns, nothing in flight.
    Idle,
    /// Waiting on the provider for this turn's reply.
    Generating,
    /// Running a tool. `target` is the file or command where one is known, so a
    /// watcher can say what is being read rather than only that something is.
    InTool {
        tool: String,
        target: Option<String>,
    },
    /// Blocked on a human decision.
    AwaitingApproval { action: String },
    /// Finished. Terminal: no further phase follows.
    Settled,
}

impl Phase {
    /// Whether elapsed time in this phase is evidence of a stall.
    ///
    /// A person is not a stall. An agent waiting on an approval card has done
    /// nothing wrong and stopping it for taking too long punishes the operator
    /// for stepping away, so this phase is exempt from every inactivity bound.
    pub fn is_stall_evidence(&self) -> bool {
        !matches!(self, Phase::AwaitingApproval { .. } | Phase::Settled)
    }

    /// Short label for a status line.
    pub fn label(&self) -> String {
        match self {
            Phase::Idle => "idle".to_string(),
            Phase::Generating => "generating".to_string(),
            Phase::InTool { tool, target } => match target {
                Some(target) => format!("{tool} {target}"),
                None => tool.clone(),
            },
            Phase::AwaitingApproval { .. } => "waiting on you".to_string(),
            Phase::Settled => "settled".to_string(),
        }
    }
}

/// A session's live state: what it is doing, since when, and what it has spent.
#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: Phase,
    /// When the current phase began — the clock an inactivity bound reads.
    pub since: Instant,
    pub turns: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    pub cost_usd: f64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            since: Instant::now(),
            turns: 0,
            tool_calls: 0,
            tokens: 0,
            cost_usd: 0.0,
        }
    }
}

impl Progress {
    /// How long the current phase has run.
    pub fn in_phase(&self) -> Duration {
        self.since.elapsed()
    }

    /// How long this phase has been stalled, or `None` when the phase is not
    /// evidence of a stall at all.
    pub fn stalled_for(&self) -> Option<Duration> {
        self.phase.is_stall_evidence().then(|| self.in_phase())
    }
}

/// Publishes one session's progress. Cheap to clone and share.
#[derive(Clone)]
pub struct ProgressHandle {
    tx: Arc<watch::Sender<Progress>>,
}

/// Reads one session's progress. Latest-wins: a watcher that falls behind sees
/// the current state, never a backlog of stale ones.
pub type ProgressWatch = watch::Receiver<Progress>;

impl ProgressHandle {
    pub fn new() -> (Self, ProgressWatch) {
        let (tx, rx) = watch::channel(Progress::default());
        (Self { tx: Arc::new(tx) }, rx)
    }

    /// Another reader of this session's state, for a watcher that outlives the
    /// roster's own receiver.
    pub fn subscribe(&self) -> ProgressWatch {
        self.tx.subscribe()
    }

    /// Move to `phase`, restarting the phase clock. A repeat of the current
    /// phase does not restart it: re-reporting `Generating` every chunk would
    /// hide a stream that has gone quiet mid-reply.
    pub fn enter(&self, phase: Phase) {
        self.tx.send_if_modified(|progress| {
            if progress.phase == phase {
                return false;
            }
            if phase == Phase::Generating {
                progress.turns = progress.turns.saturating_add(1);
            }
            progress.phase = phase;
            progress.since = Instant::now();
            true
        });
    }

    /// Note progress within the current phase without moving out of it. This is
    /// what separates a stream that is slow from one that is dead.
    pub fn ticked(&self) {
        self.tx.send_modify(|progress| progress.since = Instant::now());
    }

    pub fn tool_dispatched(&self) {
        self.tx
            .send_modify(|progress| progress.tool_calls = progress.tool_calls.saturating_add(1));
    }

    /// Add a turn's token usage. Providers report per-turn, so these accumulate.
    pub fn metered(&self, tokens: u64) {
        self.tx
            .send_modify(|progress| progress.tokens = progress.tokens.saturating_add(tokens));
    }

    /// Record spend so far. The governor reports a running total, so this
    /// replaces rather than adds — accumulating a total would square it.
    pub fn priced(&self, total_usd: f64) {
        self.tx.send_modify(|progress| progress.cost_usd = total_usd);
    }

    /// Terminal. Held by the registry afterwards so a settled agent still reads
    /// back its final counters.
    pub fn settled(&self) {
        self.enter(Phase::Settled);
    }
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
