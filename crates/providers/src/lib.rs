//! Provider adapters; `openai_compat` is the baseline.

pub mod catalogue;
mod learned;
pub mod model_test;
pub mod models_dev;
pub mod openai_compat;
pub mod profile;
pub(crate) mod protocol;
#[cfg(test)]
mod test_http;
pub(crate) mod transport;

pub use models_dev::{
    CapabilityResolution, CapabilitySource, CapabilityState, ModalitySet, ModelCapabilities,
};
pub use openai_compat::{OpenAiCompat, ProviderClient};
pub use profile::{AuthKind, ProviderProfile, TokenCounter};
