use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn dispatch_turn(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        intents: Vec<ToolIntent>,
        request_tools: &[String],
    ) -> Result<bool, KernelError> {
        // Model-supplied trust/confidence/provenance is stripped and replaced
        // with taint-window values, which stop at this turn's dispatch.
        let mut intents = intents;
        // Arguments of the wrong JSON type are repaired against the tool's
        // own schema before policy, approval and dispatch see them. The
        // saved reply above keeps what the model actually wrote.
        let mut adjusted = std::collections::HashMap::new();
        // What this round's results may still put in front of the model.
        let mut room = self.result_limits().1;
        for it in &mut intents {
            let Some(spec) = history.specs.iter().find(|spec| spec.name == it.tool) else {
                continue;
            };
            let repairs = crate::arg_repair::repair(&mut it.args, &spec.schema);
            if let Some(note) = crate::arg_repair::note(&repairs) {
                let paths: Vec<&str> = repairs.iter().map(|repair| repair.path.as_str()).collect();
                tracing::info!(tool = %it.tool, arguments = ?paths, "tool call arguments repaired");
                adjusted.insert(it.id.clone(), note);
            }
        }
        for it in &mut intents {
            if is_memory_tool(&it.tool) {
                // A page stays in the model's view after the person's next
                // message, so what is saved in this chat is not their words.
                let trust = match history.web_tainted {
                    true => TrustLabel::Web,
                    false => history.window_taint,
                };
                enrich_memory_intent(&mut it.args, trust, &history.window_events, cx.session.id);
            }
        }

        // Dispatch admission: intents are logged HERE — after the cancel
        // check, immediately before execution — so a logged intent always
        // gets an observation (real or synthesized). Replay order per id is
        // intent → policy.decision → observation (unavailable tools skip policy).
        for it in &intents {
            let mut event = Event::model_intent(cx.session, it);
            if let Some(note) = adjusted.get(&it.id) {
                event.payload["adjusted"] = serde_json::json!(note);
            }
            self.log.append(event).await?;
        }
        // Notify the surface of the calls before they run (live feedback).
        for it in &intents {
            cx.sink.tool_call_with_id(&it.id, &it.tool, &it.args);
        }
        // Provider adapters have already mapped wire names to canonical names.
        // An undeclared call is invalid model output, not a policy denial: it
        // skips mutation scheduling, hooks, approval and execution, but settles
        // in its call-order slot — some chat templates pair results by position.
        let mut unavailable = std::collections::HashMap::new();
        if intents.iter().any(|i| !request_tools.contains(&i.tool)) {
            let wire = crate::portable_tool_name_map(request_tools);
            let mut available: Vec<String> = wire.keys().cloned().collect();
            available.sort();
            for intent in intents.iter().filter(|i| !request_tools.contains(&i.tool)) {
                let next = match crate::types::suggested_tool(&intent.tool, &wire) {
                    Some(tool) => format!("Use `{tool}` for this instead."),
                    None => "Choose an available tool.".to_string(),
                };
                let mut observation = Observation::error(
                    &intent.id,
                    format!(
                        "Tool '{}' is unavailable in this request. {next} Approval cannot \
                             enable this tool; do not retry this name.",
                        intent.tool
                    ),
                );
                observation.payload["error_code"] = serde_json::json!("tool_unavailable");
                observation.payload["requested_tool"] = serde_json::json!(intent.tool);
                observation.payload["available_tools"] = serde_json::json!(available);
                unavailable.insert(intent.id.clone(), observation);
            }
        }
        let mut modified_files = false;

        // Reads run together; this turn's changes run one by one, in order. If
        // two commit B→A while the log records A→B, replay disagrees with live
        // state. What a change must not overlap comes from the executor.
        let dispatch_cancel = cx.cancel.clone();
        let dispatch_wall_deadline = state.gov.deadline();
        let dispatch_settle_deadline = Arc::new(std::sync::OnceLock::new());
        // Every intent in this batch was proposed from the same prior model
        // input. A web result persisted early below taints the *next* turn,
        // not sibling calls the model had already emitted without seeing it.
        let dispatch_web_tainted = history.web_tainted;
        let mut read_batch = Vec::new();
        for intent in intents {
            if !unavailable.contains_key(&intent.id)
                && let Some(mutation_key) = self.executor.mutation_key(&intent)
            {
                if !read_batch.is_empty() {
                    let settled = self
                        .settle_reads(
                            cx.session,
                            std::mem::take(&mut read_batch),
                            &mut unavailable,
                            dispatch_web_tainted,
                            &dispatch_cancel,
                            dispatch_wall_deadline,
                            &dispatch_settle_deadline,
                            cx.sink,
                        )
                        .await;
                    for (id, tool, mut obs, discovered) in settled {
                        modified_files |= self.executor.changed_files(&tool, &obs);
                        if let Some(notice) = state.refusals.note(&tool, &mut obs) {
                            cx.sink.notice(&notice);
                        }
                        self.persist_admitted(
                            cx.session,
                            id,
                            tool,
                            obs,
                            discovered,
                            &adjusted,
                            &mut room,
                            &mut history.window_events,
                            &mut history.window_taint,
                            &mut history.web_tainted,
                            &mut history.ordered_messages,
                            &mut history.messages,
                            cx.sink,
                        )
                        .await?;
                    }
                }
                // The change takes its lane once approved, inside, and keeps it
                // until its result is saved below; a later read never waits on it.
                let lane = Lane {
                    key: mutation_key,
                    cancel: dispatch_cancel.clone(),
                    deadline: dispatch_wall_deadline,
                    held: Default::default(),
                };
                let (id, tool, mut obs, discovered) = self
                    .execute_admitted(
                        cx.session,
                        intent,
                        dispatch_web_tainted,
                        dispatch_cancel.clone(),
                        dispatch_wall_deadline,
                        Arc::clone(&dispatch_settle_deadline),
                        Some(&lane),
                        cx.sink,
                    )
                    .await;
                modified_files |= self.executor.changed_files(&tool, &obs);
                if let Some(notice) = state.refusals.note(&tool, &mut obs) {
                    cx.sink.notice(&notice);
                }
                self.persist_admitted(
                    cx.session,
                    id,
                    tool,
                    obs,
                    discovered,
                    &adjusted,
                    &mut room,
                    &mut history.window_events,
                    &mut history.window_taint,
                    &mut history.web_tainted,
                    &mut history.ordered_messages,
                    &mut history.messages,
                    cx.sink,
                )
                .await?;
                drop(lane);
            } else {
                read_batch.push(intent);
            }
        }
        if !read_batch.is_empty() {
            let settled = self
                .settle_reads(
                    cx.session,
                    read_batch,
                    &mut unavailable,
                    dispatch_web_tainted,
                    &dispatch_cancel,
                    dispatch_wall_deadline,
                    &dispatch_settle_deadline,
                    cx.sink,
                )
                .await;
            for (id, tool, mut obs, discovered) in settled {
                modified_files |= self.executor.changed_files(&tool, &obs);
                if let Some(notice) = state.refusals.note(&tool, &mut obs) {
                    cx.sink.notice(&notice);
                }
                self.persist_admitted(
                    cx.session,
                    id,
                    tool,
                    obs,
                    discovered,
                    &adjusted,
                    &mut room,
                    &mut history.window_events,
                    &mut history.window_taint,
                    &mut history.web_tainted,
                    &mut history.ordered_messages,
                    &mut history.messages,
                    cx.sink,
                )
                .await?;
            }
        }
        Ok(modified_files)
    }
}
