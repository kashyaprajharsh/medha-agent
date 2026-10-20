use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    /// One turn: stream the model and collect a legacy control view plus the exact
    /// canonical assistant message. Transient failures retry with capped backoff;
    /// after output, only a sink that can explicitly rewind is retried. Context
    /// overflow is surfaced to the caller to compact. Other errors are fatal.
    #[allow(clippy::too_many_arguments)]
    #[allow(
        clippy::too_many_lines,
        reason = "Existing retry state machine; 424 Clippy lines, shrink only."
    )]
    pub(super) async fn run_turn(
        &self,
        session: &Session,
        prepared: &crate::provider::PreparedModelRequest,
        mut prepared_input_tokens: Option<u64>,
        mut reserved_output_tokens: Option<u64>,
        governor: &mut crate::budgets::Governor,
        sink: &dyn StreamSink,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        join: &mut crate::seam::Join,
    ) -> Result<
        (
            Message,
            ModelMessage,
            Vec<ToolIntent>,
            Option<crate::types::Usage>,
            bool,
        ),
        KernelError,
    > {
        let mut attempt = 0u32;
        let mut stalls = 0u32;
        let mut cut_call_retries = 0u32;
        let (mut thinking_off_tried, mut output_limit_retried) = (false, false);
        let (mut reasoning_reset, mut schemas_plain, mut reasoning_stripped) =
            (false, false, false);
        // A rate-limited key was already given its one more try.
        let mut limit_retried = false;
        let mut request = prepared.clone();
        // The model does not change inside one call, so neither does its price.
        let price = self.price();
        if governor.requires_pricing() && price.is_none() {
            return Err(KernelError::Provider("max_cost_usd requires input and output prices for the selected model; configure its pricing before running a cost-capped task".into()));
        }
        let (text, reasoning, intents, canonical, usage, interrupted) = loop {
            let reservation = governor
                .reserve_model(prepared_input_tokens, reserved_output_tokens, price)
                .map_err(KernelError::Budget)?;
            sink.phase(crate::progress::Phase::Generating);
            self.observe_hook_with(
                session,
                medha_extension_api::HookPoint::PreModel,
                serde_json::json!({"model": &request.model, "attempt": attempt}),
                cancel,
            )
            .await;
            let mut cut = crate::cut_off::CutOff::default();
            let mut attempt_usage = None;
            let outcome = self
                .stream_turn(
                    &request,
                    sink,
                    cancel,
                    wall_deadline,
                    &mut cut,
                    join,
                    &mut attempt_usage,
                )
                .await;
            self.observe_model_completion(
                session,
                &request.model,
                attempt,
                outcome.as_ref().map(|data| data.5).ok(),
                cancel,
            )
            .await;
            let (usage, error) = match &outcome {
                Ok(data) => (data.4, None),
                Err((error, _)) => (attempt_usage, Some(error)),
            };
            reservation
                .reconcile_attempt(usage, price, error)
                .map_err(KernelError::Budget)?;
            match outcome {
                Ok(data) => break data,
                Err((e, emitted)) => {
                    if matches!(e, crate::provider::ProviderError::Truncated) {
                        if !cut.tool_call {
                            // Thinking used the whole allowance and nothing was
                            // written: asked once more, this time without thinking.
                            let quiet = crate::provider::ReasoningConfig {
                                enabled: Some(false),
                                effort: None,
                            };
                            let again = (cut.text.is_empty()
                                && !thinking_off_tried
                                && (!emitted || sink.supports_restart()))
                            .then(|| self.provider.with_request_reasoning(&request, &quiet));
                            if let Some(Ok(Some(adjusted))) = again {
                                thinking_off_tried = true;
                                let control = crate::context::CompileControl::new(
                                    cancel.clone(),
                                    wall_deadline,
                                );
                                let recount = self.validated_preflight(&adjusted, &control).await?;
                                prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                                request = adjusted;
                                if emitted {
                                    sink.restarted();
                                }
                                sink.notice(
                                    "The model used its whole output allowance on thinking and \
                                     wrote nothing. Asking once more with thinking off…",
                                );
                                continue;
                            }
                            return Err(KernelError::Truncated(cut.text));
                        }
                        // The same call again with more room; a screen that
                        // cannot take back what it showed is not sent it twice.
                        let limits = self.provider.model_limits();
                        let room = crate::cut_off::more_room(
                            self.provider.requested_output_tokens(),
                            reserved_output_tokens,
                            cut_call_retries + 1,
                            limits.max_output_tokens,
                        );
                        let retry = cut_call_retries < crate::cut_off::ROUNDS
                            && (!emitted || sink.supports_restart());
                        let adjusted = match retry {
                            true => self
                                .provider
                                .with_output_limit(&request, room)
                                .map_err(|error| KernelError::Provider(error.to_string()))?,
                            false => None,
                        };
                        let Some(adjusted) = adjusted else {
                            return Err(KernelError::Provider(
                                crate::cut_off::CALL_STAYED_CUT.into(),
                            ));
                        };
                        cut_call_retries += 1;
                        let control =
                            crate::context::CompileControl::new(cancel.clone(), wall_deadline);
                        let recount = self.validated_preflight(&adjusted, &control).await?;
                        prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                        reserved_output_tokens = Some(room);
                        request = adjusted;
                        if emitted {
                            sink.restarted();
                        }
                        sink.notice(&format!(
                            "The model's tool call was cut off at its output limit. Retrying \
                             with more room ({cut_call_retries}/{})…",
                            crate::cut_off::ROUNDS
                        ));
                        continue;
                    }
                    match e.classify() {
                        crate::provider::ProviderFailure::InputContextOverflow {
                            reported_limit,
                        } => {
                            return Err(KernelError::ContextOverflow { reported_limit });
                        }
                        crate::provider::ProviderFailure::OutputLimit {
                            available_output: Some(available),
                        } if !emitted && !output_limit_retried && available > 0 => {
                            // Keep a tiny margin for providers whose diagnostic
                            // value is inclusive/rounded. This changes only the
                            // failed call's output cap; history is untouched.
                            let safe = available.saturating_sub(64).max(1);
                            if let Some(adjusted) = self
                                .provider
                                .with_output_limit(&request, safe)
                                .map_err(|error| KernelError::Provider(error.to_string()))?
                            {
                                // `max_tokens` participates in the request
                                // fingerprint. Re-count the adjusted request so
                                // strict mode never sends a body different from
                                // the one its counter authorized.
                                let control = crate::context::CompileControl::new(
                                    cancel.clone(),
                                    wall_deadline,
                                );
                                let adjusted_count =
                                    self.validated_preflight(&adjusted, &control).await?;
                                prepared_input_tokens =
                                    adjusted_count.as_ref().map(|count| count.tokens);
                                reserved_output_tokens = Some(safe);
                                request = adjusted;
                                output_limit_retried = true;
                                continue;
                            }
                        }
                        crate::provider::ProviderFailure::OutputLimit { .. } => {
                            return Err(KernelError::Provider(format!(
                                "the provider rejected the output-token allowance; check the profile's max_output_tokens. Original rejection: {e}"
                            )));
                        }
                        // With images aboard, they are the first thing to send less of.
                        crate::provider::ProviderFailure::PayloadTooLarge
                        | crate::provider::ProviderFailure::ImageUnfit
                            if !emitted && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::ImageUnfit(e.to_string()));
                        }
                        crate::provider::ProviderFailure::ImageUnfit => {}
                        crate::provider::ProviderFailure::PayloadTooLarge => {
                            return Err(KernelError::Provider(
                                "the provider rejected the HTTP payload size; reduce retained media or byte-heavy tool results"
                                    .into(),
                            ));
                        }
                        crate::provider::ProviderFailure::UnsupportedImage
                            if !emitted
                                && self.provider.image_input_mode()
                                    == crate::ImageInputMode::Auto
                                && self.provider.image_support()
                                    != crate::ImageSupport::Unsupported
                                && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::UnsupportedImage);
                        }
                        crate::provider::ProviderFailure::UnsupportedImage => {}
                        // Refusals are worded too many ways to list: test it without the image.
                        crate::provider::ProviderFailure::Fatal
                            if !emitted
                                && matches!(
                                    e,
                                    crate::provider::ProviderError::Status(400 | 415 | 422, _)
                                )
                                && self.provider.image_input_mode()
                                    == crate::ImageInputMode::Auto
                                && self.provider.image_support()
                                    != crate::ImageSupport::Unsupported
                                && context_has_media(&request.context) =>
                        {
                            return Err(KernelError::ImageSuspected(e.to_string()));
                        }
                        crate::provider::ProviderFailure::InvalidToolCall => {
                            if emitted && sink.supports_restart() {
                                sink.restarted();
                            }
                            if let crate::provider::ProviderError::InvalidToolCall(error) = e {
                                return Err(KernelError::InvalidToolCall(error));
                            }
                        }
                        crate::provider::ProviderFailure::Transient
                        | crate::provider::ProviderFailure::Fatal => {}
                    }
                    // A setting or a tool description this server will not take
                    // is left out, once, and stays out for the rest of the chat.
                    let adjusted = match e.recovery().first {
                        crate::recovery::First::DefaultReasoning if !reasoning_reset => {
                            reasoning_reset = true;
                            let own = crate::provider::ReasoningConfig::default();
                            let adjusted = self.provider.with_request_reasoning(&request, &own);
                            adjusted.ok().flatten().inspect(|_| {
                                let _ = self.provider.set_reasoning(own);
                                sink.notice(
                                    "This server refused the reasoning setting. Using its own \
                                     default for the rest of this chat…",
                                );
                            })
                        }
                        crate::recovery::First::PlainSchemas if !schemas_plain => {
                            schemas_plain = true;
                            let adjusted = self.provider.with_plain_schemas(&request);
                            adjusted.ok().flatten().inspect(|_| {
                                sink.notice(
                                    "This server could not use the pattern rules in the tool \
                                     descriptions. Sending them without those rules…",
                                );
                            })
                        }
                        crate::recovery::First::StaleReasoning if !reasoning_stripped => {
                            reasoning_stripped = true;
                            let adjusted = self.provider.without_replayed_reasoning(&request);
                            adjusted.ok().flatten().inspect(|_| {
                                sink.notice(
                                    "This server could not open earlier reasoning from this chat. \
                                     Sending without it…",
                                );
                            })
                        }
                        _ => None,
                    };
                    if let Some(adjusted) = adjusted.filter(|_| !emitted) {
                        let control =
                            crate::context::CompileControl::new(cancel.clone(), wall_deadline);
                        let recount = self.validated_preflight(&adjusted, &control).await?;
                        prepared_input_tokens = recount.as_ref().map(|count| count.tokens);
                        request = adjusted;
                        continue;
                    }
                    // A stream that died with text already on screen: the text is
                    // kept and continued from, never sent for again from the start.
                    if e.is_retryable() && !cut.text.trim().is_empty() {
                        if !cut.tool_call {
                            return Err(KernelError::Dropped(cut.text));
                        }
                        // A half-written call cannot stay on screen; the text can.
                        if sink.supports_restart() {
                            sink.restarted();
                            sink.text(&cut.text);
                            return Err(KernelError::CallStalled {
                                text: cut.text,
                                tools: cut.tools,
                                said: e.to_string(),
                            });
                        }
                    }
                    // Stalls have their own, smaller allowance inside the
                    // turn's overall one, so earlier dropped connections
                    // neither use it up nor are counted as stalls.
                    let stalled = e.is_stream_stall();
                    // A key the server turned away is swapped for the next saved
                    // one before another model is thought of. A rate limit is
                    // first given one more try on the same key.
                    let said = match &e {
                        crate::provider::ProviderError::Status(_, said)
                        | crate::provider::ProviderError::Throttled { body: said, .. } => {
                            said.to_ascii_lowercase()
                        }
                        _ => String::new(),
                    };
                    let key_fault = match &e {
                        crate::provider::ProviderError::Status(status, _)
                        | crate::provider::ProviderError::Throttled { status, .. }
                            if matches!(status, 401 | 402 | 403 | 429) && !emitted =>
                        {
                            // A limit further upstream, or a server overloaded for
                            // everyone, is not the key's: resting it would cost a good key.
                            let healthy = match status {
                                429 => crate::rejections::not_the_keys_limit(&said),
                                // A block page in front of the server never saw the key.
                                403 => crate::rejections::blocked_on_the_way(&said),
                                _ => false,
                            };
                            (!healthy).then_some(*status)
                        }
                        _ => None,
                    };
                    let once_more = key_fault == Some(429)
                        && !limit_retried
                        && !crate::rejections::plan_limit(&said)
                        && self.provider.another_key();
                    let swapped = key_fault.filter(|_| !once_more).and_then(|status| {
                        Some((status, self.provider.next_key(status, e.retry_after())?))
                    });
                    if let Some((status, (at, of))) = swapped {
                        limit_retried = false;
                        let why = match status {
                            402 => "has no credit",
                            429 => "is rate limited",
                            _ => "was refused",
                        };
                        sink.notice(&format!(
                            "The key in use {why}. Using your next key for this server \
                             ({at} of {of})…"
                        ));
                        continue;
                    }
                    limit_retried |= once_more;
                    // With another model listed, a rate limit is not waited out
                    // and a server that cannot be reached is tried only twice more.
                    let patience = match e.recovery().hand_over_after {
                        _ if once_more => attempt + 1,
                        Some(tries) if self.provider.stand_in_left() => tries,
                        _ => MAX_TURN_RETRIES,
                    };
                    // Some failures are given fewer tries than the rest, the first counted.
                    let most = e.recovery().tries.map_or(patience, |tries| tries - 1);
                    let patience = patience.min(most);
                    if e.is_retryable()
                        && attempt < patience.min(MAX_TURN_RETRIES)
                        && (!stalled || stalls < MAX_STALL_RETRIES)
                    {
                        attempt += 1;
                        stalls += u32::from(stalled);
                        // A stream that died after streaming part of a reply is
                        // the common shape of a transient failure, so refusing
                        // to retry once anything was emitted would strand
                        // exactly the turns most worth saving. The sink drops
                        // its partial render instead, and the reply arrives once.
                        if emitted {
                            if !sink.supports_restart() {
                                return Err(KernelError::Provider(format!(
                                    "{e}; the stream stopped after partial output and this output surface cannot rewind it safely"
                                )));
                            }
                            sink.restarted();
                        }
                        // A retry is progress, not silence. Without this an
                        // inactivity bound would kill the turns that are
                        // recovering from exactly the stall it exists to catch.
                        sink.phase_ticked();
                        // The provider's own number when it gave one; retrying
                        // sooner than asked just earns another refusal.
                        let wait = e.retry_after().unwrap_or_else(|| retry_backoff(attempt));
                        sink.notice(&if stalled {
                            // Says what happened and what Medha is doing about it.
                            let said = e.to_string();
                            let said = said.strip_prefix("the model").unwrap_or(&said);
                            format!("The model{said}. Reconnecting ({stalls}/{MAX_STALL_RETRIES})…")
                        } else {
                            format!(
                                "Model request failed. Retrying ({attempt}/{MAX_TURN_RETRIES})…"
                            )
                        });
                        // The backoff nap races the cancel token too — Esc
                        // during a retry wait must stop the turn, not queue
                        // another attempt.
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => continue,
                            _ = cancel.cancelled() => {
                                break (
                                    String::new(),
                                    String::new(),
                                    Vec::new(),
                                    message_from_stream_parts(Vec::new()),
                                    None,
                                    true,
                                );
                            }
                            _ = async {
                                if let Some(deadline) = wall_deadline {
                                    tokio::time::sleep_until(deadline).await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                            } => {
                                break (
                                    String::new(),
                                    String::new(),
                                    Vec::new(),
                                    message_from_stream_parts(Vec::new()),
                                    None,
                                    true,
                                );
                            }
                        }
                    }
                    let said = match stalled && stalls > 0 {
                        true => format!(
                            "{e}, {} times this turn; Medha stopped reconnecting",
                            stalls + 1
                        ),
                        false => e.to_string(),
                    };
                    // A block under the provider's content rules is said in plain words.
                    let declined = e.recovery().stand_in == Some("it declined to answer");
                    let said = match declined {
                        true => format!("{} Its words: {said}", crate::rejections::BLOCKED),
                        false => said,
                    };
                    // A stream that keeps dying under one tool call: the screen drops
                    // the half-written call, and the model is told to send less at once.
                    if e.is_retryable() && cut.tool_call && sink.supports_restart() {
                        sink.restarted();
                        if !cut.text.is_empty() {
                            sink.text(&cut.text);
                        }
                        return Err(KernelError::CallStalled {
                            text: cut.text,
                            tools: cut.tools,
                            said,
                        });
                    }
                    // No retry is left, or none would help: another model may answer.
                    let stand_in = e.recovery().stand_in;
                    if let Some(why) = stand_in.filter(|_| !emitted || sink.supports_restart()) {
                        return Err(KernelError::ModelUnavailable {
                            model: request.model.clone(),
                            why,
                            said,
                            shown: emitted,
                            rest: e.rest(),
                            back: e.recovery().comes_back.then(|| e.retry_after()),
                        });
                    }
                    return Err(KernelError::Provider(said));
                }
            }
        };
        // `run_session` appends the canonical message only after dispatch admission.
        if !reasoning.is_empty() {
            self.log
                .append(Event::model_reasoning(session, &reasoning))
                .await?;
        }
        if let Some(usage) = usage.filter(|usage| usage.prompt_tokens > 0) {
            let anchor = crate::usage_reporting::UsageAnchor::capture(
                self.provider.context_identity(),
                &request,
                &usage,
                price,
            );
            if let Err(error) = self
                .log
                .append(Event::context_usage(session, &anchor))
                .await
            {
                tracing::warn!(%error, "could not persist context usage calibration");
            }
        }
        Ok((
            Message::assistant_calls(text, intents.clone()),
            canonical,
            intents,
            usage,
            interrupted,
        ))
    }

    async fn observe_model_completion(
        &self,
        session: &Session,
        model: &str,
        attempt: u32,
        interrupted: Option<bool>,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        self.observe_hook_with(
            session,
            medha_extension_api::HookPoint::PostModel,
            serde_json::json!({
                "model": model,
                "attempt": attempt,
                "outcome": match interrupted {
                    Some(true) => "interrupted",
                    Some(false) => "completed",
                    None => "failed",
                },
            }),
            cancel,
        )
        .await;
    }

    /// Establish and consume one model stream, emitting deltas to the sink as
    /// they arrive. Returns the compatibility view and exact completed message.
    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_turn(
        &self,
        prepared: &crate::provider::PreparedModelRequest,
        sink: &dyn StreamSink,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        cut: &mut crate::cut_off::CutOff,
        join: &mut crate::seam::Join,
        attempt_usage: &mut Option<crate::Usage>,
    ) -> Result<
        (
            String,
            String,
            Vec<ToolIntent>,
            ModelMessage,
            Option<crate::types::Usage>,
            bool,
        ),
        (crate::provider::ProviderError, bool),
    > {
        // A reply that goes on from an earlier piece has its start held back
        // until it is known not to repeat that piece's end.
        join.cut = 0;
        let mut seam = (!join.written.is_empty()).then(|| crate::seam::Seam::after(&join.written));
        // Connection and prompt processing must remain cancellable before the
        // first stream byte arrives.
        let request_started = std::time::Instant::now();
        let mut usage_report = crate::usage_reporting::AttemptUsage::new(sink, prepared);
        let mut stream = tokio::select! {
            s = self.provider.stream_prepared(prepared) => s.map_err(|e| (e, false))?,
            _ = cancel.cancelled() => {
                return Ok((
                    String::new(),
                    String::new(),
                    Vec::new(),
                    message_from_stream_parts(Vec::new()),
                    None,
                    true,
                ));
            }
            _ = wait_for_deadline(wall_deadline) => {
                return Ok((
                    String::new(),
                    String::new(),
                    Vec::new(),
                    message_from_stream_parts(Vec::new()),
                    None,
                    true,
                ));
            }
        };
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut intents: Vec<ToolIntent> = Vec::new();
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut completed: Option<ModelMessage> = None;
        let mut usage: Option<crate::types::Usage> = None;
        let mut emitted = false;
        let mut tool_started = false;
        // Names of the tool calls the model has begun, for advice if the stream dies.
        let mut called: Vec<String> = Vec::new();
        let mut stream_blocks = 0usize;
        let mut stream_bytes = 0usize;
        // Calls whose arguments have started to arrive. When the first piece
        // lands shows whether a provider streams a call or sends it whole.
        let mut arguments_started = std::collections::HashSet::new();
        loop {
            let block = tokio::select! {
                block = stream.next() => block,
                _ = cancel.cancelled() => {
                    // Cancelled mid-stream: keep what streamed (the user saw
                    // it), drop un-dispatched intents — they were never
                    // admitted, so nothing in the log dangles.
                    let mut canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
                    only_shown(&seam, &mut text, &mut canonical);
                    return Ok((text, reasoning, Vec::new(), canonical, usage, true));
                }
                _ = wait_for_deadline(wall_deadline) => {
                    let mut canonical = completed
                        .as_ref()
                        .map(strip_tool_calls)
                        .unwrap_or_else(|| strip_tool_calls(&message_from_stream_parts(parts)));
                    only_shown(&seam, &mut text, &mut canonical);
                    return Ok((text, reasoning, Vec::new(), canonical, usage, true));
                }
            };
            let Some(block) = block else { break };
            // Every block is proof the connection is alive. Without this a slow
            // reply and a dead one share one phase and one clock.
            sink.phase_ticked();
            stream_blocks = stream_blocks.saturating_add(1);
            if stream_blocks > MAX_PROVIDER_STREAM_BLOCKS {
                return Err((
                    crate::provider::ProviderError::Decode(format!(
                        "provider stream exceeded the {} block kernel limit",
                        MAX_PROVIDER_STREAM_BLOCKS
                    )),
                    emitted,
                ));
            }
            match block {
                Ok(Block::Text(t)) => {
                    if text.is_empty() && !t.is_empty() {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            "model first text delta"
                        );
                    }
                    charge_stream_bytes(&mut stream_bytes, t.len())
                        .map_err(|error| (error, emitted))?;
                    match seam.as_mut().map(|seam| seam.feed(&t)) {
                        // Still held: nothing has reached the screen yet.
                        Some(shown) if shown.is_empty() => {}
                        Some(shown) => {
                            emitted = true;
                            sink.text(&shown);
                        }
                        None => {
                            emitted = true;
                            sink.text(&t);
                        }
                    }
                    text.push_str(&t);
                    match parts.last_mut() {
                        Some(ContentPart::Text(part)) => part.text.push_str(&t),
                        _ => parts.push(ContentPart::Text(TextPart {
                            text: t,
                            provider_state: Vec::new(),
                        })),
                    }
                }
                Ok(Block::Reasoning(r)) => {
                    if reasoning.is_empty() && !r.is_empty() {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            "model first reasoning delta"
                        );
                    }
                    charge_stream_bytes(&mut stream_bytes, r.len())
                        .map_err(|error| (error, emitted))?;
                    emitted = true;
                    sink.reasoning(&r);
                    reasoning.push_str(&r);
                    match parts.last_mut() {
                        Some(ContentPart::Reasoning(part)) => {
                            part.text.get_or_insert_with(String::new).push_str(&r)
                        }
                        _ => parts.push(ContentPart::Reasoning(ReasoningPart {
                            text: Some(r),
                            provider_state: Vec::new(),
                        })),
                    }
                }
                Ok(Block::ToolStarted { name, target }) => {
                    emitted = true;
                    tool_started = true;
                    if !called.contains(&name) {
                        called.push(name.clone());
                    }
                    sink.tool_started(&name, target.as_deref());
                }
                // Passed straight on and kept nowhere: the finished call below is
                // what is counted against the stream's size, admitted and logged.
                Ok(Block::ToolInput { id, name, delta }) => {
                    if arguments_started.len() < MAX_TOOL_INTENTS_PER_TURN
                        && arguments_started.insert(id.clone())
                    {
                        tracing::info!(
                            elapsed_ms = request_started.elapsed().as_millis() as u64,
                            tool = %name,
                            "model first tool argument delta"
                        );
                    }
                    emitted = true;
                    sink.tool_input(&id, &name, &delta);
                }
                Ok(Block::ToolIntent(it)) => {
                    if intents.len() >= MAX_TOOL_INTENTS_PER_TURN {
                        return Err((
                            crate::provider::ProviderError::Decode(format!(
                                "provider emitted more than {} tool calls in one turn",
                                MAX_TOOL_INTENTS_PER_TURN
                            )),
                            emitted,
                        ));
                    }
                    let intent_bytes = serde_json::to_vec(&it)
                        .map_err(|error| {
                            (
                                crate::provider::ProviderError::Decode(error.to_string()),
                                emitted,
                            )
                        })?
                        .len();
                    charge_stream_bytes(&mut stream_bytes, intent_bytes)
                        .map_err(|error| (error, emitted))?;
                    emitted = true;
                    parts.push(ContentPart::ToolCall(ToolCallPart {
                        id: it.id.clone(),
                        tool: it.tool.clone(),
                        args: it.args.clone(),
                        provider_state: Vec::new(),
                    }));
                    intents.push(it);
                }
                Ok(Block::Usage(u)) => {
                    usage_report.observe(u);
                    usage = Some(u);
                    *attempt_usage = Some(u);
                }
                Ok(Block::CompletedMessage(message)) => {
                    if completed.is_some() {
                        return Err((
                            crate::provider::ProviderError::Decode(
                                "provider emitted more than one completed message".into(),
                            ),
                            emitted,
                        ));
                    }
                    if let Err(error) = completed_control_view(&message) {
                        return Err((error, emitted));
                    }
                    let message_bytes = serde_json::to_vec(&message)
                        .map_err(|error| {
                            (
                                crate::provider::ProviderError::Decode(error.to_string()),
                                emitted,
                            )
                        })?
                        .len();
                    charge_stream_bytes(&mut stream_bytes, message_bytes)
                        .map_err(|error| (error, emitted))?;
                    completed = Some(message);
                }
                Err(e) => {
                    // What a reply had written when it stopped decides how it is recovered.
                    cut.tool_call =
                        tool_started || !intents.is_empty() || !arguments_started.is_empty();
                    cut.text = std::mem::take(&mut text);
                    // A start that was held or cut was never shown, and is not kept.
                    if let Some(seam) = &seam {
                        crate::seam::without_start(&mut cut.text, seam.hidden());
                    }
                    cut.tools = std::mem::take(&mut called);
                    return Err((e, emitted));
                }
            }
        }
        let mut canonical = completed.unwrap_or_else(|| message_from_stream_parts(parts));
        let (canonical_text, canonical_reasoning, canonical_intents) =
            completed_control_view(&canonical).map_err(|error| (error, emitted))?;
        if canonical_intents.len() > MAX_TOOL_INTENTS_PER_TURN {
            return Err((
                crate::provider::ProviderError::Decode(format!(
                    "provider completed message contained more than {} tool calls",
                    MAX_TOOL_INTENTS_PER_TURN
                )),
                emitted,
            ));
        }
        if !text.is_empty() && text != canonical_text {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider text deltas disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        if !reasoning.is_empty() && reasoning != canonical_reasoning {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider reasoning deltas disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        let intents_match = intents.len() == canonical_intents.len()
            && intents.iter().zip(&canonical_intents).all(|(left, right)| {
                left.id == right.id && left.tool == right.tool && left.args == right.args
            });
        if !intents.is_empty() && !intents_match {
            return Err((
                crate::provider::ProviderError::Decode(
                    "provider tool-call blocks disagree with its completed message".into(),
                ),
                emitted,
            ));
        }
        if text.is_empty() && !canonical_text.is_empty() {
            // A reply that arrived whole is read for a repeat like any other.
            match seam.as_mut().map(|seam| seam.feed(&canonical_text)) {
                Some(shown) if shown.is_empty() => {}
                Some(shown) => sink.text(&shown),
                None => sink.text(&canonical_text),
            }
            text = canonical_text;
        }
        // The reply is over: what was held is shown, less a repeat, and the
        // repeat is left out of what is saved as well.
        if let Some(seam) = seam.as_mut() {
            let held = seam.finish();
            if !held.is_empty() {
                sink.text(&held);
            }
            join.cut = seam.cut;
            crate::seam::without_start(&mut text, seam.cut);
            crate::seam::without_start_of(&mut canonical, seam.cut);
        }
        if reasoning.is_empty() && !canonical_reasoning.is_empty() {
            sink.reasoning(&canonical_reasoning);
            reasoning = canonical_reasoning;
        }
        if intents.is_empty() {
            intents = canonical_intents;
        }
        Ok((text, reasoning, intents, canonical, usage, false))
    }
}
