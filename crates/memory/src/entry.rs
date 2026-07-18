//! Typed memory entry (design §3.1) — the payload of an `EventKind::MemoryWrite`
//! event and the row shape the projection stores/queries.

use kernel::TrustLabel;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Preference,
    Project,
    Feedback,
    Reference,
    Decision,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Preference => "preference",
            MemoryKind::Project => "project",
            MemoryKind::Feedback => "feedback",
            MemoryKind::Reference => "reference",
            MemoryKind::Decision => "decision",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "preference" => MemoryKind::Preference,
            "project" => MemoryKind::Project,
            "feedback" => MemoryKind::Feedback,
            "reference" => MemoryKind::Reference,
            "decision" => MemoryKind::Decision,
            _ => return None,
        })
    }
}

/// Which store an entry lives in (D9): project entries travel with the
/// workspace, user entries follow the person across projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "project" => Scope::Project,
            "user" => Scope::User,
            _ => return None,
        })
    }
}

/// Coarse, auditable confidence ladder (D6) — not a float; a float invites
/// fake precision no one can justify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceRung {
    Candidate,
    Confirmed,
    UserStated,
}

impl ConfidenceRung {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfidenceRung::Candidate => "candidate",
            ConfidenceRung::Confirmed => "confirmed",
            ConfidenceRung::UserStated => "user_stated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "candidate" => ConfidenceRung::Candidate,
            "confirmed" => ConfidenceRung::Confirmed,
            "user_stated" => ConfidenceRung::UserStated,
            _ => return None,
        })
    }
}

/// One typed memory (design §3.1). `trust`, `confidence`, and `provenance` are
/// kernel-computed at dispatch (D6) — this struct carries them, it doesn't
/// decide them; nothing here should be treated as a trusted tool argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub name: String,
    pub claim: String,
    pub description: String,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub trust: TrustLabel,
    pub confidence: ConfidenceRung,
    pub provenance: Vec<Ulid>,
    pub version: u32,
    pub pinned: bool,
    pub links: Vec<String>,
    pub created: f64,
    pub updated: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemoryEntry {
        MemoryEntry {
            name: "no-coauthored-by".into(),
            claim: "Omit the AI-attribution trailer from git commit messages.".into(),
            description: "user doesn't want Co-Authored-By in commits".into(),
            kind: MemoryKind::Feedback,
            scope: Scope::User,
            trust: TrustLabel::User,
            confidence: ConfidenceRung::UserStated,
            provenance: vec![Ulid::new(), Ulid::new()],
            version: 1,
            pinned: false,
            links: vec!["no-competitor-names-in-code".into()],
            created: 1000.0,
            updated: 1000.0,
        }
    }

    #[test]
    fn entry_serde_round_trips() {
        let e = sample();
        let json = serde_json::to_value(&e).unwrap();
        let back: MemoryEntry = serde_json::from_value(json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn kind_scope_confidence_str_round_trip() {
        for k in [
            MemoryKind::Preference,
            MemoryKind::Project,
            MemoryKind::Feedback,
            MemoryKind::Reference,
            MemoryKind::Decision,
        ] {
            assert_eq!(MemoryKind::parse(k.as_str()), Some(k));
        }
        for s in [Scope::Project, Scope::User] {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        for c in [ConfidenceRung::Candidate, ConfidenceRung::Confirmed, ConfidenceRung::UserStated] {
            assert_eq!(ConfidenceRung::parse(c.as_str()), Some(c));
        }
    }
}
