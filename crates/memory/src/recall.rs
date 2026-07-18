//! Frozen K3 recall index (D2/D10).

use crate::{ConfidenceRung, MemoryEntry, MemoryError, MemoryProjection, Scope};
use context::{BpeCounter, TokenCounter};

/// Generous by default so the whole working set stays *in the prompt* (recall
/// is injection, not a model-initiated search) — overflow to `memory.search`
/// should be the exception, not the norm. Still tiny against a modern window.
pub const DEFAULT_K3_BUDGET_TOKENS: u32 = 3_000;
pub const DEFAULT_STALE_AFTER_DAYS: u32 = 30;
pub const MEMORY_MARKER: &str = "## Memory";

fn age_days(entry: &MemoryEntry, now: f64) -> u32 {
    ((now - entry.updated).max(0.0) / 86_400.0).floor() as u32
}

pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rung(entry: &MemoryEntry) -> &'static str {
    match entry.confidence {
        ConfidenceRung::Candidate => "candidate",
        ConfidenceRung::Confirmed => "confirmed",
        ConfidenceRung::UserStated => "user-stated",
    }
}

fn line(entry: &MemoryEntry, now: f64, description: &str, stale_after_days: u32) -> String {
    let age = age_days(entry, now);
    let pin = if entry.pinned { " · pinned" } else { "" };
    let stale = if age > stale_after_days {
        " ⚠ verify before asserting"
    } else {
        ""
    };
    format!(
        "• [{} · {}{}] {} — {} ({}d{})",
        rung(entry),
        entry.trust.as_str(),
        pin,
        entry.name,
        description,
        age,
        stale,
    )
}

pub(crate) fn index_eligible(entry: &MemoryEntry, now: f64, stale_after_days: u32) -> bool {
    entry.pinned
        || entry.confidence != ConfidenceRung::Candidate
        || age_days(entry, now) <= stale_after_days
}

