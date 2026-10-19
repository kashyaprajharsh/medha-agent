use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn begin_run(
        &self,
        session: &Session,
        mut messages: Vec<Message>,
        budget: crate::budgets::Budget,
        sink: &dyn StreamSink,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
    ) -> Flow<Ready> {
        self.record_opening(session).await?;
        // `None` (headless) → a token that never trips; every cancel path is dead.
        let cancel = interrupts.as_ref().map(|q| q.token()).unwrap_or_default();
        // A sub-agent or a headless run has nobody pausing between its turns.
        let request_owner = crate::RequestScope {
            conversation: session.id.to_string(),
            unattended: self.sub_agent || interrupts.is_none(),
        };
        // A turn that fell back to another model does not decide the next one,
        // unless the user's own is still resting after a limit.
        let resting = match self.sub_agent {
            true => None,
            false => self.provider.restore_primary(),
        };
        if let Some((was, now)) = &resting {
            sink.notice(&format!(
                "{was} is still resting after its limit. Using {now} for this message."
            ));
        }
        let started = crate::events::now_ts();
        let images_left_out_said = false;
        let (retired_said, unreadable_said) = (0, false);
        // Replies that stopped early in this run and were asked to go on.
        let cut_offs = 0u32;
        // What such a reply has written so far, so its next piece does not repeat it.
        let join = crate::seam::Join::default();
        // Times this run waited for a server that was only away for now.
        let waits = 0u32;
        // Empty replies: whether the one nudge was used, the tries made since the
        // last real reply, and whether the last empty one produced no output at all.
        let (empty_nudged, empty_tries, empty_was_silent) = (false, 0u32, false);
        // The last real reply was text followed only by tidying-up tools.
        let tidied_after_text = false;
        // The model that failed and the one answering for it, once one stood in.
        let stood_in: Option<(String, String)> = resting;
        // Where a refusal left this run's images; later requests start there.
        let image_floor = match self.shrinks_images.load(Ordering::Relaxed) {
            true => Sending::Shrunk,
            false => Sending::Whole,
        };
        let planning = session.autonomy == crate::types::AutonomyLevel::Plan;
        // Tools describe themselves by this, so it is said before they are listed.
        self.executor.images_reach_model(
            self.vision.is_some() || self.route_reads_images().unwrap_or(false),
        );
        let gov = crate::budgets::Governor::new(budget).reserve_for_finish();
        // Every trailing user message is new, not just the last: a surface can
        // append a typed prompt and then an agent report in one turn.
        // Memory taint window: event ids since the last real user message and the
        // lowest trust among them. Kernel-owned; the model can never assert it.
        let mut window_events: Vec<ulid::Ulid> = Vec::new();
        let mut window_taint = TrustLabel::User;
        // Skip what the log already ends with: a retry would append the run
        // twice, and the projection only collapses adjacent identical turns.
        let history_started = std::time::Instant::now();
        let prior_events = self.log.checked_events(session.id).await?;
        let prior_tools = crate::tool_snapshot::ToolSnapshot::restore(&prior_events);
        let tool_snapshot = crate::tool_snapshot::ToolSnapshot::refresh(
            prior_tools.as_ref(),
            self.executor.as_ref(),
            false,
        )
        .await;
        let specs = tool_snapshot.specs.clone();
        self.context.note_tools(&specs);
        // A surface restored from the log may end with user-channel failure
        // feedback. That whole prefix is already admitted, even across usage,
        // hook and compaction events; only its new suffix is a new prompt.
        let projected = crate::events::project_messages(&prior_events);
        let system_len = messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        let admitted = (!projected.is_empty()
            && messages[system_len..].len() >= projected.len()
            && messages[system_len..]
                .iter()
                .zip(&projected)
                .all(|(a, b)| same_legacy_message(a, b)))
        .then_some(system_len + projected.len());
        let already = logged_tail(&prior_events);
        // Retried input is already durable but still belongs to this evidence
        // window. Skipping its append must not also erase its provenance or
        // upgrade a Web/Tool report back to User.
        window_events.extend(already.iter().map(|input| input.id));
        for input in &already {
            window_taint = window_taint.min(input.trust);
        }
        let mut logged_cursor = 0;
        let fresh = admitted.unwrap_or_else(|| unlogged_tail(&messages));
        let new_image_submission = messages[fresh..]
            .iter()
            .any(|message| !message.attachments.is_empty());
        let image_route_reported = false;
        for message in &mut messages[fresh..] {
            // Match the durable suffix in order and with its trust label.
            // Membership matching reordered duplicate lines, while comparing
            // text alone could treat a Tool/Web report as a trusted retry.
            let trust = message.trust.unwrap_or(TrustLabel::User);
            if admitted.is_none()
                && already.get(logged_cursor).is_some_and(|input| {
                    crate::turn_instruction::without_prefix(
                        &input.content,
                        input.instruction_prefix.as_deref(),
                    ) == crate::turn_instruction::without_prefix(
                        &message.content,
                        message.instruction_prefix.as_deref(),
                    ) && input.trust == trust
                        && input.attachments == serde_json::json!(message.attachments)
                })
            {
                message.content = already[logged_cursor].content.clone();
                message.instruction_prefix = already[logged_cursor].instruction_prefix.clone();
                logged_cursor += 1;
                continue;
            }
            crate::turn_instruction::apply(message, planning);
            let e = self
                .log
                .append(Event::user_input_message(session, message))
                .await?;
            window_events.push(e.id);
            // A report carries the weakest label its agent touched; injected
            // content must not enter as if the user had typed it.
            if let Some(trust) = message.trust {
                window_taint = window_taint.min(trust);
            }
        }
        let prompt = messages[fresh..]
            .iter()
            .filter(|message| {
                message.role == crate::types::Role::User
                    && message.trust.is_none_or(|trust| trust == TrustLabel::User)
            })
            .map(|message| {
                crate::turn_instruction::without_prefix(
                    &message.content,
                    message.instruction_prefix.as_deref(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut hook_events = self
            .session_start_hooks(session, !prior_events.is_empty(), &cancel, &mut messages)
            .await?;
        match self
            .prompt_submit_hooks(session, &prompt, &cancel, &mut messages)
            .await?
        {
            hook_points::PromptGate::Proceed(events) => hook_events.extend(events),
            hook_points::PromptGate::Blocked(reason) => {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, sink);
                }
                sink.notice(&format!("prompt blocked by a plugin hook: {reason}"));
                return Ok(ControlFlow::Break((messages, StopReason::Blocked)));
            }
        }
        if !hook_events.is_empty() {
            window_events.extend(hook_events);
            window_taint = window_taint.min(TrustLabel::Tool);
        }
        let hook_continuations = 0u8;
        if prior_tools.as_ref().map(|snapshot| snapshot.value()) != Some(tool_snapshot.value()) {
            self.log
                .append(Event::tool_snapshot(session, &tool_snapshot))
                .await?;
        }
        let mut ordered_messages: Vec<ModelMessage> =
            messages.iter().map(Message::ordered).collect();
        let mut logged_events = self.log.checked_events(session.id).await?;
        self.begin_context_message(session, &mut logged_events, sink)
            .await?;
        {
            let mut stored = self.outside_stored.lock().unwrap();
            for event in logged_events.iter().filter(|event| {
                event.trust == TrustLabel::Skill || event.trust.rank() < TrustLabel::Tool.rank()
            }) {
                if let Some(hash) = event.payload["stored"].as_str() {
                    stored
                        .entry(hash.to_owned())
                        .and_modify(|label| *label = label.min(event.trust))
                        .or_insert(event.trust);
                }
            }
        }
        tracing::info!(
            elapsed_ms = history_started.elapsed().as_millis() as u64,
            "session history prepared"
        );
        let checkpoint = logged_events.iter().rev().find(|event| {
            event.kind == EventKind::Compaction
                && crate::events::has_valid_compaction_snapshot(&event.payload)
        });
        let has_checkpoint = checkpoint.is_some();
        let checkpoint_policy = checkpoint
            .and_then(|event| event.payload.get("policy_version"))
            .and_then(serde_json::Value::as_u64);
        let check_checkpoint_policy = has_checkpoint;
        let history_scope = None;
        let restored_usage = false;
        let usage_anchor = logged_events
            .iter()
            .rev()
            .take_while(|event| event.kind != EventKind::Compaction)
            .find(|event| event.kind == EventKind::ContextUsage)
            .and_then(|event| {
                serde_json::from_value::<crate::usage_reporting::UsageAnchor>(event.payload.clone())
                    .ok()
            });
        if has_checkpoint {
            // A compaction event is a full request checkpoint: replace both views
            // wholesale, or the system sheath duplicates and replay is not exact.
            messages = crate::events::project_request_messages(&logged_events);
            ordered_messages = crate::events::project_request_ordered_messages(&logged_events);
        } else if logged_events
            .iter()
            .any(|event| event.kind == EventKind::ModelMessage)
        {
            let projected = crate::events::project_ordered_messages(&logged_events);
            ordered_messages = hydrate_ordered_log(&messages, projected);
        }
        let retained_summary = logged_events.iter().rev().find_map(|event| {
            if event.kind != EventKind::Compaction
                || !crate::events::has_valid_compaction_snapshot(&event.payload)
            {
                return None;
            }
            event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .filter(|summary| {
                    messages.iter().any(|message| {
                        message.role == Role::Assistant && message.content == *summary
                    })
                })
                .map(str::to_owned)
        });
        // Spill after hydration — checkpoint replay would silently undo an earlier
        // spill. Both views are rewritten independently.
        self.egress.restore(&logged_events);
        self.spill_hydrated_tool_results(&mut messages, &mut ordered_messages)
            .await;
        // Compaction changes the visible window, never the session's exposure.
        let web_tainted = messages
            .iter()
            .any(|message| message.trust == Some(TrustLabel::Web))
            || logged_events
                .iter()
                .any(|event| event.trust == TrustLabel::Web);
        Ok(ControlFlow::Continue(Ready {
            cancel,
            request_owner,
            started,
            planning,
            history: History {
                messages,
                ordered_messages,
                logged_events,
                window_events,
                window_taint,
                tool_snapshot,
                specs,
                checkpoint_policy,
                check_checkpoint_policy,
                history_scope,
                restored_usage,
                usage_anchor,
                retained_summary,
                web_tainted,
                has_checkpoint,
            },
            state: RunState {
                gov,
                images_left_out_said,
                retired_said,
                unreadable_said,
                cut_offs,
                join,
                waits,
                empty_nudged,
                empty_tries,
                empty_was_silent,
                tidied_after_text,
                stood_in,
                image_floor,
                image_route_reported,
                new_image_submission,
                hook_continuations,
            },
        }))
    }
}
