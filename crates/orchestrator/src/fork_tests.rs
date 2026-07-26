use super::*;

fn say(role: Role, content: &str) -> Message {
    Message::new(role, content)
}

fn calling(content: &str) -> Message {
    Message {
        tool_calls: vec![kernel::ToolIntent {
            id: "c1".into(),
            tool: "fs.read".into(),
            args: serde_json::json!({}),
        }],
        ..Message::new(Role::Assistant, content)
    }
}

fn conversation() -> Vec<Message> {
    vec![
        say(Role::System, "you are medha"),
        say(Role::User, "what does the parser do?"),
        calling("let me look"),
        say(Role::Tool, "…4000 lines of file…"),
        say(Role::Assistant, "it builds an AST"),
        say(Role::User, "now check the lexer"),
        say(Role::Assistant, "the lexer is table-driven"),
    ]
}

fn texts(messages: &[Message]) -> Vec<&str> {
    messages.iter().map(|m| m.content.as_str()).collect()
}

#[test]
fn a_cold_child_inherits_nothing() {
    assert!(Fork::None.apply(&conversation()).is_empty());
}

#[test]
fn a_fork_carries_the_conversation_but_not_the_working_out() {
    let inherited = Fork::All.apply(&conversation());
    // Tool results are the bulk of the tokens and name call ids the child's
    // provider never issued; the assistant turn that only carried a tool call
    // is half an exchange whose other half is gone.
    assert_eq!(
        texts(&inherited),
        [
            "you are medha",
            "what does the parser do?",
            "it builds an AST",
            "now check the lexer",
            "the lexer is table-driven",
        ]
    );
}

#[test]
fn the_last_n_turns_start_at_a_user_message() {
    let inherited = Fork::LastTurns(1).apply(&conversation());
    // A window that began mid-exchange would hand the child an answer to a
    // question it cannot see.
    assert_eq!(
        texts(&inherited),
        ["now check the lexer", "the lexer is table-driven"]
    );
}

#[test]
fn asking_for_more_turns_than_exist_yields_the_whole_conversation() {
    let history = conversation();
    assert_eq!(
        texts(&Fork::LastTurns(99).apply(&history)),
        texts(&Fork::All.apply(&history))
    );
}

#[test]
fn empty_messages_are_never_inherited() {
    // A blank turn costs a message slot and says nothing.
    let blank = vec![say(Role::User, "   "), say(Role::Assistant, "")];
    assert!(Fork::All.apply(&blank).is_empty());
}

#[test]
fn a_fork_setting_is_parsed_or_refused_never_guessed() {
    assert_eq!(Fork::parse("none").unwrap(), Fork::None);
    assert_eq!(Fork::parse("ALL").unwrap(), Fork::All);
    assert_eq!(Fork::parse(" 3 ").unwrap(), Fork::LastTurns(3));
    assert!(Fork::parse("0").is_err());
    assert!(Fork::parse("some").is_err());
}
