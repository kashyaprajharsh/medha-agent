//! Kernel call sites for hook points other than tool dispatch.
//!
//! Hooks narrow or add context; they never widen access. Anything a hook adds
//! for the model is appended to the event log first, as tool-trust input.

use super::Kernel;
use crate::errors::KernelError;
use crate::events::{Event, EventLog};
use crate::hooks::{HookBatch, HookContext, HookDirective};
use crate::provider::Provider;
use crate::types::{Message, ModelMessage, Observation, Session, TrustLabel};
use medha_extension_api::HookPoint;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A task-completion hook may send the agent back to work this many times per
/// run; it is the recursion bound for hook-driven continuation.
pub(super) const MAX_HOOK_CONTINUATIONS: u8 = 3;

pub(super) enum PromptGate {
    Proceed(Vec<ulid::Ulid>),
    Blocked(String),
}

impl<P: Provider, L: EventLog> Kernel<P, L> {
    /// Runs once per root session in this process, on its first turn.
    pub(super) async fn session_start_hooks(
        &self,
        session: &Session,
        resumed: bool,
        cancel: &CancellationToken,
        messages: &mut Vec<Message>,
    ) -> Result<Vec<ulid::Ulid>, KernelError> {
        let first = self
            .started_sessions
            .lock()
            .map(|mut started| started.insert(session.id))
            .unwrap_or(false);
        if self.sub_agent || !first {
            return Ok(Vec::new());
        }
        let source = if resumed { "resume" } else { "startup" };
        let batch = self
            .invoke_hook(
                session,
                HookPoint::SessionStart,
                TrustLabel::System,
                json!({ "source": source }),
                cancel,
            )
            .await
            .map_err(KernelError::Log)?;
        self.deliver_contexts(session, HookPoint::SessionStart, &batch, messages, None)
            .await
    }

    /// Gates newly typed prompts before any model call sees them.
    pub(super) async fn prompt_submit_hooks(
        &self,
        session: &Session,
        prompt: &str,
        cancel: &CancellationToken,
        messages: &mut Vec<Message>,
    ) -> Result<PromptGate, KernelError> {
        if self.sub_agent || prompt.trim().is_empty() {
            return Ok(PromptGate::Proceed(Vec::new()));
        }
        let batch = self
            .invoke_hook(
                session,
                HookPoint::PromptSubmit,
                TrustLabel::User,
                json!({ "prompt": super::hook_safe_value(&Value::String(prompt.into())) }),
                cancel,
            )
            .await
            .map_err(KernelError::Log)?;
        match &batch.directive {
            HookDirective::Deny(reason) | HookDirective::RequestApproval(reason) => {
                Ok(PromptGate::Blocked(reason.clone()))
            }
            HookDirective::Continue => Ok(PromptGate::Proceed(
                self.deliver_contexts(session, HookPoint::PromptSubmit, &batch, messages, None)
                    .await?,
            )),
        }
    }

    /// Returns true when a hook sent the agent back to work with feedback.
    pub(super) async fn task_completion_hooks(
        &self,
        session: &Session,
        verified: bool,
        continuations: u8,
        cancel: &CancellationToken,
        messages: &mut Vec<Message>,
        ordered: &mut Vec<ModelMessage>,
    ) -> Result<bool, KernelError> {
        let batch = self
            .invoke_hook(
                session,
                HookPoint::TaskCompletion,
                TrustLabel::System,
                json!({
                    "verified": verified,
                    "verification_required": self.verifier.required(),
                    "continuations": continuations,
                }),
                cancel,
            )
            .await
            .map_err(KernelError::Log)?;
        if batch.contexts.is_empty() || continuations >= MAX_HOOK_CONTINUATIONS {
            return Ok(false);
        }
        self.deliver_contexts(
            session,
            HookPoint::TaskCompletion,
            &batch,
            messages,
            Some(ordered),
        )
        .await?;
        Ok(true)
    }

    pub(super) async fn post_compaction_hooks(
        &self,
        session: &Session,
        before_tokens: u32,
        after_tokens: u32,
        summarized: bool,
        cancel: &CancellationToken,
    ) {
        self.observe_hook_with(
            session,
            HookPoint::PostCompaction,
            json!({
                "before_tokens": before_tokens,
                "after_tokens": after_tokens,
                "summarized": summarized,
            }),
            cancel,
        )
        .await;
    }

    /// Context a hook adds for a starting sub-agent. It joins the child's first
    /// input as tool-trust text, so the child's run logs it before any request.
    pub async fn agent_start_hooks(
        &self,
        session: &Session,
        payload: Value,
        messages: &mut Vec<Message>,
    ) {
        let batch = match self
            .invoke_hook(
                session,
                HookPoint::AgentStart,
                TrustLabel::System,
                payload,
                &CancellationToken::new(),
            )
            .await
        {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!(%error, "agent start hook was not recorded");
                return;
            }
        };
        for context in batch.contexts {
            messages.push(
                Message::user(format!(
                    "[agent_start hook {}/{}] {}",
                    context.plugin_id, context.component_id, context.text
                ))
                .carrying(TrustLabel::Tool),
            );
        }
    }

    /// Observation-only points for surfaces that own the event, such as
    /// sub-agent stop. Decisions other than annotations are ignored.
    pub async fn observe_hook(&self, session: &Session, point: HookPoint, payload: Value) {
        self.observe_hook_with(session, point, payload, &CancellationToken::new())
            .await;
    }

    async fn observe_hook_with(
        &self,
        session: &Session,
        point: HookPoint,
        payload: Value,
        cancel: &CancellationToken,
    ) {
        if let Err(error) = self
            .invoke_hook(session, point, TrustLabel::System, payload, cancel)
            .await
        {
            tracing::warn!(point = point.as_str(), %error, "observer hook was not recorded");
        }
    }

    /// Attaches post-tool and tool-failure context to the observation the model
    /// reads; the caller logs that observation.
    pub(super) fn attach_hook_contexts(observation: &mut Observation, contexts: &[HookContext]) {
        if contexts.is_empty() {
            return;
        }
        let notes: Vec<Value> = contexts
            .iter()
            .map(|context| {
                json!({
                    "source": format!("{}/{}", context.plugin_id, context.component_id),
                    "text": context.text,
                })
            })
            .collect();
        match &mut observation.payload {
            Value::Object(fields) => {
                fields.insert("hook_context".into(), Value::Array(notes));
            }
            other => {
                *other = json!({ "result": other.take(), "hook_context": notes });
            }
        }
    }

    async fn deliver_contexts(
        &self,
        session: &Session,
        point: HookPoint,
        batch: &HookBatch,
        messages: &mut Vec<Message>,
        mut ordered: Option<&mut Vec<ModelMessage>>,
    ) -> Result<Vec<ulid::Ulid>, KernelError> {
        let mut events = Vec::new();
        for context in &batch.contexts {
            let text = format!(
                "[{} hook {}/{}] {}",
                point.as_str(),
                context.plugin_id,
                context.component_id,
                context.text
            );
            let event = self
                .log
                .append(Event::user_input(session, &text, TrustLabel::Tool))
                .await?;
            events.push(event.id);
            let message = Message::user(text).carrying(TrustLabel::Tool);
            if let Some(ordered) = ordered.as_mut() {
                ordered.push(message.ordered());
            }
            messages.push(message);
        }
        Ok(events)
    }
}
