//! Who exists, where, and how many.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::path::AgentPath;
use crate::{AgentStatus, Error};

/// One child, live or settled.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub path: AgentPath,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
    pub state: State,
    /// The capabilities this agent was admitted with, kept so a follow-up
    /// resumes the same agent rather than a differently-privileged one wearing
    /// its name. Not part of the listing the model reads.
    #[serde(skip)]
    pub write: bool,
    #[serde(skip)]
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Settled(AgentStatus),
}

impl Agent {
    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running)
    }
}

/// Handles for acting on a live child.
pub(crate) struct Live {
    pub(crate) cancel: CancellationToken,
    pub(crate) steer: kernel::InterruptHandle,
    /// What this agent was admitted with, so its own children inherit its
    /// ceilings and its pool rather than the root's.
    pub(crate) budget: kernel::Budget,
}

#[derive(Default)]
struct Tree {
    agents: HashMap<AgentPath, Agent>,
    live: HashMap<AgentPath, Live>,
    settled: Vec<AgentPath>,
    /// Names taken but not yet started. Held apart from `agents` so a listing
    /// never has to know about a half-built entry to skip it.
    reserved: HashSet<AgentPath>,
}

impl Tree {
    fn contains_key(&self, path: &AgentPath) -> bool {
        self.agents.contains_key(path) || self.reserved.contains(path)
    }
}

/// The agent tree for one session, shared by every descendant.
#[derive(Default)]
pub struct AgentRegistry {
    tree: Mutex<Tree>,
    max_settled: usize,
}

