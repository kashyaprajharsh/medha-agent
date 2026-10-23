use super::*;

#[test]
fn an_existing_preset_changes_and_everything_else_stays_as_written() {
    let text = "# team settings\n[policy]\nautonomy = \"careful\" # keep\n\n[tools]\n  preset = \"full\"  # all\n\n[agents]\nmax_active = 2\n";
    let edited = set_tools_preset(text, "minimal").unwrap();
    assert_eq!(
        edited,
        "# team settings\n[policy]\nautonomy = \"careful\" # keep\n\n[tools]\n  preset = \"minimal\"\n\n[agents]\nmax_active = 2\n"
    );
}

#[test]
fn a_missing_table_or_key_is_added() {
    assert_eq!(
        set_tools_preset("[policy]\nautonomy = \"careful\"\n", "minimal").unwrap(),
        "[policy]\nautonomy = \"careful\"\n\n[tools]\npreset = \"minimal\"\n"
    );
    assert_eq!(
        set_tools_preset("", "full").unwrap(),
        "[tools]\npreset = \"full\"\n"
    );
    assert_eq!(
        set_tools_preset("[tools]\n\n[agents]\nmax_active = 2\n", "minimal").unwrap(),
        "[tools]\npreset = \"minimal\"\n\n[agents]\nmax_active = 2\n"
    );
}

#[test]
fn forms_it_cannot_edit_safely_are_refused() {
    assert!(set_tools_preset("tools = { preset = \"full\" }\n", "minimal").is_err());
    assert!(set_tools_preset("tools.preset = \"full\"\n", "minimal").is_err());
    assert!(set_tools_preset("[tools]\npreset = \"full\"\n", "everything").is_err());
}

#[test]
fn a_tool_switch_edits_only_the_deny_line_and_removes_it_when_empty() {
    let text = "[tools]\npreset = \"full\" # all\ndeny = [\"web\"]\n\n[agents]\nmax_active = 2\n";
    let off = set_tool_denied(text, "lsp", true).unwrap();
    assert_eq!(
        off,
        "[tools]\npreset = \"full\" # all\ndeny = [\"web\", \"lsp\"]\n\n[agents]\nmax_active = 2\n"
    );
    let on = set_tool_denied(&set_tool_denied(&off, "lsp", false).unwrap(), "web", false).unwrap();
    assert_eq!(
        on,
        "[tools]\npreset = \"full\" # all\n\n[agents]\nmax_active = 2\n"
    );
    assert_eq!(
        set_tool_denied("", "web", true).unwrap(),
        "[tools]\ndeny = [\"web\"]\n"
    );
}

#[test]
fn a_switch_never_overrules_a_pattern_or_rewrites_a_list_it_cannot_see_whole() {
    assert!(set_tool_denied("[tools]\ndeny = [\"lsp*\"]\n", "lsp", false).is_err());
    assert!(set_tool_denied("[tools]\ndeny = [\n  \"web\",\n]\n", "lsp", true).is_err());
}
