//! Durable model instructions that stay out of the person's transcript.

use crate::{Message, Role, TrustLabel};

const PLAN: &str = "For this turn, Plan mode is active. Investigate using read-only tools and present an actionable plan with affected files, risks, and validation steps. Do not implement changes, execute commands, or delegate work.\n\n";

pub(crate) fn apply(message: &mut Message, planning: bool) {
    if planning
        && message.role == Role::User
        && message.trust.is_none_or(|trust| trust == TrustLabel::User)
        && message.instruction_prefix.is_none()
    {
        message.content.insert_str(0, PLAN);
        message.instruction_prefix = Some(PLAN.to_owned());
    }
}

pub(crate) fn without_prefix<'a>(content: &'a str, prefix: Option<&str>) -> &'a str {
    prefix
        .and_then(|prefix| content.strip_prefix(prefix))
        .unwrap_or(content)
}

pub fn shown_payload(payload: &serde_json::Value, from_person: bool) -> (&str, Option<&str>) {
    let text = payload["text"].as_str().unwrap_or_default();
    if !from_person {
        return (text, None);
    }
    let text = without_prefix(text, payload["instruction_prefix"].as_str());
    let about = payload.get("about").and_then(|about| {
        Some((
            usize::try_from(about["lead"].as_u64()?).ok()?,
            about["label"].as_str()?,
        ))
    });
    shown(text, about)
}

pub(crate) fn shown<'a>(
    text: &'a str,
    about: Option<(usize, &'a str)>,
) -> (&'a str, Option<&'a str>) {
    match about {
        Some((lead, label)) if text.is_char_boundary(lead) => {
            (&text[lead..], Some(label).filter(|label| !label.is_empty()))
        }
        _ => (text, None),
    }
}

#[cfg(test)]
#[path = "turn_instruction_tests.rs"]
mod tests;
