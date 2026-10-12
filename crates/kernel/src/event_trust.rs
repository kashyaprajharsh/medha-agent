use crate::{ContentPart, Event, EventKind, Message, ModelMessage, Role, TrustLabel};
use std::collections::HashSet;

#[derive(Default)]
pub(crate) struct ReplayTrust {
    web: bool,
    web_calls: HashSet<String>,
}

impl ReplayTrust {
    pub fn observe(&mut self, event: &Event) {
        self.web |= event.trust == TrustLabel::Web;
        if event.kind == EventKind::ToolObs
            && event.trust == TrustLabel::Web
            && let Some(id) = event.payload["intent_id"].as_str()
        {
            self.web_calls.insert(id.to_owned());
        }
    }

    pub fn summary(&self, mut message: Message) -> Message {
        if self.web {
            message.trust = Some(TrustLabel::Web);
        }
        message
    }

    pub fn legacy(&self, snapshot: &mut [Message], summary: Option<&str>) {
        for message in snapshot {
            let web_result = message
                .tool_call_id
                .as_ref()
                .is_some_and(|id| self.web_calls.contains(id));
            let web_summary = self.web
                && message.role == Role::Assistant
                && summary == Some(message.content.as_str());
            if web_result || web_summary {
                message.trust = Some(TrustLabel::Web);
            }
        }
    }

    pub fn ordered(&self, snapshot: &mut [ModelMessage], summary: Option<&str>) {
        for message in snapshot {
            let web_result = message.parts.iter().any(|part| matches!(part, ContentPart::ToolResult(result) if self.web_calls.contains(&result.tool_call_id)));
            let web_summary = self.web
                && message.role == Role::Assistant
                && summary.is_some_and(|summary| {
                    let text: String = message
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text(part) => Some(part.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    text == summary
                });
            if web_result || web_summary {
                message.trust = Some(TrustLabel::Web);
            }
        }
    }
}
