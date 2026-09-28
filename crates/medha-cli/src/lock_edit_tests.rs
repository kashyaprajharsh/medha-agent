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
