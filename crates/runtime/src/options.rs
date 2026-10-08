//! What a caller chose for one chat, gathered once so nothing later reads flags or the environment.

use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Resume {
    #[default]
    None,
    Latest,
    Id(String),
}

fn env_text(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} must contain valid Unicode"),
    }
}

impl SessionOptions {
    /// The frontend resolves its flags/environment once. The backend must not
    /// replace these choices with those of the process that started it.
    pub fn startup(&self) -> protocol::StartupOptions {
        let env = &self.model_env;
        protocol::StartupOptions {
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            model_env: protocol::ModelOverrides {
                base_url: env.base_url.clone(),
                model: env.model.clone(),
                api_key: env.api_key.clone(),
                max_ctx: env.max_ctx.clone(),
                protocol: env.protocol.clone(),
                auth: env.auth.clone(),
                headers_json: env.headers_json.clone(),
                max_output_tokens: env.max_output_tokens.clone(),
                token_counter: env.token_counter.clone(),
                token_accounting: env.token_accounting.clone(),
                reasoning_support: env.reasoning_support.clone(),
                image_input: env.image_input.clone(),
            },
            search_env: protocol::SearchOverrides {
                tavily: self.search_env.tavily.clone(),
                brave: self.search_env.brave.clone(),
                searxng: self.search_env.searxng.clone(),
            },
            budget: protocol::BudgetLimits {
                max_turns: self.budget.max_turns,
                max_tokens: self.budget.max_tokens,
                max_cost_usd: self.budget.max_cost_usd,
                max_wall_s: self.budget.max_wall_s,
            },
            approve: self.approve.clone(),
            reasoning: self.reasoning.as_ref().map(|r| protocol::ReasoningChoice {
                enabled: r.enabled,
                effort: r.effort.map(|e| match e {
                    kernel::ReasoningEffort::None => protocol::Effort::None,
                    kernel::ReasoningEffort::Minimal => protocol::Effort::Minimal,
                    kernel::ReasoningEffort::Low => protocol::Effort::Low,
                    kernel::ReasoningEffort::Medium => protocol::Effort::Medium,
                    kernel::ReasoningEffort::High => protocol::Effort::High,
                    kernel::ReasoningEffort::XHigh => protocol::Effort::XHigh,
                    kernel::ReasoningEffort::Max => protocol::Effort::Max,
                    kernel::ReasoningEffort::Ultra => protocol::Effort::Ultra,
                }),
            }),
            mode: self.autonomy.map(|m| match m {
                kernel::AutonomyLevel::Plan => protocol::Mode::Plan,
                kernel::AutonomyLevel::Careful => protocol::Mode::Careful,
                kernel::AutonomyLevel::Normal => protocol::Mode::Normal,
                kernel::AutonomyLevel::Yolo => protocol::Mode::Yolo,
            }),
            verify_command: self.verify_command.clone(),
            require_verify: self.require_verify,
            sandbox: self.sandbox.map(|s| match s {
                sandbox::BackendKind::Host => protocol::Sandbox::Host,
                sandbox::BackendKind::Native => protocol::Sandbox::Native,
                sandbox::BackendKind::Container => protocol::Sandbox::Container,
                sandbox::BackendKind::Ssh => protocol::Sandbox::Ssh,
            }),
            tools_preset: self.tools_preset.clone(),
            max_parallel_tools: self.max_parallel_tools,
            resume: match &self.resume {
                Resume::None => protocol::Resume::None,
                Resume::Latest => protocol::Resume::Latest,
                Resume::Id(id) => protocol::Resume::Id(id.clone()),
            },
            first_run_setup: self.first_run_setup,
            may_start_unconfigured: self.may_start_unconfigured,
            session_mcp: self.session_mcp.clone(),
        }
    }

