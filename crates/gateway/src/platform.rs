//! What every messaging platform provides. Only receiving and sending text are required;
//! buttons, typing and files have defaults that work everywhere, and a platform overrides
//! them when it can do better.

use anyhow::Result;
use std::path::Path;

/// One message from a person, in the same shape whatever platform it came from.
#[derive(Debug, Clone, Default)]
pub struct Incoming {
    /// Where to answer: a private chat, a group or a channel.
    pub chat: String,
    /// Who wrote it; pairing approves people, not chats.
    pub user: String,
    pub name: String,
    pub text: String,
    pub files: Vec<Attachment>,
    /// A message in a group only reaches the agent when it names the bot.
    pub group: bool,
    pub mentioned: bool,
}

/// A file a person sent, fetched only once the sender is known to be allowed.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
}

/// What a platform hears.
#[derive(Debug, Clone)]
pub enum Heard {
    Message(Incoming),
    /// A button pressed under an earlier message.
    Pressed {
        chat: String,
        user: String,
        choice: String,
    },
}

/// A platform refused the saved setup, such as a revoked token: trying again cannot help.
#[derive(Debug)]
pub struct SetupProblem(pub String);

impl std::fmt::Display for SetupProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupProblem {}

/// One button; `choice` comes back in [`Heard::Pressed`] when it is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    pub choice: String,
}

#[async_trait::async_trait]
pub trait Platform: Send + Sync {
    /// The name used in settings and pairing, such as `telegram`.
    fn name(&self) -> &'static str;

    /// The longest message the platform accepts, in characters.
    fn limit(&self) -> usize;

    /// Waits for what people sent next; an empty list is a quiet poll.
    async fn hear(&self) -> Result<Vec<Heard>>;

    async fn send(&self, chat: &str, text: &str) -> Result<()>;

    /// Offers choices. Without buttons, they are numbered and a reply with the number picks one.
    async fn ask(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<()> {
        self.send(chat, &numbered(text, buttons)).await
    }

    /// Shows that the agent is working; platforms without it do nothing.
    async fn typing(&self, _chat: &str) -> Result<()> {
        Ok(())
    }

    async fn fetch(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        anyhow::bail!(
            "{} cannot receive files yet ({})",
            self.name(),
            attachment.name
        )
    }

    async fn send_file(&self, chat: &str, path: &Path) -> Result<()> {
        self.send(
            chat,
            &format!("The file is ready on the computer: {}", path.display()),
        )
        .await
    }
}

pub fn numbered(text: &str, buttons: &[Button]) -> String {
    let mut message = text.to_owned();
    for (number, button) in buttons.iter().enumerate() {
        message.push_str(&format!("\n{}. {}", number + 1, button.label));
    }
    if !buttons.is_empty() {
        message.push_str("\nReply with a number.");
    }
    message
}

/// Splits text into messages no longer than `limit` characters, preferring to break between
/// paragraphs, then lines, then words, and never inside a character.
pub fn split(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > limit {
        let end = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(at, _)| at);
        let window = &rest[..end];
        let cut = ["\n\n", "\n", " "]
            .iter()
            .find_map(|gap| window.rfind(gap).filter(|at| *at > 0))
            .unwrap_or(end);
        parts.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        parts.push(rest.to_owned());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_arrives_whole_in_order_within_the_limit() {
        let text = format!(
            "{}\n\n{}\n{} {}",
            "a".repeat(30),
            "b".repeat(30),
            "c".repeat(25),
            "d".repeat(20)
        );
        let parts = split(&text, 40);
        assert!(parts.iter().all(|part| part.chars().count() <= 40));
        // A paragraph break wins over a line break, which wins over a space.
        assert_eq!(
            parts,
            [
                "a".repeat(30),
                "b".repeat(30),
                "c".repeat(25),
                "d".repeat(20)
            ]
        );
        let joined: String = parts.concat();
        let original: String = text.split_whitespace().collect();
        assert_eq!(joined, original);

        let unbroken = "é".repeat(95);
        let parts = split(&unbroken, 40);
        assert_eq!(
            parts
                .iter()
                .map(|part| part.chars().count())
                .collect::<Vec<_>>(),
            [40, 40, 15]
        );
    }
}
