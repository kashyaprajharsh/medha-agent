use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn recover_turn(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        request: &mut RequestState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
        failed: FailedRequest<'_>,
    ) -> Flow<Recovery> {
        let FailedRequest {
            prepared,
            prepared_input_tokens,
            error,
        } = failed;
        match error {
            KernelError::InvalidToolCall(error)
                if request.tool_call_corrections < MAX_TOOL_CALL_CORRECTIONS =>
            {
                request.tool_call_corrections += 1;
                let tool_call_corrections = request.tool_call_corrections;
                // Keep invalid calls out of canonical history. Only bounded
                // diagnostic data is fed back, never raw model arguments.
                let detail = serde_json::json!({
                    "tool": error.tool.chars().take(128).collect::<String>(),
                    "call_id": error.call_id.chars().take(128).collect::<String>(),
                    "error": error.reason.chars().take(512).collect::<String>(),
                });
                let feedback = format!(
                    "[tool-call validation] Your response contained an invalid tool call: {detail}. \
                             No tools from that response executed. Regenerate all intended calls with \
                             valid tool names and complete JSON objects matching their schemas."
                );
                let mut event = Event::user_input(cx.session, &feedback, TrustLabel::Tool);
                event.payload["tool_call_correction"] = detail;
                self.save_feedback(history, event, feedback).await?;
                cx.sink.notice(&format!(
                            "Invalid tool call. Asking the model to correct it ({tool_call_corrections}/{MAX_TOOL_CALL_CORRECTIONS})…"
                        ));
                // Rebuild and re-count: feedback changes the request body.
                Ok(ControlFlow::Continue(Recovery::Retry))
            }
            // The server's own refusal outranks what a catalogue says of
            // the model in general: this deployment takes no images.
            KernelError::UnsupportedImage
                if request.sending != Sending::AsText
                    && self.provider.image_input_mode() == crate::ImageInputMode::Auto
                    && self.provider.image_support() != crate::ImageSupport::Unsupported =>
            {
                request.sending = Sending::AsText;
                self.provider
                    .observe_image_support(crate::ImageSupport::Unsupported);
                cx.sink.notice(
                    "This model's server says it takes no images. They reach it as text \
                             from now on.",
                );
                Ok(ControlFlow::Continue(Recovery::Retry))
            }
            // Not known to be the images' doing: send less of them each
            // try. Only a request that then passes shows what was wrong.
            e @ (KernelError::ImageSuspected(_) | KernelError::ImageUnfit(_))
                if request.sending != Sending::AsText =>
            {
                request.unfit_named |= matches!(e, KernelError::ImageUnfit(_));
                request.small_refused |= request.sent.one_small();
                let refusal = e.to_string();
                // A side the server names holds for every later request to it.
                let named = crate::request_images::side_limit(&refusal);
                let side = self.image_side.fetch_min(named, Ordering::Relaxed);
                request.sending = request.sending.after_refusal(
                    request.sent,
                    self.shrinker.is_some(),
                    side.min(named),
                );
                request.image_refusal.get_or_insert(refusal);
                Ok(ControlFlow::Continue(Recovery::Retry))
            }
            // A text reply cut off at its limit is kept as the model's turn;
            // whether it is then continued is decided once it is saved.
            // One piece of text repeated until the limit: more of it is all a
            // continuation would bring, so the reply is dropped, not saved.
            KernelError::Truncated(text) if crate::repetition::looping(&text) => {
                Err(KernelError::Provider(crate::cut_off::REPEATING.into()))
            }
            KernelError::Truncated(text) if !text.is_empty() => {
                request.go_on = Some(GoOn::Cut(prepared_input_tokens));
                let tools = prepared.context.tools.iter();
                Ok(ControlFlow::Continue(Recovery::Keep(
                    kept_text(text),
                    tools.map(|spec| spec.name.clone()).collect(),
                )))
            }
            // What was written before the connection died is the model's
            // turn too: kept, and continued from, not paid for again.
            KernelError::Dropped(text) => {
                request.go_on = Some(GoOn::Dropped(prepared_input_tokens));
                let tools = prepared.context.tools.iter();
                Ok(ControlFlow::Continue(Recovery::Keep(
                    kept_text(text),
                    tools.map(|spec| spec.name.clone()).collect(),
                )))
            }
            KernelError::Truncated(_) => Err(KernelError::Provider(
                crate::cut_off::NOTHING_WRITTEN.into(),
            )),
            // The text before the half-written call is kept as the model's turn,
            // and the model is then told to do the work in smaller calls.
            KernelError::CallStalled { text, tools, said } => {
                if state.cut_offs + 1 >= crate::cut_off::ROUNDS {
                    return Err(KernelError::Provider(said));
                }
                request.go_on = Some(GoOn::Stalled(tools));
                let tools = prepared.context.tools.iter();
                Ok(ControlFlow::Continue(Recovery::Keep(
                    kept_text(text),
                    tools.map(|spec| spec.name.clone()).collect(),
                )))
            }
            // The request is rebuilt for the next model the user listed;
            // with none left, the provider's own error is the answer.
            KernelError::ModelUnavailable {
                model,
                why,
                said,
                shown,
                rest,
                back,
            } => {
                let Some((was, now)) = self.provider.fall_back(&model, rest) else {
                    // Nobody to hand over to, and the server is only away
                    // for now: wait for it, a few times, then try again.
                    let Some(asked) = back.filter(|_| state.waits < self.server_waits) else {
                        return Err(KernelError::Provider(said));
                    };
                    state.waits += 1;
                    let waits = state.waits;
                    let wait = crate::recovery::wait_for_server(state.waits, asked, jitter_nanos());
                    if shown {
                        cx.sink.restarted();
                    }
                    cx.sink.notice(&format!(
                        "The model's server is still not answering. Trying again in \
                                 {}s ({waits}/{}). Stop the turn to give up.",
                        wait.as_secs(),
                        self.server_waits
                    ));
                    let heard = interrupts.as_ref().map(|q| q.activity());
                    match waited_out(wait, cx.cancel, heard, cx.sink).await {
                        Waited::Out => return Ok(ControlFlow::Continue(Recovery::Retry)),
                        // Back to where a typed message is taken in.
                        Waited::Spoken => return Ok(ControlFlow::Continue(Recovery::NextTurn)),
                        Waited::Stopped => {}
                    }
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
                };
                // What the failed model had begun to show is taken back.
                if shown {
                    cx.sink.restarted();
                }
                let again = match rest {
                    crate::recovery::Rest::NextMessage => "your next message tries",
                    crate::recovery::Rest::Limited(_) => {
                        "once its limit has had time to lift, a message tries"
                    }
                };
                cx.sink.notice(&format!(
                    "{was} could not be used: {why}. Using {now} for the rest of this \
                             turn; {again} {was} again."
                ));
                state.stood_in = Some((was, now));
                Ok(ControlFlow::Continue(Recovery::Retry))
            }
            KernelError::ContextOverflow { reported_limit } if !request.overflow_retried => {
                request.overflow_retried = true;
                if let Some(limit) = reported_limit {
                    self.provider.update_context_limit(limit);
                }
                self.context.force_next_compaction();
                Ok(ControlFlow::Continue(Recovery::Retry))
            }
            KernelError::ContextOverflow { .. } => {
                // Already retried once and still over — stop gracefully.
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, cx.sink);
                }
                Ok(ControlFlow::Break((
                    std::mem::take(&mut history.messages),
                    StopReason::Budget(crate::budgets::BudgetStop::ContextOverflow),
                )))
            }
            KernelError::Interrupted => {
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
                Ok(ControlFlow::Break((
                    std::mem::take(&mut history.messages),
                    StopReason::Interrupted,
                )))
            }
            KernelError::Budget(stop) => {
                if matches!(
                    stop,
                    crate::budgets::BudgetStop::Cost | crate::budgets::BudgetStop::Tokens
                ) {
                    self.finish_budget(
                        cx.session,
                        &mut history.messages,
                        &mut state.gov,
                        cx.sink,
                        cx.cancel,
                    )
                    .await?;
                }
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, cx.sink);
                }
                Ok(ControlFlow::Break((
                    std::mem::take(&mut history.messages),
                    StopReason::Budget(stop),
                )))
            }
            // Refused without its images too: they were not why, so the
            // first refusal is the one to report, and nothing is learned.
            e => Err(match request.image_refusal.take() {
                Some(refusal) => KernelError::Provider(refusal),
                None => e,
            }),
        }
    }

    pub(super) fn accept_request(
        &self,
        cx: &RunContext<'_>,
        state: &mut RunState,
        request: &mut RequestState,
        prepared: &crate::PreparedModelRequest,
        t: Turn,
    ) -> (Turn, Vec<String>) {
        if request.image_refusal.take().is_some() {
            state.image_floor = request.sending;
            match request.sending {
                // Size and damage ruled out: it takes no images.
                Sending::AsText if request.small_refused && !request.unfit_named => {
                    self.provider
                        .observe_image_support(crate::ImageSupport::Unsupported);
                    cx.sink.notice(
                        "This model's server refused a request with one small \
                                         image and accepted it without one. Images are left out \
                                         for this model from now on.",
                    );
                }
                Sending::AsText => cx.sink.notice(
                    "The server refused this request's images, so they are left \
                                     out for the rest of this turn. The model was told.",
                ),
                Sending::OneSmall => cx.sink.notice(
                    "The server refused this request's images. Only the newest \
                                     is sent, made small, for the rest of this turn.",
                ),
                Sending::Shrunk => {
                    self.shrinks_images.store(true, Ordering::Relaxed);
                    cx.sink.notice(
                        "The server refused the images at full size. Large \
                                         images are sent smaller from now on in this chat.",
                    );
                }
                Sending::Whole => {}
            }
        }
        if context_has_media(&prepared.context) {
            if self.provider.image_input_mode() == crate::ImageInputMode::Auto {
                self.provider
                    .observe_image_support(crate::ImageSupport::Supported);
            }
            if state.new_image_submission && !state.image_route_reported {
                cx.sink
                    .notice("attached image sent natively to the selected model");
                state.image_route_reported = true;
            }
        }
        // Validate against the catalogue of the request that produced
        // this response, not a fresh (possibly changed) executor list.
        let tools: Vec<String> = prepared
            .context
            .tools
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        (t, tools)
    }
}
