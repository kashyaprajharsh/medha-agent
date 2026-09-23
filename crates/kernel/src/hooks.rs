//! Host-owned hook boundary.
//!
//! The kernel supplies redacted event data and consumes only typed narrowing
//! decisions. Process launch, package discovery, and protocol transport live
//! outside the trusted core behind [`HookRunner`].

use crate::TrustLabel;
use async_trait::async_trait;
use medha_extension_api::{HookDecision, HookPoint};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct HookRequest {
    pub event_id: String,
    pub causation_id: Option<String>,
    pub depth: u8,
    pub session_id: String,
    pub point: HookPoint,
    pub trust: TrustLabel,
    pub payload: Value,
}

impl HookRequest {
    pub fn new(
        session_id: impl Into<String>,
        point: HookPoint,
        trust: TrustLabel,
        payload: Value,
    ) -> Self {
        Self {
            event_id: ulid::Ulid::new().to_string(),
            causation_id: None,
            depth: 0,
            session_id: session_id.into(),
            point,
            trust,
            payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Skipped,
}

impl HookStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookAudit {
    pub event_id: String,
    pub plugin_id: String,
    pub component_id: String,
    pub point: HookPoint,
    pub status: HookStatus,
    pub decision: Option<HookDecision>,
    /// Already bounded and sanitized by the runner. Raw process output never
    /// crosses this boundary or enters the durable event log.
    pub reason: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDirective {
    Continue,
    Deny(String),
    RequestApproval(String),
}

/// Text a hook asked the host to show the model, with the component it came
/// from. The kernel logs it as a tool-trust event before any request uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContext {
    pub plugin_id: String,
    pub component_id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct HookBatch {
    pub directive: HookDirective,
    pub audits: Vec<HookAudit>,
    pub contexts: Vec<HookContext>,
    /// Operator-facing annotations; shown by the surface, never sent to the model.
    pub notices: Vec<String>,
}

impl Default for HookBatch {
    fn default() -> Self {
        Self {
            directive: HookDirective::Continue,
            audits: Vec::new(),
            contexts: Vec::new(),
            notices: Vec::new(),
        }
    }
}

#[async_trait]
pub trait HookRunner: Send + Sync {
    async fn invoke(&self, request: &HookRequest, cancel: &CancellationToken) -> HookBatch;

    /// Rereads approved hooks from disk; returns operator-facing warnings.
    fn reload(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct NoHooks;

#[async_trait]
impl HookRunner for NoHooks {
    async fn invoke(&self, _request: &HookRequest, _cancel: &CancellationToken) -> HookBatch {
        HookBatch::default()
    }
}
