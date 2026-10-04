//! Per-task ceilings an operator set in the environment.

use anyhow::Result;

pub fn env_number<T: std::str::FromStr>(name: &str) -> Result<Option<T>> {
    match std::env::var(name) {
        Ok(value) => value
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("{name} must be a valid numeric limit")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} must contain valid Unicode"),
    }
}

/// Ceilings the caller set for one chat; each replaces the lock file's value.
#[derive(Clone, Copy, Debug, Default)]
pub struct BudgetLimits {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_s: Option<u64>,
}

impl BudgetLimits {
    pub fn from_env() -> Result<Self> {
        let max_turns = env_number("MEDHA_MAX_TURNS")?;
        let max_tokens = env_number("MEDHA_MAX_TOKENS")?;
        let max_cost_usd = env_number::<f64>("MEDHA_MAX_COST")?;
        if let Some(c) = max_cost_usd {
            anyhow::ensure!(
                c.is_finite() && c >= 0.0,
                "MEDHA_MAX_COST must be finite and non-negative"
            );
        }
        let max_wall_s = env_number("MEDHA_MAX_WALL")?;
        Ok(Self {
            max_turns,
            max_tokens,
            max_cost_usd,
            max_wall_s,
        })
    }

    pub fn apply(&self, mut b: kernel::Budget) -> kernel::Budget {
        if let Some(t) = self.max_turns {
            b.max_turns = Some(t);
        }
        if let Some(t) = self.max_tokens {
            b.max_tokens = Some(t);
        }
        if let Some(c) = self.max_cost_usd {
            b.max_cost_usd = Some(c);
        }
        if let Some(w) = self.max_wall_s {
            b.max_wall_s = Some(w);
        }
        b
    }
}
