use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn prepare_turn(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        request: &mut RequestState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
        compile_control: &crate::CompileControl,
    ) -> Flow<Prepared> {
        if let Some(refresh) = &self.request_context
            && let Some(index) = history
                .messages
                .iter()
                .position(|message| message.role == Role::System)
        {
            let system = &mut history.messages[index];
            system.content = refresh.refresh(&system.content, cx.sink);
            history.ordered_messages[index] = system.ordered();
        }
        let (prepared, prepared_input_tokens, reserved_output_tokens) = loop {
            let limits = self.provider.model_limits();
            let input_limit = limits
                .input_allowance(self.provider.requested_output_tokens())
                .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32);
            let mut candidate = CompiledContext {
                model: String::new(),
                messages: history.messages.clone(),
                ordered: Some(history.ordered_messages.clone()),
                tools: history.specs.clone(),
            };
            self.resolve_turn_images(cx, state, request, &mut candidate)
                .await?;
            let model_candidate = crate::provenance::for_model(&candidate);
            let mut prepared = self
                .provider
                .prepare_request_in(&model_candidate, cx.request_owner)
                .map_err(|error| KernelError::Provider(error.to_string()))?;
            if state.gov.capped_requests() && self.provider.requested_output_tokens().is_none() {
                // As long as the model allows and the budget can still pay for.
                let prompt = self.context.estimate_prepared(&prepared);
                let prompt = prompt.map_or(0, |count| count.tokens);
                let room = state.gov.output_room(prompt, self.price());
                let cap = crate::budgets::capped_output(limits.max_output_tokens, room);
                prepared = self
                    .provider
                    .with_output_limit(&prepared, cap)
                    .map_err(|error| KernelError::Provider(error.to_string()))?
                    .ok_or_else(|| {
                        KernelError::Provider(
                            "A capped task needs a provider that can bound its output".into(),
                        )
                    })?;
            }

            let request_scope = format!(
                "{}:{}:{}:{limits:?}",
                cx.session.id,
                self.provider.context_identity(),
                prepared.model,
            );
            self.context.begin_request(&request_scope);
            if history.history_scope.as_ref() != Some(&request_scope) {
                self.context
                    .restore_history(&history.logged_events, input_limit);
                history.history_scope = Some(request_scope);
            }
            if !history.restored_usage {
                history.restored_usage = true;
                self.context.restore_usage(&candidate, 0);
                if let Some(anchor) = &history.usage_anchor
                    && let Some(prefix) = anchor.matching_context(
                        self.provider.as_ref(),
                        &candidate,
                        cx.request_owner,
                    )
                {
                    let priced = self
                        .provider
                        .prepare_request_in(
                            &crate::provenance::for_model(&prefix),
                            cx.request_owner,
                        )
                        .map_err(|error| KernelError::Provider(error.to_string()))?;
                    self.context
                        .restore_prepared_usage(&priced, anchor.prompt_tokens);
                }
            }

            if let Some(summary) = &history.retained_summary {
                self.context.restore_summary(summary);
            }

            self.context.clear_preflight();
            let preflight = match self.validated_preflight(&prepared, compile_control).await {
                Ok(count) => count,
                Err(KernelError::Interrupted) => {
                    if let Some(q) = interrupts.as_mut() {
                        return self
                            .finish_interrupted(
                                cx.session,
                                std::mem::take(&mut history.messages),
                                q,
                                cx.sink,
                            )
                            .await
                            .map(ControlFlow::Break);
                    }
                    return Ok(ControlFlow::Break((
                        std::mem::take(&mut history.messages),
                        StopReason::Interrupted,
                    )));
                }
                Err(KernelError::Budget(stop)) => {
                    if let Some(q) = interrupts.as_mut() {
                        Self::return_unapplied_steers(q, cx.sink);
                    }
                    return Ok(ControlFlow::Break((
                        std::mem::take(&mut history.messages),
                        StopReason::Budget(stop),
                    )));
                }
                Err(error) => return Err(error),
            };
            if let Some(count) = &preflight {
                self.context.update_preflight(count);
            }

            if history.check_checkpoint_policy {
                history.check_checkpoint_policy = false;
                if self.context.compaction_policy_version().is_some()
                    && history.checkpoint_policy != self.context.compaction_policy_version()
                    && self
                        .context
                        .checkpoint_needs_refresh(&history.messages, input_limit)
                {
                    self.context.force_next_compaction();
                }
            }

            let compaction_planned = self
                .context
                .compaction_planned(&history.messages, input_limit);
            if compaction_planned {
                self.observe_hook(
                    cx.session,
                    medha_extension_api::HookPoint::PreCompaction,
                    serde_json::json!({
                        "message_count": history.messages.len(),
                        "input_limit": input_limit,
                    }),
                )
                .await;
            }

            if compaction_planned {
                cx.sink.compacting(true);
            }
            let compiled = self
                .context
                .compile_controlled(&history.messages, input_limit, compile_control)
                .await;
            if compaction_planned {
                cx.sink.compacting(false);
            }
            if let Some(failure) = self.context.take_summary_failure() {
                cx.sink.notice(&failure);
                if let Err(error) = self
                    .log
                    .append(Event::summary_failed(cx.session, &failure))
                    .await
                {
                    tracing::warn!(%error, "could not persist the summary failure");
                }
            }
            let compiled = match compiled {
                Ok(compiled) => compiled,
                Err(crate::context::ContextCompileError::Cancelled) => {
                    if let Some(q) = interrupts.as_mut() {
                        return self
                            .finish_interrupted(
                                cx.session,
                                std::mem::take(&mut history.messages),
                                q,
                                cx.sink,
                            )
                            .await
                            .map(ControlFlow::Break);
                    }
                    return Ok(ControlFlow::Break((
                        std::mem::take(&mut history.messages),
                        StopReason::Interrupted,
                    )));
                }
                Err(crate::context::ContextCompileError::Deadline) => {
                    if let Some(q) = interrupts.as_mut() {
                        Self::return_unapplied_steers(q, cx.sink);
                    }
                    return Ok(ControlFlow::Break((
                        std::mem::take(&mut history.messages),
                        StopReason::Budget(crate::budgets::BudgetStop::Wall),
                    )));
                }
                Err(error @ crate::context::ContextCompileError::Summary(_)) => {
                    if let Some(q) = interrupts.as_mut() {
                        Self::return_unapplied_steers(q, cx.sink);
                    }
                    return Err(KernelError::Provider(error.to_string()));
                }
            };
            let pressure = self.context.pressure().unwrap_or_else(|| {
                crate::ContextPressure::new(
                    preflight
                        .as_ref()
                        .map_or(u64::from(compiled.before_tokens), |count| count.tokens),
                    input_limit,
                    preflight
                        .as_ref()
                        .map_or(crate::TokenCountQuality::LocalEstimate, |count| {
                            count.quality
                        }),
                )
            });
            cx.sink.context_pressure(pressure);
            // A compaction result must be checkpointed and recounted,
            // even if the compiler still considers it too large. Never
            // discard useful reductions or send an unrecounted body.
            let counted_overflow = preflight
                .as_ref()
                .zip(input_limit)
                .is_some_and(|(count, limit)| count.tokens >= u64::from(limit));
            if !compiled.compacted && (compiled.overflow || counted_overflow) {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, cx.sink);
                }
                return Ok(ControlFlow::Break((
                    std::mem::take(&mut history.messages),
                    StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                )));
            }
            if !compiled.compacted {
                self.context.note_request(&candidate);
                let input_tokens = preflight.as_ref().map(|count| count.tokens);
                let output_tokens =
                    self.provider
                        .requested_output_tokens()
                        .or_else(|| {
                            let room = state
                                .gov
                                .output_room(input_tokens.unwrap_or(0), self.price());
                            state.gov.capped_requests().then(|| {
                                crate::budgets::capped_output(limits.max_output_tokens, room)
                            })
                        })
                        .or(limits.max_output_tokens)
                        .or_else(|| {
                            limits
                                .max_combined_tokens
                                .zip(input_tokens)
                                .map(|(combined, input)| combined.saturating_sub(input))
                        });
                break (prepared, input_tokens, output_tokens);
            }

            if let ControlFlow::Break(stopped) = self
                .checkpoint_compaction(cx, history, request, compiled, input_limit, interrupts)
                .await?
            {
                return Ok(ControlFlow::Break(stopped));
            }
        };
        Ok(ControlFlow::Continue((
            prepared,
            prepared_input_tokens,
            reserved_output_tokens,
        )))
    }
}
