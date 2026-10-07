//! Shared application command names and available model adapters. Frontends
//! render these descriptions; the service uses the names to avoid collisions.

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    (
        "/attach",
        "images: /attach PATH · /attach paste · /attach remove [number|all]",
    ),
    ("/status", "model, context window, current pressure"),
    (
        "/usage",
        "tokens and cost — this session and the last 7 days by model",
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
        "/search",
        "choose the web-search provider (Tavily/Brave/SearXNG) & key; DuckDuckGo fallback",
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

pub(crate) const MODEL_PROTOCOLS: &[(&str, bool, kernel::Protocol)] = &[
    ("OpenAI-compatible Chat", true, kernel::Protocol::OpenAiChat),
    (
        "Gemini Interactions v1",
        true,
        kernel::Protocol::GeminiInteractions,
    ),
    (
        "Anthropic Messages",
        false,
        kernel::Protocol::AnthropicMessages,
    ),
    ("OpenAI Responses", false, kernel::Protocol::OpenAiResponses),
];
