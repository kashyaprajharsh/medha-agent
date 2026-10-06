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
}
