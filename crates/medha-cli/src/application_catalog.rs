//! Shared application command names and available model adapters. Frontends
//! render these descriptions; the service uses the names to avoid collisions.

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    (
        "/trust",
        "trust this checkout and shown settings; /trust --revoke removes them",
    ),
    (
        "/attach",
        "images: /attach PATH · /attach paste · /attach remove [number|all]",
    ),
    ("/status", "model, context window, current pressure"),
    (
        "/chatgpt",
        "ChatGPT accounts: use one, sign out, or add another",
    ),
    (
        "/usage",
        "tokens and cost — this session and the last 7 days by model; /usage manage opens your ChatGPT plan's usage",
    ),
    (
        "/pulse",
        "config health: which model/key resolves & from where; /pulse fix auto-repairs",
    ),
    (
        "/reasoning",
        "configure mode, visibility, effort, and inspect delivery",
    ),
    (
        "/stream",
        "toggle live token streaming (off: whole reply at once — surfaces reasoning on some gateways)",
    ),
    (
        "/model",
        "switch models, or /model <name>; add presets/custom endpoints and keys",
    ),
    (
        "/fallback",
        "choose saved models to stand in, in order, when yours cannot answer",
    ),
    (
        "/search",
        "choose a web-search provider and key; keyless DuckDuckGo is the default",
    ),
    (
        "/mode",
        "execution: read-only plan · careful · normal · yolo",
    ),
    ("/detail", "expand/collapse full tool input & output"),
    ("/theme", "light · dark · auto (bare /theme toggles)"),
    ("/resume", "switch to a past session"),
    (
        "/rewind",
        "time-travel: branch from an earlier turn (undoes later edits)",
    ),
    ("/tasks", "list currently owned shell tasks"),
    ("/lsp", "language-server sessions and health"),
    (
        "/connect",
        "connect an app — Linear, Notion, Figma… · /connect <name> connects it directly",
    ),
    (
        "/mcp",
        "MCP servers — manage · connect · remove · add  ·  /mcp catalog <search> to browse the registry",
    ),
    (
        "/agents",
        "manage agents · /agents tree · /agents steer … · /agents followup …",
    ),
    (
        "/memory",
        "list memories · /memory <name> jumps to provenance",
    ),
    (
        "/skill",
        "skill hub — use a skill, or add one (search / paste a link)  ·  /skill <name> to load  ·  /skill enable|disable <name>",
    ),
    (
        "/plugins",
        "plugins — install from GitHub or a marketplace · discover · on/off · update",
    ),
    (
        "/hooks",
        "add a hook: pick when it runs and which tools, then a command or script",
    ),
    ("/clear", "reset the conversation"),
    ("/exit", "quit (also Ctrl-D)"),
];

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
