//! Shared application command names and available model adapters. Frontends
//! render these descriptions; the service uses the names to avoid collisions.

pub(crate) use protocol::commands::COMMANDS;

/// Whether each one can be picked comes from `providers::implemented`.
pub(crate) const MODEL_PROTOCOLS: &[(&str, kernel::Protocol)] = &[
    ("OpenAI-compatible Chat", kernel::Protocol::OpenAiChat),
    ("OpenAI Responses", kernel::Protocol::OpenAiResponses),
    (
        "Gemini Interactions v1",
        kernel::Protocol::GeminiInteractions,
    ),
    ("Anthropic Messages", kernel::Protocol::AnthropicMessages),
];
