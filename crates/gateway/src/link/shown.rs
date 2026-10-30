//! Messages the chat changes after sending: a choice is settled in place, its buttons taken
//! away and the outcome written under it, and the tool steps of the work under way are one
//! message rewritten as they run.

use super::ChatLink;
use crate::platform::split;
use crate::progress::Progress;
use crate::prompts::Ask;
use protocol::TurnEvent;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// Telegram takes about one change a second in a chat; steps arriving faster are drawn together.
const REDRAW: Duration = Duration::from_millis(1500);

/// A message with buttons, by what it asked.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Asked {
    Approval(u64),
    Question(u64),
    Command(&'static str),
}

/// A sent message that may change: its id and the words it shows.
struct Shown {
    id: String,
    text: String,
}

#[derive(Default)]
pub(super) struct Live {
    asked: HashMap<Asked, Shown>,
    progress: Progress,
    /// This stretch's steps message, once sent.
    steps: Option<Shown>,
    /// Steps are shown in this turn: the setting is on and the platform can change a message.
    on: bool,
    /// When the steps message is next drawn, while it has news.
    pub due: Option<Instant>,
    drawn: Option<Instant>,
}

impl ChatLink {
    /// Offers choices; a message kept as `asked` is settled in place later.
    pub(super) async fn ask(&mut self, ask: Ask, asked: Option<Asked>) {
        let platform = std::sync::Arc::clone(&self.shared.platform);
        let mut parts = split(&ask.text, platform.limit());
        let last = parts.pop().unwrap_or_default();
        for part in parts {
            self.send_text(&part).await;
        }
        // A list offered again replaces the last one, whose buttons would now pick from it.
        if let Some(asked) = asked {
            self.settle(asked, "Replaced by the list below.").await;
        }
        match platform.ask(&self.chat, &last, &ask.buttons).await {
            Ok(Some(id)) => {
                if let Some(asked) = asked {
                    self.live.asked.insert(asked, Shown { id, text: last });
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "gateway: offering choices")
            }
        }
    }

    /// Takes the buttons away and writes `outcome` under the question; `false` when there was
    /// no such message to change, so the caller may say it instead.
    pub(super) async fn settle(&mut self, asked: Asked, outcome: &str) -> bool {
        let Some(shown) = self.live.asked.remove(&asked) else {
            return false;
        };
        // The question keeps every line; only one at the very limit loses its end.
        let room = self
            .shared
            .platform
            .limit()
            .saturating_sub(outcome.chars().count() + 3);
        let asked = if shown.text.chars().count() > room {
            let kept: String = shown.text.chars().take(room).collect();
            format!("{kept}…")
        } else {
            shown.text
        };
        let text = format!("{asked}\n\n{outcome}");
        match self
            .shared
            .platform
            .edit(&self.chat, &shown.id, &text)
            .await
        {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "gateway: settling a choice");
                false
            }
        }
    }

    /// Approvals and questions end with the chat; a command's list still works after it.
    pub(super) async fn settle_waiting(&mut self, outcome: &str) {
        let waiting: Vec<Asked> = self
            .live
            .asked
            .keys()
            .copied()
            .filter(|asked| !matches!(asked, Asked::Command(_)))
            .collect();
        for asked in waiting {
            self.settle(asked, outcome).await;
        }
    }

    /// Notes a tool step; it is drawn when due, so a burst of steps is one change.
    pub(super) fn note_step(&mut self, event: &TurnEvent) {
        let live = &mut self.live;
        match event {
            TurnEvent::Started { .. } => {
                live.progress = Progress::default();
                live.steps = None;
                live.on = self.shared.settings.steps();
                return;
            }
            _ if !live.on => return,
            TurnEvent::ToolCall { id, tool, args } => {
                live.progress.call(id.as_deref(), tool, args, &self.folder);
            }
            TurnEvent::ToolResult { id, ok, .. } => live.progress.result(id.as_deref(), *ok),
            _ => return,
        }
        let soonest = live.drawn.map_or_else(Instant::now, |drawn| drawn + REDRAW);
        live.due.get_or_insert(soonest);
    }

    pub(super) async fn draw_steps(&mut self) {
        self.live.due = None;
        if self.live.progress.is_empty() {
            return;
        }
        let text = self.live.progress.text();
        let platform = std::sync::Arc::clone(&self.shared.platform);
        self.live.drawn = Some(Instant::now());
        match &self.live.steps {
            Some(shown) if shown.text == text => {}
            Some(shown) => {
                if let Err(error) = platform.edit(&self.chat, &shown.id, &text).await {
                    tracing::warn!(error = %format!("{error:#}"), "gateway: drawing the steps");
                } else if let Some(shown) = &mut self.live.steps {
                    shown.text = text;
                }
            }
            None => match platform.send_live(&self.chat, &text).await {
                Ok(Some(id)) => self.live.steps = Some(Shown { id, text }),
                // Nothing can be changed on this platform, so its chats show no steps.
                Ok(None) => self.live.on = false,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "gateway: sending the steps");
                    self.live.on = false;
                }
            },
        }
    }

    /// Before a choice is offered: the steps so far are drawn, and the step waiting on the
    /// choice is marked in the same message once it runs.
    pub(super) async fn steps_now(&mut self) {
        if self.live.due.is_some() {
            self.draw_steps().await;
        }
    }

    /// Before the model's words or a notice: the steps so far are drawn whole, and later ones
    /// start a new message under what is said, so the chat reads in order.
    pub(super) async fn close_steps(&mut self) {
        self.steps_now().await;
        self.live.progress.close();
        self.live.steps = None;
    }
}