pub(crate) fn entry_index_tokens(
    entry: &MemoryEntry,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> u32 {
    counter.count(&line(
        entry,
        now,
        &one_line(&entry.description),
        stale_after_days,
    ))
}

fn render(
    selected: &[(MemoryEntry, String)],
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> String {
    let project = selected.iter().filter(|(e, _)| e.scope == Scope::Project).count();
    let user = selected.len() - project;
    let mut usage = 0;
    let mut block = String::new();
    for _ in 0..8 {
        let mut next = format!(
            "── MEMORY ({} entries · {usage}/{budget_tokens} tok · project:{project} user:{user}) ──",
            selected.len()
        );
        for (entry, description) in selected {
            next.push('\n');
            next.push_str(&line(entry, now, description, stale_after_days));
        }
        next.push_str("\n…full entries: memory.search · past sessions: sessions.search");
        let counted = counter.count(&next);
        block = next;
        if counted == usage {
            break;
        }
        usage = counted;
    }
    block
}

pub(crate) fn full_index_tokens(
    entries: &[MemoryEntry],
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> u32 {
    let selected = entries
        .iter()
        .cloned()
        .map(|entry| {
            let description = one_line(&entry.description);
            (entry, description)
        })
        .collect::<Vec<_>>();
    counter.count(&render(
        &selected,
        budget_tokens,
        now,
        stale_after_days,
        counter,
    ))
}

fn fit_pinned_description(
    selected: &[(MemoryEntry, String)],
    entry: &MemoryEntry,
    description: &str,
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> Option<String> {
    let chars: Vec<char> = description.chars().collect();
    let mut lo = 0;
    let mut hi = chars.len();
    let mut best = None;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let mut clipped: String = chars[..mid].iter().collect();
        if mid < chars.len() {
            clipped.push('…');
        }
        let mut trial = selected.to_vec();
        trial.push((entry.clone(), clipped.clone()));
        if counter.count(&render(
            &trial,
            budget_tokens,
            now,
            stale_after_days,
            counter,
        )) <= budget_tokens
        {
            best = Some(clipped);
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

fn compile_with_counter(
    store: &MemoryProjection,
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
    counter: &dyn TokenCounter,
) -> Result<String, MemoryError> {
    if budget_tokens == 0 {
        return Ok(String::new());
    }
    let mut entries = store.list()?;
    entries.retain(|entry| index_eligible(entry, now, stale_after_days));
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.trust.rank().cmp(&a.trust.rank()))
            .then_with(|| b.updated.total_cmp(&a.updated))
            .then_with(|| a.name.cmp(&b.name))
    });

    let empty = render(&[], budget_tokens, now, stale_after_days, counter);
    if counter.count(&empty) > budget_tokens {
        return Ok(String::new());
    }

    let mut selected: Vec<(MemoryEntry, String)> = Vec::new();
    for entry in entries {
        let description = one_line(&entry.description);
        let mut trial = selected.clone();
        trial.push((entry.clone(), description.clone()));
        if counter.count(&render(
            &trial,
            budget_tokens,
            now,
            stale_after_days,
            counter,
        )) <= budget_tokens
        {
            selected = trial;
        } else if entry.pinned {
            if let Some(clipped) = fit_pinned_description(
                &selected,
                &entry,
                &description,
                budget_tokens,
                now,
                stale_after_days,
                counter,
            ) {
                selected.push((entry, clipped));
            }
        }
    }
    Ok(render(
        &selected,
        budget_tokens,
        now,
        stale_after_days,
        counter,
    ))
}

/// Compile the deterministic K3 snapshot. `now` is injected so replay and
/// tests never depend on wall-clock time.
pub fn compile_k3(
    store: &MemoryProjection,
    budget_tokens: u32,
    now: f64,
) -> Result<String, MemoryError> {
    compile_k3_configured(
        store,
        budget_tokens,
        now,
        DEFAULT_STALE_AFTER_DAYS,
    )
}

pub fn compile_k3_configured(
    store: &MemoryProjection,
    budget_tokens: u32,
    now: f64,
    stale_after_days: u32,
) -> Result<String, MemoryError> {
    compile_with_counter(
        store,
        budget_tokens,
        now,
        stale_after_days,
        &BpeCounter::o200k(),
    )
}

/// Replace the trailing K3 section while preserving the stable system-prefix.
pub fn replace_k3(system: &str, block: &str) -> String {
    let head = system
        .find(MEMORY_MARKER)
        .map(|idx| system[..idx].trim_end())
        .unwrap_or_else(|| system.trim_end());
    if block.is_empty() {
        return head.to_string();
    }
    format!("{head}\n\n{MEMORY_MARKER}\n\n{block}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryKind, MemoryOp};
    use context::HeuristicCounter;
    use kernel::TrustLabel;
    use ulid::Ulid;

    fn store(tag: &str) -> MemoryProjection {
        let dir = std::env::temp_dir().join(format!("medha-recall-{tag}-{}", Ulid::new()));
        MemoryProjection::open(dir.join("project.db"), dir.join("user.db")).unwrap()
    }

    fn entry(
        name: &str,
        scope: Scope,
        trust: TrustLabel,
        confidence: ConfidenceRung,
        updated: f64,
    ) -> MemoryEntry {
        MemoryEntry {
            name: name.into(),
            claim: format!("claim for '{name}'"),
            description: format!("hook with quotes, hyphens, and details for {name}"),
            kind: MemoryKind::Project,
            scope,
            trust,
            confidence,
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
    fn compiles_ranked_budgeted_index_and_demotes_stale_candidate() {
        let store = store("rank");
        let now = 40.0 * 86_400.0;
        let stale = entry("old-candidate", Scope::Project, TrustLabel::User, ConfidenceRung::Candidate, 0.0);
        store.apply(&MemoryOp::Write { entry: stale }).unwrap();
        let recent = entry("recent-user", Scope::User, TrustLabel::User, ConfidenceRung::UserStated, now - 86_400.0);
        store.apply(&MemoryOp::Write { entry: recent }).unwrap();
        let mut pinned = entry("pinned-web", Scope::Project, TrustLabel::Web, ConfidenceRung::Candidate, 0.0);
        pinned.pinned = true;
        store.apply(&MemoryOp::Write { entry: pinned }).unwrap();

        let counter = HeuristicCounter;
        let block = compile_with_counter(
            &store,
            120,
            now,
            DEFAULT_STALE_AFTER_DAYS,
            &counter,
        )
        .unwrap();
        assert!(counter.count(&block) <= 120);
        assert!(block.find("pinned-web").unwrap() < block.find("recent-user").unwrap());
        assert!(!block.contains("old-candidate"));
        assert!(block.contains("⚠ verify before asserting"));
    }

    #[test]
    fn frozen_block_is_byte_stable_until_explicit_refresh() {
        let store = store("frozen");
        let now = 10_000.0;
        store
            .apply(&MemoryOp::Write {
                entry: entry("session-a", Scope::Project, TrustLabel::User, ConfidenceRung::UserStated, now),
            })
            .unwrap();
        let frozen = compile_k3(&store, 1_200, now).unwrap();
        let system = replace_k3("persona", &frozen);

        store
            .apply(&MemoryOp::Write {
                entry: entry("mid-session", Scope::Project, TrustLabel::User, ConfidenceRung::UserStated, now),
            })
            .unwrap();
        assert_eq!(system, replace_k3("persona", &frozen));
        assert!(!system.contains("mid-session"));

        let refreshed = replace_k3(&system, &compile_k3(&store, 1_200, now).unwrap());
        assert!(refreshed.contains("session-a"));
        assert!(refreshed.contains("mid-session"));
        assert_eq!(refreshed.matches(MEMORY_MARKER).count(), 1);
    }

    #[test]
    fn fresh_projection_session_recalls_prior_write() {
        let dir = std::env::temp_dir().join(format!("medha-recall-cross-session-{}", Ulid::new()));
        let project = dir.join("project.db");
        let user = dir.join("user.db");
        let now = 20_000.0;
        {
            let session_a = MemoryProjection::open(&project, &user).unwrap();
            session_a
                .apply(&MemoryOp::Write {
                    entry: entry("quoted-fact", Scope::Project, TrustLabel::User, ConfidenceRung::UserStated, now),
                })
                .unwrap();
        }
        let session_b = MemoryProjection::open(&project, &user).unwrap();
        let block = compile_k3(&session_b, 1_200, now).unwrap();
        assert!(block.contains("quoted-fact"));
    }

    #[test]
    fn empty_and_replaced_blocks_are_well_formed() {
        assert_eq!(replace_k3("persona", ""), "persona");
        let once = replace_k3("persona", "one");
        let twice = replace_k3(&once, "two");
        assert_eq!(twice, "persona\n\n## Memory\n\ntwo");
    }

    #[test]
    fn configured_staleness_changes_candidate_eligibility() {
        let store = store("configured-stale");
        let now = 40.0 * 86_400.0;
        store
            .apply(&MemoryOp::Write {
                entry: entry(
                    "old-but-allowed",
                    Scope::Project,
                    TrustLabel::User,
                    ConfidenceRung::Candidate,
                    0.0,
                ),
            })
            .unwrap();
        let block = compile_k3_configured(&store, 1_200, now, 90).unwrap();
        assert!(block.contains("old-but-allowed"));
        assert!(!block.contains("⚠ verify before asserting"));
    }
}
