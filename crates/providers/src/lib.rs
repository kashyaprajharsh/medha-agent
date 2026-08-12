//! Provider adapters; `openai_compat` is the baseline.

pub mod models_dev;
pub mod openai_compat;
pub mod profile;
pub(crate) mod protocol;
pub(crate) mod transport;

pub use openai_compat::{OpenAiCompat, ProviderClient};
pub use profile::{AuthKind, ProviderProfile, TokenCounter};
