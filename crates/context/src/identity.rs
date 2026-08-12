/// Return the explicit persona override or the registered system prompt.
pub fn system_prompt(persona_override: Option<&str>) -> String {
    match persona_override {
        Some(p) => p.to_string(),
        None => crate::prompts::system_identity(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_covers_what_medha_can_actually_do() {
        let brief = system_prompt(None);
        for capability in [
            "lsp.definition",
            "agent.spawn",
            "read_artifact",
            "update_plan",
            "skill.load",
            "memory.write",
            "clarify",
            "mcp__",
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