    pub fn from_startup(chosen: protocol::StartupOptions) -> anyhow::Result<Self> {
        crate::session_mcp::validate(&chosen.session_mcp)?;
        if let Some(cost) = chosen.budget.max_cost_usd {
            anyhow::ensure!(
                cost.is_finite() && cost >= 0.0,
                "maximum cost must be finite and non-negative"
            );
        }
        let env = chosen.model_env;
        let reasoning = chosen.reasoning.map(|r| {
            let effort = r.effort.and_then(|e| match e {
                protocol::Effort::Auto => None,
                protocol::Effort::None => Some(kernel::ReasoningEffort::None),
                protocol::Effort::Minimal => Some(kernel::ReasoningEffort::Minimal),
                protocol::Effort::Low => Some(kernel::ReasoningEffort::Low),
                protocol::Effort::Medium => Some(kernel::ReasoningEffort::Medium),
                protocol::Effort::High => Some(kernel::ReasoningEffort::High),
                protocol::Effort::XHigh => Some(kernel::ReasoningEffort::XHigh),
                protocol::Effort::Max => Some(kernel::ReasoningEffort::Max),
                protocol::Effort::Ultra => Some(kernel::ReasoningEffort::Ultra),
            });
            kernel::ReasoningConfig {
                enabled: r.enabled,
                effort,
            }
        });
        Ok(Self {
            model: chosen.model,
            base_url: chosen.base_url,
            model_env: crate::config::ModelEnv {
                base_url: env.base_url,
                model: env.model,
                api_key: env.api_key,
                max_ctx: env.max_ctx,
                protocol: env.protocol,
                auth: env.auth,
                headers_json: env.headers_json,
                max_output_tokens: env.max_output_tokens,
                token_counter: env.token_counter,
                token_accounting: env.token_accounting,
                reasoning_support: env.reasoning_support,
                image_input: env.image_input,
            },
            search_env: crate::config::SearchEnv {
                tavily: chosen.search_env.tavily,
                brave: chosen.search_env.brave,
                searxng: chosen.search_env.searxng,
            },
            budget: crate::budget::BudgetLimits {
                max_turns: chosen.budget.max_turns,
                max_tokens: chosen.budget.max_tokens,
                max_cost_usd: chosen.budget.max_cost_usd,
                max_wall_s: chosen.budget.max_wall_s,
            },
            approve: chosen.approve,
            reasoning,
            autonomy: chosen.mode.map(|m| match m {
                protocol::Mode::Plan => kernel::AutonomyLevel::Plan,
                protocol::Mode::Careful => kernel::AutonomyLevel::Careful,
                protocol::Mode::Normal => kernel::AutonomyLevel::Normal,
                protocol::Mode::Yolo => kernel::AutonomyLevel::Yolo,
            }),
            verify_command: chosen.verify_command,
            require_verify: chosen.require_verify,
            sandbox: chosen.sandbox.map(|s| match s {
                protocol::Sandbox::Host => sandbox::BackendKind::Host,
                protocol::Sandbox::Native => sandbox::BackendKind::Native,
                protocol::Sandbox::Container => sandbox::BackendKind::Container,
                protocol::Sandbox::Ssh => sandbox::BackendKind::Ssh,
            }),
            tools_preset: chosen.tools_preset,
            max_parallel_tools: chosen.max_parallel_tools,
            resume: match chosen.resume {
                protocol::Resume::None => Resume::None,
                protocol::Resume::Latest => Resume::Latest,
                protocol::Resume::Id(id) => Resume::Id(id),
            },
            first_run_setup: chosen.first_run_setup,
            may_start_unconfigured: chosen.may_start_unconfigured,
            session_mcp: chosen.session_mcp,
            ..Self::default()
        })
    }

    /// What one chat starts from: what its caller chose, and for the rest what
    /// this process's environment asks of every chat. A terminal's flag or a
    /// client's request wins over the environment, and the environment wins
    /// over `medha.lock`. What the caller chose is never read from the
    /// environment at all, so a value there that makes no sense cannot stand
    /// in the way of a choice that does.
    pub fn from_process(
        mode: Option<kernel::AutonomyLevel>,
        reasoning: Option<kernel::ReasoningConfig>,
    ) -> anyhow::Result<Self> {
        let reasoning = match reasoning {
            Some(chosen) => Some(chosen),
            None => env_text("MEDHA_REASONING_EFFORT")?
                .map(|effort| kernel::ReasoningConfig::from_effort_text(&effort))
                .transpose()
                .map_err(anyhow::Error::msg)?,
        };
        let autonomy = match mode {
            Some(chosen) => Some(chosen),
            None => env_text("MEDHA_MODE")?
                .map(|mode| kernel::AutonomyLevel::parse(&mode))
                .transpose()
                .map_err(anyhow::Error::msg)?,
        };
        let sandbox = match std::env::var("MEDHA_SANDBOX")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("host" | "off" | "none") => Some(sandbox::BackendKind::Host),
            Some("native" | "on") => Some(sandbox::BackendKind::Native),
            _ => None,
        };
        Ok(Self {
            reasoning,
            autonomy,
            verify_command: env_text("MEDHA_VERIFY")?.filter(|command| !command.trim().is_empty()),
            sandbox,
            tools_preset: std::env::var("MEDHA_TOOLS").ok(),
            max_parallel_tools: crate::budget::env_number("MEDHA_MAX_PARALLEL_TOOLS")?,
            approve: std::env::var("MEDHA_APPROVE").ok(),
            model_env: crate::config::ModelEnv::from_process(),
            search_env: crate::config::SearchEnv::from_process(),
            budget: crate::budget::BudgetLimits::from_env()?,
            ..Self::default()
        })
    }
}

