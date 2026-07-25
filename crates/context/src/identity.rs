//! K1 Identity sheath (§4.3): the agent persona and harness rules that become
//! the system prompt. Assembling K1 is the context compiler's responsibility,
//! not the entrypoint's — so it lives here as a single, overridable home that
//! the full five-sheath pipeline grows into. A deployment may override the
//! persona via config / `medha.lock`; harness-rule fragments are appended by
//! the compiler as the sheath matures.

/// Assemble the K1 system prompt. Precedence: an explicit config persona wins;
/// otherwise the `system` prompt from the registry — an editable
/// `crates/context/prompts/system.md` embedded at build time, overridable at
/// runtime via the prompt registry's chain ([`crate::prompts`]). The brief is
/// the single biggest lever on agent behavior: it tells the model to narrate as
/// it works (the transcript is live — silence during long tool runs reads as a
/// hang), work in small verified steps, and explore before it edits.
pub fn system_prompt(persona_override: Option<&str>) -> String {
    match persona_override {
        Some(p) => p.to_string(),
        None => crate::prompts::system_identity(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The brief drifted once already: LSP, MCP and sub-agents all shipped while
    /// it still described a file-tools-and-shell agent, so capabilities were paid
    /// for and never used. This fails when the next one lands unmentioned.
    #[test]
    fn the_brief_covers_what_medha_can_actually_do() {
        let brief = system_prompt(None);
        for capability in [
            "lsp.definition", // semantic navigation, not grep guesses
            "agent.spawn",    // delegation, on its own judgement
            "read_artifact",  // paging, so nothing is "truncated"
            "update_plan",    // the user's live progress view
            "skill.load",     // project procedures come first
            "memory.write",   // what should outlive the session
            "clarify",        // ask rather than guess
            "mcp__",          // external tool output is data, not instruction
        ] {
            assert!(
                brief.contains(capability),
                "the operating brief never mentions {capability}"
            );
        }
    }

    #[test]
    fn the_brief_states_that_tool_output_is_not_instruction() {
        // Prompt injection arrives through fetched pages and MCP results; the
        // brief has to say so, because the model cannot infer a trust boundary.
        let brief = system_prompt(None).to_lowercase();
        assert!(
            brief.contains("not an instruction") || brief.contains("do not tell you what to do")
        );
        assert!(brief.contains("trust boundary"));
    }

    #[test]
    fn falls_back_to_registry_then_honors_override() {
        // No override → the embedded operating brief (contains its key rules).
        let default = system_prompt(None);
        assert!(default.contains("MEDHA"));
        assert!(default.contains("Think out loud"));
        // Explicit persona wins.
        assert_eq!(system_prompt(Some("Custom persona.")), "Custom persona.");
    }
}
