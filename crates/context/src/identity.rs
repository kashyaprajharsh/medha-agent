/// Return the explicit persona override or the registered system prompt.
pub fn system_prompt(persona_override: Option<&str>) -> String {
    match persona_override {
        Some(p) => p.to_string(),
        None => crate::prompts::system_identity(),
    }
}

/// Explicit personas remain user-owned; the default operating brief follows
/// the final executor catalogue rather than the pre-narrowing registry.
pub fn system_prompt_for_tools(
    persona_override: Option<&str>,
    tools: &std::collections::HashSet<String>,
) -> String {
    match persona_override {
        Some(persona) => persona.to_owned(),
        None => crate::prompts::system_identity_for_tools(tools),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_covers_what_medha_can_actually_do() {
        let brief = system_prompt(None);
        // Names, not prose: a brief that points at a tool which no longer
        // exists sends the model to call it and collect an unknown-tool error.
        for capability in [
            "`lsp`",
            "agent.spawn",
            "`read`",
            "update_plan",
            "`skill`",
            "`memory`",
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

    #[test]
    fn restricted_brief_omits_unavailable_capabilities_but_keeps_safety_rules() {
        let tools = ["read", "edit", "shell.exec", "grep", "glob"]
            .map(String::from)
            .into_iter()
            .collect();
        let brief = system_prompt_for_tools(None, &tools);
        for absent in [
            "`update_plan`",
            "`clarify`",
            "`agent.spawn`",
            "`skill`",
            "`memory`",
            "`lsp`",
        ] {
            assert!(!brief.contains(absent), "unavailable instruction: {absent}");
        }
        assert!(brief.contains("trust boundary"));
        assert!(brief.contains("Report tool outcomes honestly"));
        assert!(!brief.contains("<!--"));
        assert!(brief.len() < system_prompt(None).len());
    }
}
