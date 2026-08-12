use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::path::AgentPath;
use crate::{AgentStatus, Error};

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub path: AgentPath,
    pub session: String,
    pub objective: String,
    pub started_ms: u64,
    pub state: State,
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

pub(crate) enum Followup {
    Delivered(Agent),
    Resume(Agent),
}

impl Agent {
    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running)
    }
}

pub(crate) struct Live {
    pub(crate) cancel: CancellationToken,
    pub(crate) steer: kernel::InterruptHandle,
    pub(crate) budget: kernel::Budget,
}

#[derive(Default)]
struct Tree {
    agents: HashMap<AgentPath, Agent>,
    live: HashMap<AgentPath, Live>,
    settled: Vec<AgentPath>,
    /// Names reserved atomically before startup and excluded from listings.
    reserved: HashSet<AgentPath>,
    /// Evicted path-to-session mappings retained for transcript resolution.
    archived: Vec<(AgentPath, String)>,
}

impl Tree {
    fn contains_key(&self, path: &AgentPath) -> bool {
        self.agents.contains_key(path) || self.reserved.contains(path)
    }
}

#[derive(Default)]
pub struct AgentRegistry {
    tree: Mutex<Tree>,
    max_settled: usize,
}

const MAX_SETTLED: usize = 32;

const MAX_ARCHIVED: usize = 4_096;

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            tree: Mutex::new(Tree::default()),
            max_settled: MAX_SETTLED,
        }
    }

    /// Select and reserve a free child name under one lock.
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

    pub(crate) fn revive(self: &Arc<Self>, path: &AgentPath) -> Result<Reservation, Error> {
        let mut tree = self.lock();
        match tree.agents.get(path) {
            Some(agent) if !agent.is_running() => {}
            Some(_) => return Err(Error::NameTaken(path.to_string())),
            None => return Err(Error::UnknownAgent(path.to_string())),
        }
        // Restore the settled entry if follow-up admission fails.
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
            if let Some(evicted) = tree.agents.remove(&oldest) {
                tree.archived.retain(|(path, _)| path != &oldest);
                tree.archived.push((oldest, evicted.session));
                if tree.archived.len() > MAX_ARCHIVED {
                    tree.archived.remove(0);
                }
            }
        }
    }

    /// Resolve an evicted descendant without widening live-agent reachability.
    pub fn archived_session(&self, from: &AgentPath, reference: &str) -> Option<String> {
        let tree = self.lock();
        if let Ok(path) = from.resolve(reference)
            && path.under(from)
            && &path != from
            && let Some((_, session)) = tree.archived.iter().find(|(entry, _)| entry == &path)
        {
            return Some(session.clone());
        }
        tree.archived
            .iter()
            .find(|(path, session)| session == reference && path.under(from) && path != from)
            .map(|(_, session)| session.clone())
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

    pub(crate) fn token(&self, path: &AgentPath) -> Option<CancellationToken> {
        self.lock().live.get(path).map(|live| live.cancel.clone())
    }

    pub(crate) fn budget(&self, path: &AgentPath) -> Option<kernel::Budget> {
        self.lock().live.get(path).map(|live| live.budget.clone())
    }

    pub(crate) fn activity(&self, path: &AgentPath) -> Option<kernel::Activity> {
        self.lock().live.get(path).map(|live| live.steer.activity())
    }

    pub(crate) fn steer(&self, path: &AgentPath, text: &str) -> bool {
        self.steer_labelled(path, text, kernel::TrustLabel::User)
    }

    /// Atomically choose between delivering to a live run and resuming a
    /// settled one. Splitting the state check from `steer` loses a follow-up
    /// when settlement removes the live handle in between.
    pub(crate) fn followup(&self, path: &AgentPath, text: &str) -> Option<Followup> {
        let tree = self.lock();
        let agent = tree.agents.get(path)?.clone();
        match agent.state {
            State::Settled(_) => Some(Followup::Resume(agent)),
            State::Running => {
                let live = tree.live.get(path)?;
                live.steer
                    .steer_labelled(text, kernel::TrustLabel::User)
                    .then_some(Followup::Delivered(agent))
            }
        }
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
