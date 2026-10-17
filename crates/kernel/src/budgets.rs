//! Hard turn, token, cost, and wall-clock ceilings. Exhaustion stops between
//! tool calls rather than killing one in flight.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::types::{Pricing, Usage};

/// Generous anti-runaway backstop on turns when the user sets nothing.
pub const DEFAULT_MAX_TURNS: u32 = 200;

/// Spend pooled across every session in one agent tree. Turns stay per-session,
/// but tokens, cost and wall-clock are the user's — a per-child copy of the
/// ceiling is no ceiling at all.
pub type Allowance = Arc<Mutex<Pooled>>;

/// A capped request's output allowance: what both the model and the budget allow,
/// but never less than one ordinary reply. When the budget cannot pay for that,
/// the request is refused and the task ends with its wrap-up instead.
pub(crate) fn capped_output(model: Option<u64>, room: Option<u64>) -> u64 {
    // A model whose own limit is unknown is not asked for more than this.
    const UNKNOWN_MODEL: u64 = 32_768;
    const ONE_REPLY: u64 = 4_096;
    let model = model.unwrap_or(UNKNOWN_MODEL);
    let floor = ONE_REPLY.min(model);
    room.map_or(model, |room| room.min(model)).max(floor)
}

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
    finish_claimed: bool,
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
    /// context-length error.
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
    finish_reserve: bool,
    finishing: bool,
}

#[derive(Debug, Default)]
struct LocalSpend {
    tokens: u64,
    cost_usd: f64,
}

