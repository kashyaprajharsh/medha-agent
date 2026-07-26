//! MEDHA kernel — the only code that calls providers, writes the event log, and
//! enforces budgets (Vol 3 §1). Everything else is a module behind a trait (P8).

pub mod artifacts;
pub mod budgets;
pub mod clarify;
pub mod context;
pub mod errors;
pub mod events;
pub mod executor;
pub mod gate;
pub mod interrupts;
pub mod policy;
pub mod provider;
pub mod sink;
pub mod types;
pub mod verify;

#[path = "loop_.rs"]
pub mod kernel_loop;

pub use artifacts::ArtifactStore;
pub use budgets::{Budget, BudgetHandle, BudgetStop, DEFAULT_MAX_TURNS, Governor, Pooled};
pub use clarify::{Answer, Asker, NoAsker, QOption, Question};
pub use context::{CompileResult, ContextEngine, DiscoveredContext, ProgressiveContext};
pub use errors::KernelError;
pub use events::{
    Event, EventKind, EventLog, FileRollback, InMemoryLog, Provenance, SessionMeta, cut_index,
    project_messages, project_ordered_messages, rollback_plan,
};
pub use executor::{BackgroundTask, Executor};
pub use gate::{Approval, AutoDeny, HumanGate};
pub use interrupts::{Activity, Interrupt, InterruptHandle, InterruptQueue};
pub use kernel_loop::{DEFAULT_MAX_PARALLEL_TOOLS, Kernel, StopReason};
pub use policy::{AllowAll, Policy};
pub use provider::{
    InputTokenCount, ModelLimits, PreparedModelRequest, Protocol, Provider, ProviderCaps,
    ProviderError, ProviderFailure, ReasoningConfig, ReasoningEffort, ReasoningSupport,
    TokenAccountingMode, TokenCountError, TokenCountQuality, ToolCallStrategy,
};
pub use sink::{NullSink, StreamSink};
pub use types::{
    AutonomyLevel, BlastRadius, Block, CompiledContext, Containment, ContentPart, Decision,
    LegacyMessageError, MediaPart, MediaSource, Message, ModelMessage, ObsStatus, Observation,
    Pricing, ProviderState, ReasoningPart, Role, Session, TextPart, ToolCallPart, ToolCategory,
    ToolIntent, ToolResultPart, ToolSpec, TrustLabel, TurnResult, Usage,
};
pub use verify::{NoVerify, Verifier, VerifyReport};