/// `None` means the caller left it to `medha.lock` or the saved profile.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub model_env: crate::config::ModelEnv,
    pub search_env: crate::config::SearchEnv,
    pub budget: crate::budget::BudgetLimits,
    pub approve: Option<String>,
    pub reasoning: Option<kernel::ReasoningConfig>,
    pub autonomy: Option<kernel::AutonomyLevel>,
    pub verify_command: Option<String>,
    pub require_verify: bool,
    pub sandbox: Option<sandbox::BackendKind>,
    pub tools_preset: Option<String>,
    pub max_parallel_tools: Option<usize>,
    pub resume: Resume,
    pub prompt: String,
    pub attach: Vec<PathBuf>,
    pub first_run_setup: bool,
    /// The surface can open model setup itself, so no saved model is not an error.
    pub may_start_unconfigured: bool,
    /// Who runs the user's remote MCP servers for every chat, when someone does.
    pub mcp_host: Option<mcp::hub::Endpoint>,
    pub session_mcp: Vec<protocol::SessionMcpServer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_startup_preserves_policy_budgets_models_and_ephemeral_credentials() {
        let options = SessionOptions {
            model: Some("caller-model".into()),
            base_url: Some("http://caller/v1".into()),
            model_env: crate::config::ModelEnv {
                model: Some("env-model".into()),
                api_key: Some("ephemeral-model-secret".into()),
                headers_json: Some(r#"{"X-Private":"ephemeral-header-secret"}"#.into()),
                protocol: Some("open-ai-chat".into()),
                auth: Some("bearer".into()),
                max_ctx: Some("128000".into()),
                max_output_tokens: Some("4000".into()),
                token_counter: Some("cl100k".into()),
                token_accounting: Some("adaptive".into()),
                reasoning_support: Some("effort".into()),
                image_input: Some("supported".into()),
                base_url: Some("http://env/v1".into()),
            },
            search_env: crate::config::SearchEnv {
                tavily: Some("ephemeral-search-secret".into()),
                brave: None,
                searxng: Some("http://caller/search".into()),
            },
            autonomy: Some(kernel::AutonomyLevel::Plan),
            approve: Some("read".into()),
            reasoning: Some(kernel::ReasoningConfig {
                enabled: Some(true),
                effort: Some(kernel::ReasoningEffort::High),
            }),
            verify_command: Some("cargo test".into()),
            require_verify: true,
            sandbox: Some(sandbox::BackendKind::Native),
            tools_preset: Some("minimal".into()),
            max_parallel_tools: Some(3),
            budget: crate::budget::BudgetLimits {
                max_turns: Some(4),
                max_tokens: Some(1000),
                max_cost_usd: Some(1.25),
                max_wall_s: Some(30),
            },
            resume: Resume::Latest,
            first_run_setup: true,
            may_start_unconfigured: true,
            ..SessionOptions::default()
        };
        let chosen = options.startup();
        let debug = format!("{chosen:?}");
        for secret in [
            "ephemeral-model-secret",
            "ephemeral-header-secret",
            "ephemeral-search-secret",
        ] {
            assert!(!debug.contains(secret));
        }
        let encoded = serde_json::to_value(&chosen).unwrap();
        let restored =
            SessionOptions::from_startup(serde_json::from_value(encoded.clone()).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(restored.startup()).unwrap(), encoded);
        assert_eq!(restored.autonomy, options.autonomy);
        assert_eq!(restored.reasoning, options.reasoning);
        assert_eq!(restored.resume, Resume::Latest);
        assert_eq!(restored.model_env.api_key, options.model_env.api_key);
        assert_eq!(restored.search_env.tavily, options.search_env.tavily);
    }

    #[test]
    fn startup_rejects_invalid_cost_without_reading_any_process_choices() {
        for cost in [f64::NAN, f64::INFINITY, -1.0] {
            let chosen = protocol::StartupOptions {
                budget: protocol::BudgetLimits {
                    max_cost_usd: Some(cost),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(SessionOptions::from_startup(chosen).is_err());
        }
        let defaults = SessionOptions::from_startup(protocol::StartupOptions::default()).unwrap();
        assert!(defaults.autonomy.is_none());
        assert!(defaults.model_env.api_key.is_none());
        assert!(defaults.search_env.tavily.is_none());
    }
}
