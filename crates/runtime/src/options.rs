//! What a caller chose for one chat, gathered once so nothing later reads flags or the environment.

use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Resume {
    #[default]
    None,
    Latest,
    Id(String),
}

/// `None` means the caller left it to `medha.lock` or the saved profile.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub model_env: crate::config::ModelEnv,
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
}
