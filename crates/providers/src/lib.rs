//! Provider adapters; `client` sends any implemented protocol for a saved profile.

pub mod catalogue;
#[cfg(test)]
mod certificate_tests;
pub mod chatgpt;
pub mod client;
pub mod discovery;
#[cfg(test)]
mod fallback_tests;
mod key_rotation;
pub mod keys;
mod learned;
#[cfg(test)]
#[path = "../examples/update_logos/rules.rs"]
mod logo_rules;
#[cfg(test)]
mod logo_tests;
pub mod model_test;
pub mod models_dev;
pub mod profile;
pub mod prompt_cache;
pub(crate) mod protocol;
#[cfg(test)]
mod proxy_tests;
mod reasoning_echo;
mod schema_rules;
#[cfg(test)]
mod test_http;
pub(crate) mod transport;

pub use client::ProviderClient;

/// Whether Medha can send this wire format today; every surface asks this one place.
pub fn implemented(protocol: kernel::Protocol) -> bool {
    protocol::wire(protocol).is_ok()
}
pub use models_dev::{
    CapabilityResolution, CapabilitySource, CapabilityState, ModalitySet, ModelCapabilities,
};
pub use profile::{AuthKind, ProviderProfile, TokenCounter};
