//! Budget governor (§4.1, §18.5). The kernel enforces hard per-task ceilings —
//! turns, tokens, cost, wall-clock — as a contract, checked before each turn.
//! Each dimension is optional (`None` = unbounded on that axis). Exhaustion
//! ends the session gracefully and reports which limit was hit (never a
//! mid-tool kill, P10). This is the principled replacement for a hardcoded
//! turn cap: the *user* sets the ceiling; turn-count is just one dimension.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Generous anti-runaway backstop on turns when the user sets nothing.
pub const DEFAULT_MAX_TURNS: u32 = 200;

/// Spend pooled across every session in one agent tree.
///
/// Turns are per-session — a child needs its own working room — but tokens,
/// cost and wall-clock are the user's, and a tree that hands each child a fresh
/// copy of the ceiling has no ceiling at all: three children sequentially, or
/// one nesting three deep, spends the budget three times over.
pub type Allowance = Arc<Mutex<Pooled>>;

/// Shared slot holding the budget a session was started with, so anything that
/// spawns work on its behalf can derive ceilings from the same source.
pub type BudgetHandle = Arc<Mutex<Option<Budget>>>;

#[derive(Debug, Default)]
pub struct Pooled {
    tokens: u64,
    cost_usd: f64,
    started: Option<Instant>,
}

impl Pooled {
    pub fn new() -> Allowance {
        Arc::new(Mutex::new(Self::default()))
    }

    fn elapsed_s(&mut self) -> u64 {
        self.started
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs()
    }
}

#[derive(Debug, Clone)]
pub struct Budget {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
    /// Shared with every descendant, so their spend counts against the same
    /// ceilings. `None` leaves this session accounting only for itself.
    pub pooled: Option<Allowance>,
}

impl Default for Budget {
    fn default() -> Self {
        // Backstop on turns; cost/tokens/wall are opt-in (set per task).
        Self {
            max_turns: Some(DEFAULT_MAX_TURNS),
            max_tokens: None,
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        }
    }
}

impl Budget {
    /// The ceilings a child inherits: the same limits, against the same pool.
    /// Turns are the caller's to set — a child needs its own working room.
    pub fn inherited(&self, max_turns: u32) -> Self {
        Self {
            max_turns: Some(max_turns),
            ..self.clone()
        }
    }

    /// Start pooling the non-turn ceilings across a tree of agents. Keeps an
    /// existing pool, so a child inheriting one stays on its parent's tally.
    pub fn pooling(mut self) -> Self {
        self.pooled.get_or_insert_with(Pooled::new);
        self
    }

    /// A pool of its own, replacing any inherited one. For the start of a task:
    /// carrying the previous task's tally forward means the ceiling is spent
    /// once and never recovered.
    pub fn with_fresh_pool(mut self) -> Self {
        self.pooled = Some(Pooled::new());
        self
    }

    /// A turn-only budget. Handy where a caller has no ceilings to inherit.
    pub fn turns(max_turns: u32) -> Self {
        Self {
            max_turns: Some(max_turns),
            ..Self::default()
        }
    }
}

/// Which ceiling was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStop {
    Turns,
    Tokens,
    Cost,
    Wall,
    /// Even the engine's best compaction couldn't fit under the hard safety
    /// ceiling for this turn — refusing to send rather than risk an API
    /// context-length error (§4.3 emergency_ratio, the second
    /// safety layer above the normal compaction trigger).
    ContextOverflow,
}

impl BudgetStop {
    pub fn label(&self) -> &'static str {
        match self {
            BudgetStop::Turns => "turn cap",
            BudgetStop::Tokens => "token budget",
            BudgetStop::Cost => "cost budget",
            BudgetStop::Wall => "wall-clock limit",
            BudgetStop::ContextOverflow => "context window (nothing left to compact)",
        }
    }
}

/// Tracks consumption against a `Budget` over one task. Created per task; the
/// kernel calls `check()` before each turn and records usage as it goes.
pub struct Governor {
    budget: Budget,
    start: Instant,
    turns: u32,
    tokens: u64,
    cost_usd: f64,
}

