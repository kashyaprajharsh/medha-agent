//! What a child inherits of its parent's conversation.

use kernel::{Message, Role};

/// How much of the parent's conversation a child starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fork {
    /// Start cold. The objective is all the child gets.
    None,
    #[default]
    All,
    /// The last `n` user turns and everything after them.
    LastTurns(usize),
}

impl Fork {
    /// Parse the tool-facing spelling: `none`, `all`, or a positive integer.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        if text.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        match text.parse::<usize>() {
            Ok(0) | Err(_) => Err(format!(
                "'{text}' is not a fork setting — use 'none', 'all', or a positive number of turns"
            )),
            Ok(turns) => Ok(Self::LastTurns(turns)),
        }
    }

    /// The slice of `history` this child inherits.
    pub fn apply(&self, history: &[Message]) -> Vec<Message> {
        if *self == Self::None {
            return Vec::new();
        }
        let scoped = match self {
            Self::LastTurns(turns) => &history[from_last_turns(history, *turns)..],
            _ => history,
        };
        scoped.iter().filter(|m| inheritable(m)).cloned().collect()
    }
}

/// The conversation, not the working-out: a child inherits what was said, never
/// how the parent got there.
///
/// Tool calls and their results are dropped for three reasons that all point the
/// same way — they are the bulk of the tokens, they name call ids the child's
/// provider never issued (which some providers reject outright), and they
/// describe a workspace the child may not even share.
fn inheritable(message: &Message) -> bool {
    match message.role {
        Role::System | Role::User => !message.content.trim().is_empty(),
        // Only a final answer. An assistant turn that exists to carry tool calls
        // is half of an exchange whose other half is being dropped.
        Role::Assistant => message.tool_calls.is_empty() && !message.content.trim().is_empty(),
        Role::Tool => false,
    }
}

/// Index of the start of the last `turns` user turns.
fn from_last_turns(history: &[Message], turns: usize) -> usize {
    history
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, _)| index)
        .rev()
        .nth(turns.saturating_sub(1))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "fork_tests.rs"]
mod tests;
