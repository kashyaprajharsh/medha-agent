//! Prompt registry (§4.11, §6). Prompts are content, not logic — versioned,
//! diffable, evolvable, eval-gatable artifacts, never code literals.
//!
//! **Embed-with-override.** Each prompt is authored as a `.md` file under
//! `prompts/` and embedded at compile time (`include_str!`) so the single
//! static binary always has a working default. At runtime a deployment may
//! override any prompt by id without recompiling; resolution order:
//!
//!   1. `$MEDHA_PROMPTS_DIR/<id>.md`         (explicit override dir)
//!   2. `./.medha/prompts/<id>.md`           (project scope)
//!   3. embedded default                      (shipped in the binary)
//!
//! A future step keys overrides by content hash via `medha.lock` so a promoted
//! prompt version is reproducible and rollback-able like any other artifact.

use std::path::PathBuf;

/// Stable prompt ids. Use these constants, not string literals at call sites.
pub const COMPACTION_SUMMARY: &str = "compaction_summary";
/// The K1 identity / operating brief that becomes the system prompt.
pub const SYSTEM_IDENTITY: &str = "system";

const EMBEDDED_COMPACTION_SUMMARY: &str = include_str!("../prompts/compaction_summary.md");
const EMBEDDED_SYSTEM_IDENTITY: &str = include_str!("../prompts/system.md");

fn embedded(id: &str) -> Option<&'static str> {
    match id {
        COMPACTION_SUMMARY => Some(EMBEDDED_COMPACTION_SUMMARY),
        SYSTEM_IDENTITY => Some(EMBEDDED_SYSTEM_IDENTITY),
        _ => None,
    }
}

/// Resolve a prompt by id: runtime override file → embedded default.
/// Returns `None` only for an unknown id (a programming error, since ids are
/// constants in this module).
pub fn get(id: &str) -> Option<String> {
    for dir in override_dirs() {
        let path = dir.join(format!("{id}.md"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some(text);
        }
    }
    embedded(id).map(str::to_owned)
}

/// Convenience accessor for the compaction summary prompt; always resolves
/// (embedded default guarantees a value).
pub fn compaction_summary() -> String {
    get(COMPACTION_SUMMARY).expect("compaction_summary prompt is embedded")
}

/// The system identity / operating brief; always resolves (embedded default).
/// Honors the same runtime override chain, so a deployment can swap the agent's
/// persona by dropping a `system.md` in its prompts dir — no recompile.
pub fn system_identity() -> String {
    get(SYSTEM_IDENTITY).expect("system prompt is embedded")
}

fn override_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("MEDHA_PROMPTS_DIR") {
        if !d.is_empty() {
            dirs.push(PathBuf::from(d));
        }
    }
    dirs.push(PathBuf::from(".medha/prompts"));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_resolves_and_has_sections() {
        let p = compaction_summary();
        assert!(p.contains("Goal"));
        assert!(p.contains("Next steps"));
        assert!(p.contains("User instructions"));
        assert!(p.contains("verbatim"));
    }

    #[test]
    fn system_prompt_makes_workspace_audits_an_explicit_delegation_case() {
        let prompt = embedded(SYSTEM_IDENTITY).expect("system prompt is embedded");
        assert!(prompt.contains("whole-workspace review or audit"));
        assert!(prompt.contains("parallel `agent.spawn` `tasks`"));
        assert!(!prompt.contains("use `background`"));
    }

    #[test]
    fn unknown_id_is_none() {
        assert!(get("no_such_prompt").is_none());
    }
}
