//! MEDHA kernel: provider calls, event logging, budgets, and execution policy.

pub mod arg_repair;
pub mod artifacts;
pub mod budgets;
pub mod clarify;
pub mod context;
mod cut_off;
mod empty_reply;
pub mod errors;
mod event_trust;
pub mod events;
pub mod executor;
pub mod gate;
pub mod hooks;
pub mod injection;
pub mod interrupts;
pub mod policy;
pub mod progress;
pub mod provenance;
pub mod provider;
pub mod recovery;
pub mod redact;
mod rejections;
mod repetition;
pub mod request_images;
mod seam;
pub mod sink;
pub mod tool_snapshot;
pub mod turn_instruction;
pub mod types;
pub mod verify;
pub mod vision;
pub mod write_lanes;

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
    Provenance, SessionMeta, TOOL_SCREEN_FIELD, cut_index, project_messages,
    project_ordered_messages, rollback_plan, rollback_plan_in,
};
pub use executor::{BackgroundTask, Executor};
pub use gate::{
    Approval, AutoDeny, ExecutionAccess, HumanGate, NetworkDecision, PathAccess, PathApproval,
    PathKind, PathRequest, execution_access, execution_access_scope, network_once_active,
    network_once_scope,
};
pub use hooks::{
    HookAudit, HookBatch, HookContext, HookDirective, HookRequest, HookRunner, HookStatus, NoHooks,
};
pub use interrupts::{Activity, Interrupt, InterruptHandle, InterruptQueue};
pub use kernel_loop::{
    DEFAULT_MAX_PARALLEL_TOOLS, Kernel, SPILL_THRESHOLD, SessionClaim, SessionOwner, StopReason,
};
pub use medha_extension_api::{HookDecision, HookPoint};
pub use policy::{AllowAll, Policy};
pub use progress::{Phase, Progress, ProgressHandle, ProgressWatch};
pub use provider::{
    ImageInputMode, ImageSupport, InputTokenCount, ModelLimits, PreparedModelRequest, Protocol,
    Provider, ProviderCaps, ProviderError, ProviderFailure, ReasoningConfig, ReasoningEffort,
    ReasoningSupport, RequestScope, TokenAccountingMode, TokenCountError, TokenCountQuality,
    ToolCallStrategy,
};
pub use request_images::ImageShrinker;
pub use sink::{NullSink, StreamSink};
pub use types::{
    AutonomyLevel, BlastRadius, Block, CompiledContext, Containment, ContentPart, Decision,
    LegacyMessageError, MediaPart, MediaSource, Message, ModelMessage, ObsStatus, Observation,
    OutputReference, Pricing, ProviderState, ReasoningPart, Role, Session, TextPart, ToolCallPart,
    ToolCategory, ToolIntent, ToolNameError, ToolResultPart, ToolSpec, TrustLabel, TurnResult,
    Usage, canonical_tool_names, portable_tool_name, portable_tool_name_map,
};
pub use verify::{NoVerify, Verifier, VerifyReport};
pub use vision::{Substituted, VisionDescriber, describe_media};

mod usage_reporting;
