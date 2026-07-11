//! MEDHA kernel — the only code that calls providers, writes the event log, and
//! enforces budgets (Vol 3 §1). Everything else is a module behind a trait (P8).

pub mod artifacts;
pub mod budgets;
pub mod context;
pub mod errors;
pub mod events;
pub mod executor;
pub mod gate;
pub mod policy;
pub mod provider;
pub mod sink;
pub mod types;
pub mod verify;

#[path = "loop_.rs"]
pub mod kernel_loop;

pub use artifacts::ArtifactStore;
pub use budgets::{Budget, BudgetStop, Governor, DEFAULT_MAX_TURNS};
pub use context::{CompileResult, ContextEngine};
pub use errors::KernelError;
pub use events::{
    cut_index, project_messages, rollback_plan, Event, EventKind, EventLog, FileRollback,
    InMemoryLog, Provenance, SessionMeta,
};
pub use executor::{BackgroundTask, Executor};
pub use gate::{Approval, AutoDeny, HumanGate};
pub use policy::{AllowAll, Policy};
pub use sink::{NullSink, StreamSink};
pub use verify::{NoVerify, VerifyReport, Verifier};
pub use kernel_loop::{Kernel, StopReason, DEFAULT_MAX_PARALLEL_TOOLS};
pub use provider::{
    Provider, ProviderCaps, ProviderError, ReasoningConfig, ReasoningEffort, ToolCallStrategy,
};
pub use types::{
    BlastRadius, Block, CompiledContext, Containment, Decision, Message, ObsStatus, Observation,
    Pricing, Role, Session, ToolCategory, ToolIntent, ToolSpec, TrustLabel, TurnResult, Usage,
};
