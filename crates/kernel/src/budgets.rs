//! Budget governor (§4.1, §18.5). The kernel enforces hard per-task ceilings —
//! turns, tokens, cost, wall-clock — as a contract, checked before each turn.
//! Each dimension is optional (`None` = unbounded on that axis). Exhaustion
//! ends the session gracefully and reports which limit was hit (never a
//! mid-tool kill, P10). This is the principled replacement for a hardcoded
//! turn cap: the *user* sets the ceiling; turn-count is just one dimension.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::types::{Pricing, Usage};

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
    reserved_tokens: u64,
    reserved_cost_usd: f64,
    started: Option<Instant>,
}

impl Pooled {
    pub fn new() -> Allowance {
        Arc::new(Mutex::new(Self::default()))
    }

    fn started(&mut self) -> Instant {
        *self.started.get_or_insert_with(Instant::now)
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
    allowance: Allowance,
    local: Arc<Mutex<LocalSpend>>,
    turns: u32,
}

#[derive(Debug, Default)]
struct LocalSpend {
    tokens: u64,
    cost_usd: f64,
}

/// An atomic worst-case reservation for one provider request.
///
/// Dropping an unreconciled reservation charges its full amount. Once a
/// request has been admitted, losing the connection or receiving no usage
/// block is uncertain spend, never free spend.
pub struct ModelReservation {
    allowance: Allowance,
    local: Arc<Mutex<LocalSpend>>,
    tokens: u64,
    cost_usd: f64,
    enforce_tokens: bool,
    enforce_cost: bool,
    settled: bool,
}

impl Governor {
    pub fn new(budget: Budget) -> Self {
        let allowance = budget.pooled.clone().unwrap_or_default();
        // Starting a descendant must not reset the tree's wall clock.
        allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .started();
        Self {
            budget,
            allowance,
            local: Arc::new(Mutex::new(LocalSpend::default())),
            turns: 0,
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
        let (tokens, cost_usd, elapsed) = self.spent();
        if matches!(self.budget.max_tokens, Some(m) if tokens >= m) {
            return Some(BudgetStop::Tokens);
        }
        if matches!(self.budget.max_cost_usd, Some(m) if cost_usd >= m) {
            return Some(BudgetStop::Cost);
        }
        if matches!(self.budget.max_wall_s, Some(m) if elapsed >= Duration::from_secs(m)) {
            return Some(BudgetStop::Wall);
        }
        None
    }

    /// What counts against the ceilings: the pool's totals where one is shared,
    /// this session's own otherwise. A poisoned pool falls back to local rather
    /// than removing the ceiling.
    fn spent(&self) -> (u64, f64, Duration) {
        let mut allowance = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            allowance.tokens.saturating_add(allowance.reserved_tokens),
            allowance.cost_usd + allowance.reserved_cost_usd,
            allowance.started().elapsed(),
        )
    }

    pub fn record_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    /// Absolute task deadline, shared by descendants using the same pool.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        let wall = self.budget.max_wall_s?;
        let started = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .started();
        let deadline = started
            .checked_add(Duration::from_secs(wall))
            .unwrap_or(started);
        Some(tokio::time::Instant::from_std(deadline))
    }

    /// Atomically reserve the maximum charge of one provider request.
    ///
    /// A hard token/cost ceiling requires exact prepared-input accounting and a
    /// finite output cap. Otherwise there is no safe upper bound to admit.
    pub fn reserve_model(
        &mut self,
        prompt_tokens: Option<u64>,
        output_tokens: Option<u64>,
        pricing: Option<Pricing>,
    ) -> Result<ModelReservation, BudgetStop> {
        if self.budget.max_tokens.is_some() && (prompt_tokens.is_none() || output_tokens.is_none())
        {
            return Err(BudgetStop::Tokens);
        }
        if self.budget.max_cost_usd.is_some()
            && (prompt_tokens.is_none() || output_tokens.is_none() || pricing.is_none())
        {
            return Err(BudgetStop::Cost);
        }

        let prompt = prompt_tokens.unwrap_or(0);
        let output = output_tokens.unwrap_or(0);
        let tokens = prompt.saturating_add(output);
        let cost_usd = pricing
            .map(|price| {
                (prompt as f64 * price.input_per_mtok + output as f64 * price.output_per_mtok)
                    / 1_000_000.0
            })
            .unwrap_or(0.0);
        if !cost_usd.is_finite() || cost_usd.is_sign_negative() {
            return Err(BudgetStop::Cost);
        }

        let mut allowance = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            self.budget.max_tokens,
            Some(max)
                if allowance
                    .tokens
                    .saturating_add(allowance.reserved_tokens)
                    .saturating_add(tokens)
                    > max
        ) {
            return Err(BudgetStop::Tokens);
        }
        if matches!(
            self.budget.max_cost_usd,
            Some(max)
                if allowance.cost_usd + allowance.reserved_cost_usd + cost_usd
                    > max
        ) {
            return Err(BudgetStop::Cost);
        }
        allowance.reserved_tokens = allowance.reserved_tokens.saturating_add(tokens);
        allowance.reserved_cost_usd += cost_usd;
        drop(allowance);

