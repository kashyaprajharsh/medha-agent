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

pub fn apply_budget_env(mut b: kernel::Budget) -> Result<kernel::Budget> {
    if let Some(t) = env_number("MEDHA_MAX_TURNS")? {
        b.max_turns = Some(t);
    }
    if let Some(t) = env_number("MEDHA_MAX_TOKENS")? {
        b.max_tokens = Some(t);
    }
    if let Some(c) = env_number::<f64>("MEDHA_MAX_COST")? {
        anyhow::ensure!(
            c.is_finite() && c >= 0.0,
            "MEDHA_MAX_COST must be finite and non-negative"
        );
        b.max_cost_usd = Some(c);
    }
    if let Some(w) = env_number("MEDHA_MAX_WALL")? {
        b.max_wall_s = Some(w);
    }
    Ok(b)
}
