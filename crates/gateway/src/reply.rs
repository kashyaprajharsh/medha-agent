//! Turns a chat's events into what a person reads on a phone: the model's words in whole
//! messages, plus the notices, errors and endings that matter. Tool traffic is not shown.

use protocol::{TurnEvent as E, TurnOutcome as O};
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct Reply {
    text: String,
    /// Messages this chat sent, so their echo is not shown back.
    sent: VecDeque<String>,
    error: Option<String>,
}

impl Reply {
    pub fn sent(&mut self, content: &str) {
        self.sent.push_back(content.trim().to_owned());
        while self.sent.len() > 16 {
            self.sent.pop_front();
        }
    }

    /// What to send now because of this event.
    pub fn event(&mut self, event: E) -> Vec<String> {
        match event {
            E::Text { delta, .. } => {
                self.text.push_str(&delta);
                Vec::new()
            }
            E::ToolCall { .. } | E::ToolStarted { .. } => self.flush(),
            E::User { content, .. } | E::Steered { content } => self.echo(&content),
            E::Notice { text } => self.then(text),
            E::Error { message } => {
                self.error = Some(message.clone());
                self.then(format!("Error: {message}"))
            }
            E::Verify { ok, summary } => {
                self.then(format!("Check {}: {summary}", if ok { "passed" } else { "failed" }))
            }
            E::Returned { contents } => {
                self.then(format!("Not sent, send again if still wanted:\n{}", contents.join("\n")))
            }
            E::Restarted => self.then("The model stopped mid-reply; trying again.".into()),
            E::ProjectTrust { review } if !review.trusted => self.then(format!(
                "This folder's own skills and settings are off until you trust it: run `medha trust --yes` in {}.",
                review.root.display()
            )),
            E::Settled { completion } => {
                let mut messages = self.flush();
                messages.extend(self.ending(completion.outcome));
                messages
            }
            _ => Vec::new(),
        }
    }

    pub fn flush(&mut self) -> Vec<String> {
        let text = std::mem::take(&mut self.text);
        let text = text.trim();
        if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_owned()]
        }
    }

    fn then(&mut self, message: String) -> Vec<String> {
        let mut messages = self.flush();
        messages.push(message);
        messages
    }

    /// A message typed on another screen is shown, so the phone sees the whole conversation.
    fn echo(&mut self, content: &str) -> Vec<String> {
        let content = content.trim();
        if let Some(at) = self.sent.iter().position(|sent| sent == content) {
            self.sent.remove(at);
            return self.flush();
        }
        self.then(format!("From another screen: {content}"))
    }

    fn ending(&mut self, outcome: O) -> Option<String> {
        let error = self.error.take();
        match outcome {
            O::Finished => None,
            O::Cancelled => Some("Stopped.".into()),
            O::Refused => Some("The model declined to answer.".into()),
            O::TokenLimit => Some("The reply stopped at the model's length limit.".into()),
            O::RequestLimit => Some("Stopped at the limit of model requests for one task.".into()),
            O::Failed { message } if error.as_ref() == Some(&message) => None,
            O::Failed { message } => Some(format!("Failed: {message}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(delta: &str) -> E {
        E::Text {
            delta: delta.into(),
            source_event: None,
        }
    }

    fn settled(outcome: O) -> E {
        E::Settled {
            completion: protocol::TurnCompletion { turn: 1, outcome },
        }
    }

    #[test]
    fn words_around_tool_calls_arrive_in_order_and_our_own_message_is_not_echoed() {
        let mut reply = Reply::default();
        reply.sent("fix the test");
        assert!(
            reply
                .event(E::User {
                    content: "fix the test".into(),
                    turn: 1,
                    images: vec![],
                    about: None
                })
                .is_empty()
        );
        assert!(reply.event(text("Running ")).is_empty());
        assert!(reply.event(text("the tests.")).is_empty());
        let call = E::ToolCall {
            id: None,
            tool: "shell.exec".into(),
            args: serde_json::json!({}),
        };
        assert_eq!(reply.event(call), ["Running the tests."]);
        reply.event(text("All pass."));
        let typed = E::User {
            content: "also lint".into(),
            turn: 2,
            images: vec![],
            about: None,
        };
        assert_eq!(
            reply.event(typed),
            ["All pass.", "From another screen: also lint"]
        );
        reply.event(text("Linted."));
        assert_eq!(reply.event(settled(O::Finished)), ["Linted."]);
    }

    #[test]
    fn a_failure_is_told_once() {
        let mut reply = Reply::default();
        assert_eq!(
            reply.event(E::Error {
                message: "no key".into()
            }),
            ["Error: no key"]
        );
        assert!(
            reply
                .event(settled(O::Failed {
                    message: "no key".into()
                }))
                .is_empty()
        );
        assert_eq!(
            reply.event(settled(O::Failed {
                message: "late".into()
            })),
            ["Failed: late"]
        );
    }
}
