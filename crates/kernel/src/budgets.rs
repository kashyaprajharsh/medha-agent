//! Budget governor (§4.1, §18.5). The kernel enforces hard per-task ceilings —
//! turns, tokens, cost, wall-clock — as a contract, checked before each turn.
//! Each dimension is optional (`None` = unbounded on that axis). Exhaustion
//! ends the session gracefully and reports which limit was hit (never a
//! mid-tool kill, P10). This is the principled replacement for a hardcoded
//! turn cap: the *user* sets the ceiling; turn-count is just one dimension.

use std::time::Instant;

/// Generous anti-runaway backstop on turns when the user sets nothing.
pub const DEFAULT_MAX_TURNS: u32 = 200;

#[derive(Debug, Clone)]
pub struct Budget {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
}

impl Default for Budget {
    fn default() -> Self {
        // Backstop on turns; cost/tokens/wall are opt-in (set per task).
        Self {
            max_turns: Some(DEFAULT_MAX_TURNS),
            max_tokens: None,
            max_cost_usd: None,
            max_wall_s: None,
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
    /// context-length error (§4.3 emergency_ratio, the Hermes-style second
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
    pub fn check(&self) -> Option<BudgetStop> {
        if matches!(self.budget.max_turns, Some(m) if self.turns >= m) {
            return Some(BudgetStop::Turns);
        }
        if matches!(self.budget.max_tokens, Some(m) if self.tokens >= m) {
            return Some(BudgetStop::Tokens);
        }
        if matches!(self.budget.max_cost_usd, Some(m) if self.cost_usd >= m) {
            return Some(BudgetStop::Cost);
        }
        if matches!(self.budget.max_wall_s, Some(m) if self.start.elapsed().as_secs() >= m) {
            return Some(BudgetStop::Wall);
        }
        None
    }

    pub fn record_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    /// Record real token spend (and cost, when a price is known; 0.0 otherwise).
    pub fn record_tokens(&mut self, total_tokens: u64, cost_usd: f64) {
        self.tokens = self.tokens.saturating_add(total_tokens);
        self.cost_usd += cost_usd;
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
        });
        g.record_tokens(120_000, per_turn);
        assert_eq!(g.check(), None);
        g.record_tokens(120_000, per_turn);
        assert_eq!(g.check(), Some(BudgetStop::Cost));
    }

    #[test]
    fn unbounded_never_trips() {
        let g = Governor::new(Budget {
            max_turns: None,
            max_tokens: None,
            max_cost_usd: None,
            max_wall_s: None,
        });
        assert_eq!(g.check(), None);
    }
}
