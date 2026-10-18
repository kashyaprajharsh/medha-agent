use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn checkpoint_compaction(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        request: &mut RequestState,
        compiled: crate::CompileResult,
        input_limit: Option<u32>,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
    ) -> Flow<()> {
        request.compaction_passes += 1;
        if request.compaction_passes > MAX_COMPACTION_PASSES {
            if let Some(q) = interrupts.as_mut() {
                Self::return_unapplied_steers(q, cx.sink);
            }
            return Ok(ControlFlow::Break((
                std::mem::take(&mut history.messages),
                StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
            )));
        }
        cx.sink.compaction(
            compiled.before_tokens,
            compiled.after_tokens,
            compiled.summarized,
            compiled.summary.as_deref(),
        );
        // Reconcile before logging so the event checkpoints both
        // exact views the next provider request will use. In
        // particular, canonical messages retain opaque protocol
        // replay state that cannot be recovered from `Message`.
        let compacted_ordered = reconcile_ordered(
            &compiled.messages,
            &history.ordered_messages,
            &compiled.source_indices,
        );
        if compiled.summarized || !history.has_checkpoint {
            history.checkpoint_policy = self.context.compaction_policy_version();
        }
        if compiled.summarized {
            history.tool_snapshot = crate::tool_snapshot::ToolSnapshot::refresh(
                Some(&history.tool_snapshot),
                self.executor.as_ref(),
                true,
            )
            .await;
            history.specs = history.tool_snapshot.specs.clone();
            self.context.note_tools(&history.specs);
        }
        let mut checkpoint = Event::compaction_snapshot(
            cx.session,
            compiled.before_tokens,
            compiled.after_tokens,
            compiled.summary.as_deref(),
            &compiled.messages,
            &compacted_ordered,
        );
        if let Some(version) = history.checkpoint_policy {
            checkpoint.payload["policy_version"] = serde_json::json!(version);
        }
        checkpoint.payload["prune_rearm_tokens"] =
            serde_json::json!(self.context.prune_rearm_tokens());
        checkpoint.payload["tools"] = history.tool_snapshot.value();
        self.log.append(checkpoint).await?;
        self.post_compaction_hooks(
            cx.session,
            compiled.before_tokens,
            compiled.after_tokens,
            compiled.summarized,
            cx.cancel,
        )
        .await;
        // The durable log keeps both originals and this canonical
        // checkpoint; the active view is now the only candidate
        // that may be prepared and sent.
        if let Some(summary) = &compiled.summary {
            history.retained_summary = Some(summary.clone());
        }
        history.ordered_messages = compacted_ordered;
        history.messages = compiled.messages;
        history.web_tainted |= history
            .messages
            .iter()
            .any(|message| message.trust == Some(TrustLabel::Web));
        // With no real limit there is no next preflight ceiling to
        // re-check. The synthetic recovery pass must itself prove
        // enough reduction; checkpoint its work, then stop if not.
        if compiled.overflow && input_limit.is_none() {
            if let Some(q) = interrupts.as_mut() {
                Self::return_unapplied_steers(q, cx.sink);
            }
            return Ok(ControlFlow::Break((
                std::mem::take(&mut history.messages),
                StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
            )));
        }
        Ok(ControlFlow::Continue(()))
    }
}