/// Settled agents kept addressable. A transcript stays readable past this; only
/// the listing forgets.
const MAX_SETTLED: usize = 32;

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            tree: Mutex::new(Tree::default()),
            max_settled: MAX_SETTLED,
        }
    }

    /// Claim a free name under `parent`, preferring `wanted`, before any work
    /// starts.
    ///
    /// Finding the name and taking it happen under one lock. Split across two,
    /// two concurrent spawns settle on the same free name and the loser is
    /// refused for a collision it never chose — auto-generated names make that
    /// the common case, not the rare one.
    pub(crate) fn claim(
        self: &Arc<Self>,
        parent: &AgentPath,
        wanted: &str,
    ) -> Result<(AgentPath, Reservation), Error> {
        let base = parent
            .child(wanted)
            .map_err(|error| Error::BadName(error.to_string()))?;
        let mut tree = self.lock();
        let path = match tree.contains_key(&base) {
            false => base,
            true => (2..100)
                .find_map(|n| {
                    let candidate = parent.child(&format!("{wanted}-{n}")).ok()?;
                    (!tree.contains_key(&candidate)).then_some(candidate)
                })
                .ok_or_else(|| Error::NameTaken(base.to_string()))?,
        };
        tree.reserved.insert(path.clone());
        drop(tree);
        Ok((
            path.clone(),
            Reservation {
                registry: Arc::clone(self),
                path: Some(path),
                restore: None,
            },
        ))
    }

    /// Re-take a settled agent's own name, so a follow-up continues that agent
    /// rather than creating a near-namesake beside it.
    pub(crate) fn revive(self: &Arc<Self>, path: &AgentPath) -> Result<Reservation, Error> {
        let mut tree = self.lock();
        match tree.agents.get(path) {
            Some(agent) if !agent.is_running() => {}
            Some(_) => return Err(Error::NameTaken(path.to_string())),
            None => return Err(Error::UnknownAgent(path.to_string())),
        }
        // Held for the reservation's lifetime: a follow-up that then fails
        // admission — at capacity, no isolation for a writer — would otherwise
        // have deleted the agent it was asked to continue, leaving nothing to
        // retry against and no record that it ever existed.
        let previous = tree.agents.remove(path);
        tree.settled.retain(|settled| settled != path);
        tree.reserved.insert(path.clone());
        Ok(Reservation {
            registry: Arc::clone(self),
            path: Some(path.clone()),
            restore: previous,
        })
    }

    fn started(&self, agent: Agent, live: Live) {
        let mut tree = self.lock();
        tree.reserved.remove(&agent.path);
        tree.live.insert(agent.path.clone(), live);
        tree.agents.insert(agent.path.clone(), agent);
    }

    pub(crate) fn settled(&self, path: &AgentPath, status: AgentStatus) {
        let mut tree = self.lock();
        tree.live.remove(path);
        if let Some(agent) = tree.agents.get_mut(path) {
            agent.state = State::Settled(status);
        }
        tree.settled.retain(|settled| settled != path);
        tree.settled.push(path.clone());
        while tree.settled.len() > self.max_settled {
            let oldest = tree.settled.remove(0);
            tree.agents.remove(&oldest);
        }
    }

    pub fn running(&self) -> Vec<Agent> {
        self.select(|agent| agent.is_running())
    }

    pub fn all(&self) -> Vec<Agent> {
        self.select(|_| true)
    }

    fn select(&self, keep: impl Fn(&Agent) -> bool) -> Vec<Agent> {
        let mut found: Vec<Agent> = self
            .lock()
            .agents
            .values()
            .filter(|agent| keep(agent))
            .cloned()
            .collect();
        found.sort_by(|a, b| a.started_ms.cmp(&b.started_ms).then(a.path.cmp(&b.path)));
        found
    }

    /// Find an agent by path — relative to `from`, or absolute — or by session
    /// id.
    ///
    /// No bare-name fallback. `/survey/parser` and `/writer/parser` share a
    /// name, so resolving one would be a hash-order coin toss between two
    /// agents belonging to different owners.
    pub fn find(&self, from: &AgentPath, reference: &str) -> Option<Agent> {
        let tree = self.lock();
        if let Ok(path) = from.resolve(reference)
            && let Some(agent) = tree.agents.get(&path)
        {
            return Some(agent.clone());
        }
        tree.agents
            .values()
            .find(|agent| agent.session == reference)
            .cloned()
    }

    pub(crate) fn cancel(&self, path: &AgentPath) -> bool {
        match self.lock().live.get(path) {
            Some(live) => {
                live.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// The live cancellation token of a running agent, so a child can be
    /// derived from its own parent rather than from the root of the tree.
    pub(crate) fn token(&self, path: &AgentPath) -> Option<CancellationToken> {
        self.lock().live.get(path).map(|live| live.cancel.clone())
    }

    /// A running agent's own budget, for sizing the children it spawns.
    pub(crate) fn budget(&self, path: &AgentPath) -> Option<kernel::Budget> {
        self.lock().live.get(path).map(|live| live.budget.clone())
    }

    /// Watch what is being queued against `path`'s own session.
    pub(crate) fn activity(&self, path: &AgentPath) -> Option<kernel::Activity> {
        self.lock().live.get(path).map(|live| live.steer.activity())
    }

    pub(crate) fn steer(&self, path: &AgentPath, text: &str) -> bool {
        self.steer_labelled(path, text, kernel::TrustLabel::User)
    }

    /// Queue text that did not come from the operator — a child's report above
    /// all — so its label reaches the receiving session's taint window.
    pub(crate) fn steer_labelled(
        &self,
        path: &AgentPath,
        text: &str,
        trust: kernel::TrustLabel,
    ) -> bool {
        match self.lock().live.get(path) {
            Some(live) => live.steer.steer_labelled(text, trust),
            None => false,
        }
    }

    /// Undo an admission whose durable dispatch could not be written. No child
    /// has started yet, so removing its live entry is lossless; a failed
    /// follow-up restores the settled agent it displaced.
    pub(crate) fn rollback_start(&self, path: &AgentPath, restore: Option<Agent>) {
        let mut tree = self.lock();
        tree.live.remove(path);
        tree.agents.remove(path);
        tree.settled.retain(|settled| settled != path);
        if let Some(agent) = restore {
            tree.settled.push(path.clone());
            tree.agents.insert(path.clone(), agent);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tree> {
        self.tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A claimed path, released on drop unless committed — so a spawn that fails
/// between reserving and starting does not leave the name burned.
pub(crate) struct Reservation {
    registry: Arc<AgentRegistry>,
    path: Option<AgentPath>,
    /// The settled agent a revive took off the roster, put back if the
    /// reservation is dropped without being committed.
    restore: Option<Agent>,
}

impl Reservation {
    pub(crate) fn commit(mut self, agent: Agent, live: Live) {
        self.path = None;
        self.restore = None;
        self.registry.started(agent, live);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let mut tree = self.registry.lock();
        tree.reserved.remove(&path);
        // Uncommitted: whatever this reservation displaced goes back, or a
        // failed follow-up silently destroys the agent it meant to resume.
        if let Some(agent) = self.restore.take() {
            tree.settled.retain(|settled| settled != &path);
            tree.settled.push(path.clone());
            tree.agents.insert(path, agent);
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
