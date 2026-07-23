//! Deterministic working-set pressure checks (D3/D10).

use crate::recall::{
    DEFAULT_STALE_AFTER_DAYS, entry_index_tokens, full_index_tokens, index_eligible, one_line,
};
use crate::{MemoryEntry, MemoryError, MemoryProjection};
use context::{BpeCounter, TokenCounter};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PressureEntry {
    pub name: String,
    pub preview: String,
    pub size_tokens: u32,
    pub age_days: u32,
    pub rung: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetAssessment {
    pub budget_tokens: u32,
    pub used_tokens: u32,
    pub projected_tokens: u32,
    pub deficit_tokens: u32,
    pub entries: Vec<PressureEntry>,
}

impl BudgetAssessment {
    pub fn over_budget(&self) -> bool {
        self.deficit_tokens > 0
    }
}

fn preview(text: &str) -> String {
    let line = one_line(text);
    let mut chars = line.chars();
    let head = chars.by_ref().take(80).collect::<String>();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn assess_with_counter(
    store: &MemoryProjection,
    incoming: &MemoryEntry,
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> Result<BudgetAssessment, MemoryError> {
    let mut current = store.list()?;
    current.retain(|entry| index_eligible(entry, now, stale_after_days));
    let used_tokens = full_index_tokens(&current, budget_tokens, now, stale_after_days, counter);
    let mut projected = current.clone();
    if index_eligible(incoming, now, stale_after_days) {
        projected.push(incoming.clone());
    }
    let projected_tokens =
        full_index_tokens(&projected, budget_tokens, now, stale_after_days, counter);
    let entries = current
        .into_iter()
        .map(|entry| PressureEntry {
            name: entry.name.clone(),
            preview: preview(&entry.description),
            size_tokens: entry_index_tokens(&entry, now, stale_after_days, counter),
            age_days: ((now - entry.updated).max(0.0) / 86_400.0).floor() as u32,
            rung: entry.confidence.as_str().to_string(),
        })
        .collect();
    Ok(BudgetAssessment {
        budget_tokens,
        used_tokens,
        projected_tokens,
        deficit_tokens: projected_tokens.saturating_sub(budget_tokens),
        entries,
    })
}

/// Assess whether a new index-eligible write fits the complete K3 working set.
pub fn assess_write(
    store: &MemoryProjection,
    incoming: &MemoryEntry,
    budget_tokens: u32,
    now: f64,
) -> Result<BudgetAssessment, MemoryError> {
    assess_write_configured(
        store,
        incoming,
        budget_tokens,
        now,
        DEFAULT_STALE_AFTER_DAYS,
    )
}

pub fn assess_write_configured(
    store: &MemoryProjection,
    incoming: &MemoryEntry,
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
) -> Result<BudgetAssessment, MemoryError> {
    assess_with_counter(
        store,
        incoming,
        budget_tokens,
        now,
        stale_after_days,
        &BpeCounter::o200k(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfidenceRung, MemoryKind, MemoryOp, Scope};
    use context::HeuristicCounter;
    use kernel::TrustLabel;
    use ulid::Ulid;

    fn entry(name: &str, description: &str, updated: f64) -> MemoryEntry {
        MemoryEntry {
            name: name.into(),
            claim: format!("claim '{name}'"),
            description: description.into(),
            kind: MemoryKind::Project,
            scope: Scope::Project,
            trust: TrustLabel::User,
            confidence: ConfidenceRung::UserStated,
            provenance: vec![Ulid::new()],
            sessions: vec![Ulid::new()],
            version: 1,
            pinned: false,
            links: vec![],
            created: updated,
            updated,
        }
    }

    #[test]
    fn assessment_lists_realistic_entries_and_exact_deficit() {
        let dir = std::env::temp_dir().join(format!("medha-consolidate-{}", Ulid::new()));
        let store = MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap();
        let old = entry(
            "quoted-hyphen",
            "A quoted 'preview' with a hyphen and enough text to consume the budget.",
            0.0,
        );
        store.apply(&MemoryOp::Write { entry: old }).unwrap();
        let incoming = entry("new-fact", "another substantial index hook", 86_400.0);
        let assessment = assess_with_counter(
            &store,
            &incoming,
            35,
            86_400.0,
            DEFAULT_STALE_AFTER_DAYS,
            &HeuristicCounter,
        )
        .unwrap();

        assert!(assessment.over_budget());
        assert_eq!(assessment.deficit_tokens, assessment.projected_tokens - 35);
        assert_eq!(assessment.entries[0].name, "quoted-hyphen");
        assert!(assessment.entries[0].preview.contains("'preview'"));
        assert_eq!(assessment.entries[0].age_days, 1);
        assert_eq!(assessment.entries[0].rung, "user_stated");
    }

    #[test]
    fn stale_candidate_is_not_budget_pressure() {
        let dir = std::env::temp_dir().join(format!("medha-consolidate-stale-{}", Ulid::new()));
        let store = MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap();
        let mut stale = entry("old-candidate", "old hook", 0.0);
        stale.confidence = ConfidenceRung::Candidate;
        store.apply(&MemoryOp::Write { entry: stale }).unwrap();
        let incoming = entry("fresh", "fresh hook", 40.0 * 86_400.0);
        let assessment = assess_with_counter(
            &store,
            &incoming,
            1_200,
            40.0 * 86_400.0,
            DEFAULT_STALE_AFTER_DAYS,
            &HeuristicCounter,
        )
        .unwrap();
        assert!(!assessment.over_budget());
        assert!(assessment.entries.is_empty());
    }

    #[test]
    fn public_assessment_handles_an_empty_hook() {
        let dir = std::env::temp_dir().join(format!("medha-consolidate-empty-{}", Ulid::new()));
        let store = MemoryProjection::open(dir.join("p.db"), dir.join("u.db")).unwrap();
        let incoming = entry("empty-hook", "", 1_000.0);
        let assessment = assess_write(&store, &incoming, 1_200, 1_000.0).unwrap();
        assert!(!assessment.over_budget());
        assert!(assessment.projected_tokens > 0);
        let configured = assess_write_configured(&store, &incoming, 1_200, 1_000.0, 7).unwrap();
        assert_eq!(configured.projected_tokens, assessment.projected_tokens);
    }
}