/// An atomic worst-case reservation for one provider request. Dropping it
/// unreconciled charges the full amount — a lost connection is uncertain spend,
/// never free spend.
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
            finish_reserve: false,
            finishing: false,
        }
    }

    pub fn reserve_for_finish(mut self) -> Self {
        self.finish_reserve = true;
        self
    }

    pub fn capped_requests(&self) -> bool {
        self.budget.max_cost_usd.is_some() || self.budget.max_tokens.is_some()
    }

    pub fn requires_pricing(&self) -> bool {
        self.budget.max_cost_usd.is_some()
    }

    /// The share of the caps working requests may use; the rest funds the finish.
    fn working_share(&self, spent: &Pooled) -> f64 {
        let started = spent.tokens > 0
            || spent.reserved_tokens > 0
            || spent.cost_usd > 0.0
            || spent.reserved_cost_usd > 0.0;
        match self.finish_reserve && !self.finishing && started {
            true => 0.95,
            false => 1.0,
        }
    }

    /// Output tokens the caps can still pay for after `prompt`; `None` when no cap limits it.
    pub fn output_room(&self, prompt: u64, price: Option<Pricing>) -> Option<u64> {
        let spent = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let share = self.working_share(&spent);
        let tokens = self.budget.max_tokens.map(|max| {
            let used = spent.tokens + spent.reserved_tokens + prompt;
            ((max as f64 * share).floor() as u64).saturating_sub(used)
        });
        let paid = self
            .budget
            .max_cost_usd
            .zip(price)
            .and_then(|(max, price)| {
                let input = prompt as f64 * price.input_per_mtok / 1_000_000.0;
                let left = max * share - spent.cost_usd - spent.reserved_cost_usd - input;
                (price.output_per_mtok > 0.0)
                    .then(|| (left.max(0.0) * 1_000_000.0 / price.output_per_mtok).floor() as u64)
            });
        match (tokens, paid) {
            (Some(tokens), Some(paid)) => Some(tokens.min(paid)),
            (room, None) | (None, room) => room,
        }
    }

    pub fn begin_wrap_up(&mut self, prompt: u64, price: Option<Pricing>) -> Option<u64> {
        let mut allowance = self
            .allowance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if allowance.finish_claimed || !self.capped_requests() {
            return None;
        }
        let mut cap = 512;
        if let Some(max) = self.budget.max_tokens {
            cap = cap.min(
                max.saturating_sub(allowance.tokens.saturating_add(allowance.reserved_tokens))
                    .saturating_sub(prompt),
            );
        }
        if let Some(max) = self.budget.max_cost_usd {
            let price = price?;
            let remaining = max
                - allowance.cost_usd
                - allowance.reserved_cost_usd
                - prompt as f64 * price.input_per_mtok / 1_000_000.0;
            if !remaining.is_finite() || remaining < 0.0 {
                return None;
            }
            if price.output_per_mtok > 0.0 {
                cap = cap.min((remaining * 1_000_000.0 / price.output_per_mtok).floor() as u64);
            }
        }
        if cap == 0 {
            return None;
        }
        allowance.finish_claimed = true;
        self.finishing = true;
        Some(cap)
    }

    /// `Some(reason)` if any ceiling is reached — stop before the turn. Turns are
    /// this session's; everything else measures against the pool when there is one.
    pub fn check(&self) -> Option<BudgetStop> {
        if self
            .budget
            .max_cost_usd
            .is_some_and(|n| !n.is_finite() || n < 0.0)
        {
            return Some(BudgetStop::Cost);
        }
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
    /// Admission uses prepared-input accounting and a finite output cap.
    /// Local estimates are reconciled against the provider's actual usage.
    pub fn reserve_model(
        &mut self,
        prompt_tokens: Option<u64>,
        output_tokens: Option<u64>,
        pricing: Option<Pricing>,
    ) -> Result<ModelReservation, BudgetStop> {
        if self
            .budget
            .max_cost_usd
            .is_some_and(|n| !n.is_finite() || n < 0.0)
        {
            return Err(BudgetStop::Cost);
        }
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
        let working_share = self.working_share(&allowance);
        if matches!(
            self.budget.max_tokens,
            Some(max)
                if allowance
                    .tokens
                    .saturating_add(allowance.reserved_tokens)
                    .saturating_add(tokens)
                    > (max as f64 * working_share).floor() as u64
        ) {
            return Err(BudgetStop::Tokens);
        }
        if matches!(
            self.budget.max_cost_usd,
            Some(max)
                if allowance.cost_usd + allowance.reserved_cost_usd + cost_usd
                    > max * working_share
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
    /// Reconcile with authoritative usage. Missing or all-zero usage charges
    /// the reserved worst-case amount (the provider call may have run even if
    /// metering was absent, broken, or the transport failed).
    pub fn reconcile(
        mut self,
        usage: Option<Usage>,
        pricing: Option<Pricing>,
    ) -> Result<(), BudgetStop> {
        let usage = usage.filter(|usage| {
            usage.prompt_tokens != 0 || usage.completion_tokens != 0 || usage.total_tokens != 0
        });
        let (tokens, cost_usd) = usage.map_or((self.tokens, self.cost_usd), |usage| {
            (
                u64::from(
                    usage
                        .total_tokens
                        .max(usage.prompt_tokens.saturating_add(usage.completion_tokens)),
                ),
                pricing
                    .map(|price| price.cost(&usage))
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

    /// A capped request may write as much as the model allows while money
    /// remains, and never more than the rest of the budget can pay for.
    #[test]
    fn a_capped_reply_is_as_long_as_the_budget_can_pay_for() {
        let price = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 10.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            indicative: false,
        };
        let governor = Governor::new(Budget {
            max_cost_usd: Some(5.0),
            ..Budget::default()
        });
        // $5 less $0.01 of prompt pays for 499,000 output tokens: the model's limit decides.
        let room = governor.output_room(10_000, Some(price));
        assert_eq!(room, Some(499_000));
        assert_eq!(capped_output(Some(64_000), room), 64_000);
        assert_eq!(capped_output(None, room), 32_768);
        // Nearly spent: no tiny working reply is asked for. A full one is, which the
        // budget refuses, so the task ends with its wrap-up instead.
        let nearly = Governor::new(Budget {
            max_cost_usd: Some(0.02),
            ..Budget::default()
        });
        let room = nearly.output_room(10_000, Some(price));
        assert_eq!(room, Some(1_000));
        assert_eq!(capped_output(Some(64_000), room), 4_096);
        assert_eq!(capped_output(Some(2_000), room), 2_000);
        let tokens = Governor::new(Budget {
            max_tokens: Some(20_000),
            ..Budget::default()
        });
        assert_eq!(tokens.output_room(15_000, None), Some(5_000));
    }

    #[test]
    fn invalid_cost_limits_never_admit_spend() {
        for limit in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut governor = Governor::new(Budget {
                max_cost_usd: Some(limit),
                ..Budget::default()
            });
            assert_eq!(governor.check(), Some(BudgetStop::Cost));
            assert!(matches!(
                governor.reserve_model(
                    Some(1),
                    Some(1),
                    Some(Pricing {
                        input_per_mtok: 1.0,
                        output_per_mtok: 1.0,
                        cached_input_per_mtok: None,
                        cache_write_per_mtok: None,
                        indicative: false,
                    })
                ),
                Err(BudgetStop::Cost)
            ));
        }
    }

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
        let p = crate::types::Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            indicative: false,
        };
        let per_turn = p.cost(&Usage {
            prompt_tokens: 100_000,
            completion_tokens: 20_000,
            total_tokens: 120_000,
            cached_prompt_tokens: None,
            cache_write_tokens: None,
        }); // 0.3 + 0.3 = $0.60
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
    fn zeroed_usage_commits_the_worst_case_reservation() {
        let pricing = Pricing {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            indicative: false,
        };
        let mut governor = Governor::new(Budget {
            max_turns: None,
            max_tokens: Some(500),
            max_cost_usd: Some(0.002),
            max_wall_s: None,
            pooled: None,
        });
        let reservation = governor
            .reserve_model(Some(400), Some(100), Some(pricing))
            .expect("the worst-case request fits exactly");
        reservation
            .reconcile(
                Some(Usage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cached_prompt_tokens: None,
                    cache_write_tokens: None,
                }),
                Some(pricing),
            )
            .unwrap();

        assert_eq!(governor.tokens(), 500);
        assert!((governor.cost_usd() - 0.001_8).abs() < f64::EPSILON);
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
                cached_prompt_tokens: None,
                cache_write_tokens: None,
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
