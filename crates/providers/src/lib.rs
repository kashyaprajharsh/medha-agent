//! Provider adapters. Open-first: `openai_compat` is the baseline/reference
//! adapter (vLLM, llama.cpp, Ollama, Together, Groq, OpenRouter, …) — one
//! adapter, parametrized by `base_url`, covers the open ecosystem (§4.4).
//! Native Anthropic/Gemini adapters are opt-in upgrades, added later.

pub mod models_dev;
pub mod openai_compat;
pub mod profile;
pub(crate) mod protocol;
pub(crate) mod transport;

pub use openai_compat::{OpenAiCompat, ProviderClient};
pub use profile::{AuthKind, ProviderProfile, TokenCounter};
