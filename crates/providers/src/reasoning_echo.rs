//! Whether a server must be sent a model's earlier thinking back.
//!
//! Some models refuse a request whose earlier assistant turns lack their
//! thinking; most servers refuse one that carries it. So it is sent only when
//! the model catalogue says this model wants it under `reasoning_content`, or
//! the person said so for a server the catalogue does not know.

use crate::ProviderProfile;

/// The one field the chat protocol can carry thinking back in.
const FIELD: &str = "reasoning_content";

/// Whether earlier assistant turns go to this server with their thinking.
pub(crate) fn expected(profile: &ProviderProfile) -> bool {
    let listed = || crate::models_dev::thinking_field(profile);
    decided(profile.reasoning_echo, listed)
}

/// The person's own setting wins; without one, what the catalogue records.
fn decided(set: Option<bool>, listed: impl FnOnce() -> Option<String>) -> bool {
    set.unwrap_or_else(|| listed().as_deref() == Some(FIELD))
}

#[cfg(test)]
#[path = "reasoning_echo_tests.rs"]
mod tests;
