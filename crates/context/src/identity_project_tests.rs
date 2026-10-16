use super::ProjectSlot;

#[test]
fn builder_owned_sections_preserve_personal_headings_and_replace_only_their_own_slot() {
    let prefix = "Persona\n\n## Memory\nPersonal notes\nAGENTS.md\n## Memory\nEND_OF_AGENTS";
    let mut prompt = ProjectSlot::after(prefix);
    prompt.set_tail("\n\n## Memory\nReal recall".into());
    let first = prompt.render("\n\nProject guidance one");
    assert_eq!(
        first,
        format!("{prefix}\n\nProject guidance one\n\n## Memory\nReal recall")
    );
    let second = prompt.render("\n\nProject guidance two");
    assert!(!second.contains("guidance one"));
    assert!(second.starts_with(prefix));
    prompt.set_tail("\n\n## Memory\nUpdated recall".into());
    let third = prompt.render("\n\nProject guidance two");
    assert!(third.starts_with(prefix) && third.ends_with("Updated recall"));
    assert_eq!(third.matches("Project guidance two").count(), 1);
    assert_eq!(
        prompt.clone().render("Child guidance"),
        format!("{prefix}Child guidance\n\n## Memory\nUpdated recall")
    );
}