impl Governor {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            start: Instant::now(),
            turns: 0,
            tokens: 0,
            cost_usd: 0.0,
        }
    }

    /// Returns `Some(reason)` if any ceiling is reached — stop before the turn.
    ///
    /// Turns are this session's own; everything else is measured against the
    /// pool when there is one, so a tree cannot spend its ceiling once per agent.
    pub fn check(&self) -> Option<BudgetStop> {
        if matches!(self.budget.max_turns, Some(m) if self.turns >= m) {
            return Some(BudgetStop::Turns);
        }
        let (tokens, cost_usd, elapsed_s) = self.spent();
        if matches!(self.budget.max_tokens, Some(m) if tokens >= m) {
            return Some(BudgetStop::Tokens);
        }
        if matches!(self.budget.max_cost_usd, Some(m) if cost_usd >= m) {
            return Some(BudgetStop::Cost);
        }
        if matches!(self.budget.max_wall_s, Some(m) if elapsed_s >= m) {
            return Some(BudgetStop::Wall);
        }
        None
    }

    /// What counts against the ceilings: the pool's totals where one is shared,
    /// this session's own otherwise. A poisoned pool falls back to local rather
    /// than removing the ceiling.
    fn spent(&self) -> (u64, f64, u64) {
        match self
            .budget
            .pooled
            .as_ref()
            .and_then(|pool| pool.lock().ok())
        {
            Some(mut pool) => (pool.tokens, pool.cost_usd, pool.elapsed_s()),
            None => (self.tokens, self.cost_usd, self.start.elapsed().as_secs()),
        }
    }

    pub fn record_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    /// Record real token spend (and cost, when a price is known; 0.0 otherwise).
    pub fn record_tokens(&mut self, total_tokens: u64, cost_usd: f64) {
        self.tokens = self.tokens.saturating_add(total_tokens);
        self.cost_usd += cost_usd;
        if let Some(mut pool) = self
            .budget
            .pooled
            .as_ref()
            .and_then(|pool| pool.lock().ok())
        {
            pool.tokens = pool.tokens.saturating_add(total_tokens);
            pool.cost_usd += cost_usd;
        }
    }

    pub fn turns(&self) -> u32 {
        self.turns
    }
    pub fn tokens(&self) -> u64 {
        self.tokens
    }
    pub fn cost_usd(&self) -> f64 {
        self.cost_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_cap_trips() {
        let mut g = Governor::new(Budget {
            max_turns: Some(2),
            ..Budget::default()
        });
        assert_eq!(g.check(), None);
        g.record_turn();
        assert_eq!(g.check(), None);
        g.record_turn();
        assert_eq!(g.check(), Some(BudgetStop::Turns));
    }

    #[test]
    fn token_budget_trips() {
        let mut g = Governor::new(Budget {
            max_turns: None,
            max_tokens: Some(1000),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        });
        g.record_tokens(600, 0.0);
        assert_eq!(g.check(), None);
        g.record_tokens(600, 0.0);
        assert_eq!(g.check(), Some(BudgetStop::Tokens));
    }

    #[test]
    fn cost_budget_trips_with_real_pricing() {
        // P1-12: with pricing resolved, recorded cost accrues and the cost
        // ceiling actually trips (it could never trip while cost was 0.0).
        let p = crate::types::Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            indicative: false,
        };
        let per_turn = p.cost(100_000, 20_000); // 0.3 + 0.3 = $0.60
        assert!((per_turn - 0.6).abs() < 1e-9);
        let mut g = Governor::new(Budget {
            max_turns: None,
            max_tokens: None,
            max_cost_usd: Some(1.0),
            max_wall_s: None,
            pooled: None,
        });
        g.record_tokens(120_000, per_turn);
        assert_eq!(g.check(), None);
        g.record_tokens(120_000, per_turn);
        assert_eq!(g.check(), Some(BudgetStop::Cost));
    }

    /// Each child used to get a fresh copy of the ceiling, so a tree spent the
    /// user's whole token budget once per agent.
    #[test]
    fn a_childs_spend_counts_against_the_parents_ceiling() {
        let parent = Budget {
            max_turns: Some(10),
            max_tokens: Some(1000),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        }
        .pooling();
        let mut root = Governor::new(parent.clone());
        root.record_tokens(600, 0.0);
        assert_eq!(root.check(), None);

        // A child with its own turns, drawing on the same pool.
        let mut child = Governor::new(parent.inherited(5));
        assert_eq!(child.check(), None, "the parent has not exhausted it yet");
        child.record_tokens(600, 0.0);
        assert_eq!(child.check(), Some(BudgetStop::Tokens));
        assert_eq!(
            root.check(),
            Some(BudgetStop::Tokens),
            "the parent sees what its child spent"
        );
    }

    #[test]
    fn an_unpooled_budget_still_accounts_only_for_itself() {
        let solo = Budget {
            max_turns: None,
            max_tokens: Some(100),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        };
        let mut a = Governor::new(solo.clone());
        let b = Governor::new(solo);
        a.record_tokens(150, 0.0);
        assert_eq!(a.check(), Some(BudgetStop::Tokens));
        assert_eq!(b.check(), None);
    }

    #[test]
    fn unbounded_never_trips() {
        let g = Governor::new(Budget {
            max_turns: None,
            max_tokens: None,
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        });
        assert_eq!(g.check(), None);
    }
}
