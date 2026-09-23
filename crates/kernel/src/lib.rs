//! MEDHA kernel: provider calls, event logging, budgets, and execution policy.

pub mod artifacts;
pub mod budgets;
pub mod clarify;
pub mod context;
pub mod errors;
pub mod events;
pub mod executor;
pub mod gate;
pub mod hooks;
pub mod interrupts;
pub mod policy;
pub mod progress;
pub mod provider;
pub mod sink;
pub mod types;
pub mod verify;
pub mod vision;

#[path = "loop_.rs"]
pub mod kernel_loop;

pub use artifacts::ArtifactStore;
pub use budgets::{Budget, BudgetHandle, BudgetStop, DEFAULT_MAX_TURNS, Governor, Pooled};
pub use clarify::{Answer, Asker, NoAsker, QOption, Question};
pub use context::{
    AuthorizedContextPath, CompileControl, CompileResult, ContextCompileError, ContextEngine,
    ContextPressure, DiscoveredContext, ProgressiveContext, ProgressiveContextPathAuthorizer,
};
pub use errors::KernelError;
pub use events::{
    AGENT_REPORT_ACKS_FIELD, Event, EventKind, EventLog, FileRollback, InMemoryLog, MutationLease,
    Provenance, SessionMeta, cut_index, project_messages, project_ordered_messages, rollback_plan,
    rollback_plan_in,
};
pub use executor::{BackgroundTask, Executor};
pub use gate::{
    Approval, AutoDeny, ExecutionAccess, HumanGate, NetworkDecision, execution_access,
    execution_access_scope, network_once_active, network_once_scope,
};
pub use hooks::{
    HookAudit, HookBatch, HookContext, HookDirective, HookRequest, HookRunner, HookStatus, NoHooks,
};
pub use interrupts::{Activity, Interrupt, InterruptHandle, InterruptQueue};
pub use kernel_loop::{DEFAULT_MAX_PARALLEL_TOOLS, Kernel, SPILL_THRESHOLD, StopReason};
pub use medha_extension_api::{HookDecision, HookPoint};
pub use policy::{AllowAll, Policy};
pub use progress::{Phase, Progress, ProgressHandle, ProgressWatch};
pub use provider::{
    ImageInputMode, ImageSupport, InputTokenCount, ModelLimits, PreparedModelRequest, Protocol,
    Provider, ProviderCaps, ProviderError, ProviderFailure, ReasoningConfig, ReasoningEffort,
    ReasoningSupport, TokenAccountingMode, TokenCountError, TokenCountQuality, ToolCallStrategy,
};
pub use sink::{NullSink, StreamSink};
pub use types::{
    AutonomyLevel, BlastRadius, Block, CompiledContext, Containment, ContentPart, Decision,
    LegacyMessageError, MediaPart, MediaSource, Message, ModelMessage, ObsStatus, Observation,
    Pricing, ProviderState, ReasoningPart, Role, Session, TextPart, ToolCallPart, ToolCategory,
    ToolIntent, ToolNameError, ToolResultPart, ToolSpec, TrustLabel, TurnResult, Usage,
    canonical_tool_names, portable_tool_name, portable_tool_name_map,
};
pub use verify::{NoVerify, Verifier, VerifyReport};
pub use vision::{NoVision, VisionDescriber, describe_media};

mod usage_reporting;