        Ok(ModelReservation {
            allowance: Arc::clone(&self.allowance),
            local: Arc::clone(&self.local),
            tokens,
            cost_usd,
            enforce_tokens: self.budget.max_tokens.is_some(),
            enforce_cost: self.budget.max_cost_usd.is_some(),
            settled: false,
        })
    }

    /// Record real token spend outside request reservation (kept for callers
    /// which meter a known non-provider charge).
    pub fn record_tokens(&mut self, total_tokens: u64, cost_usd: f64) {
        let mut local = self
            .local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        local.tokens = local.tokens.saturating_add(total_tokens);
        local.cost_usd += cost_usd;
        drop(local);
        let mut allowance = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        allowance.tokens = allowance.tokens.saturating_add(total_tokens);
        allowance.cost_usd += cost_usd;
    }

    pub fn turns(&self) -> u32 {
        self.turns
    }
    pub fn tokens(&self) -> u64 {
        self.local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tokens
    }
    pub fn cost_usd(&self) -> f64 {
        self.local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cost_usd
    }
}

impl ModelReservation {
    /// Reconcile with authoritative usage. `None` charges the reserved
    /// worst-case amount (the provider call may have run even if metering was
    /// absent or the transport failed).
    pub fn reconcile(
        mut self,
        usage: Option<Usage>,
        pricing: Option<Pricing>,
    ) -> Result<(), BudgetStop> {
        let (tokens, cost_usd) = usage.map_or((self.tokens, self.cost_usd), |usage| {
            (
                u64::from(
                    usage
                        .total_tokens
                        .max(usage.prompt_tokens.saturating_add(usage.completion_tokens)),
                ),
                pricing
                    .map(|price| price.cost(usage.prompt_tokens, usage.completion_tokens))
                    .unwrap_or(self.cost_usd),
            )
        });
        let token_overrun = self.enforce_tokens && tokens > self.tokens;
        let cost_overrun = self.enforce_cost && cost_usd > self.cost_usd + f64::EPSILON;
        self.settle(tokens, cost_usd);
        if token_overrun {
            Err(BudgetStop::Tokens)
        } else if cost_overrun {
            Err(BudgetStop::Cost)
        } else {
            Ok(())
        }
    }

    fn settle(&mut self, tokens: u64, cost_usd: f64) {
        if self.settled {
            return;
        }
        let mut allowance = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        allowance.reserved_tokens = allowance.reserved_tokens.saturating_sub(self.tokens);
        allowance.reserved_cost_usd = (allowance.reserved_cost_usd - self.cost_usd).max(0.0);
        allowance.tokens = allowance.tokens.saturating_add(tokens);
        allowance.cost_usd += cost_usd;
        drop(allowance);

        let mut local = self
            .local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        local.tokens = local.tokens.saturating_add(tokens);
        local.cost_usd += cost_usd;
        self.settled = true;
    }
}

impl Drop for ModelReservation {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(self.tokens, self.cost_usd);
        }
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

    #[test]
    fn missing_usage_commits_the_worst_case_reservation() {
        let mut governor = Governor::new(Budget {
            max_turns: None,
            max_tokens: Some(500),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        });
        let reservation = governor
            .reserve_model(Some(400), Some(100), None)
            .expect("exactly the remaining allowance is admissible");
        reservation.reconcile(None, None).unwrap();
        assert_eq!(governor.tokens(), 500);
        assert_eq!(governor.check(), Some(BudgetStop::Tokens));
    }

    #[test]
    fn a_single_request_cannot_overshoot_at_admission() {
        let mut governor = Governor::new(Budget {
            max_turns: None,
            max_tokens: Some(100),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        });
        assert!(matches!(
            governor.reserve_model(Some(80), Some(21), None),
            Err(BudgetStop::Tokens)
        ));
        assert_eq!(governor.tokens(), 0);
    }

    #[test]
    fn provider_usage_above_the_reserved_wire_cap_is_reported() {
        let mut governor = Governor::new(Budget {
            max_turns: None,
            max_tokens: Some(1_000),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        });
        let reservation = governor.reserve_model(Some(40), Some(10), None).unwrap();
        let result = reservation.reconcile(
            Some(Usage {
                prompt_tokens: 40,
                completion_tokens: 20,
                total_tokens: 60,
            }),
            None,
        );
        assert_eq!(result, Err(BudgetStop::Tokens));
        assert_eq!(governor.tokens(), 60, "meter the provider's real report");
    }

    #[test]
    fn concurrent_reservations_share_one_atomic_ceiling() {
        let budget = Budget {
            max_turns: None,
            max_tokens: Some(100),
            max_cost_usd: None,
            max_wall_s: None,
            pooled: None,
        }
        .pooling();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let budget = budget.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut governor = Governor::new(budget);
                    barrier.wait();
                    let reservation = governor.reserve_model(Some(40), Some(20), None);
                    let admitted = reservation.is_ok();
                    // Keep an admitted reservation alive long enough for the
                    // competing thread to observe it.
                    barrier.wait();
                    drop(reservation);
                    admitted
                })
            })
            .collect::<Vec<_>>();
        let admitted = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation thread"))
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
    }

    #[test]
    fn a_cost_ceiling_without_pricing_fails_closed() {
        let mut governor = Governor::new(Budget {
            max_turns: None,
            max_tokens: None,
            max_cost_usd: Some(1.0),
            max_wall_s: None,
            pooled: None,
        });
        assert!(matches!(
            governor.reserve_model(Some(10), Some(10), None),
            Err(BudgetStop::Cost)
        ));
    }
}
