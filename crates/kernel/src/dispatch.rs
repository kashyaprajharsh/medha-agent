use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    /// Take `lane` unless this call holds it already; an error is for the model.
    pub(super) async fn enter_lane(&self, lane: &Lane) -> Result<(), String> {
        if lane.held.lock().unwrap().is_some() {
            return Ok(());
        }
        let entering = async {
            let held = self.lanes.enter(&lane.key).await;
            let lease = self.leases.share(self.log.as_ref(), &lane.key).await;
            lease.map(|lease| (held, lease))
        };
        let entered = tokio::select! {
            biased;
            _ = wait_for_deadline(lane.deadline) => return Err(
                "[interrupted] task wall-clock deadline elapsed while waiting for the mutation lane".into(),
            ),
            _ = lane.cancel.cancelled() => return Err(
                "[interrupted] cancelled before the mutation lane became available".into(),
            ),
            entered = entering => entered,
        };
        let held = entered.map_err(|error| {
            format!(
                "state change was not started because its mutation lease could not be \
                 acquired: {error}"
            )
        })?;
        *lane.held.lock().unwrap() = Some(held);
        Ok(())
    }

    pub(super) async fn execute_with_effect_outbox(
        &self,
        session: &Session,
        intent: &ToolIntent,
        lane: Option<&Lane>,
    ) -> Observation {
        if let Some(lane) = lane
            && let Err(reason) = self.enter_lane(lane).await
        {
            return Observation::error(&intent.id, reason);
        }
        if let Some(mutation_key) = self.executor.mutation_key(intent)
            && let Err(error) = self
                .log
                .append(Event::tool_effect_prepared(session, intent, &mutation_key))
                .await
        {
            return Observation::error(
                &intent.id,
                format!(
                    "state change was not started because its durable execution record \
                     could not be written: {error}"
                ),
            );
        }
        self.executor.execute(intent).await
    }

    pub(super) async fn invoke_hook(
        &self,
        session: &Session,
        point: medha_extension_api::HookPoint,
        trust: TrustLabel,
        payload: serde_json::Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::hooks::HookBatch, String> {
        self.invoke_hook_lineage(
            session,
            point,
            trust,
            payload,
            crate::hooks::HookCausation::default(),
            cancel,
        )
        .await
    }

    pub(super) async fn invoke_hook_lineage(
        &self,
        session: &Session,
        point: medha_extension_api::HookPoint,
        trust: TrustLabel,
        payload: serde_json::Value,
        causation: crate::hooks::HookCausation,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::hooks::HookBatch, String> {
        let mut request =
            crate::hooks::HookRequest::new(session.id.to_string(), point, trust, payload);
        request.causation_id = causation.id;
        request.depth = causation.depth;
        let mut batch = self.hooks.invoke(&request, cancel).await;
        if !matches!(batch.directive, crate::hooks::HookDirective::Continue)
            && batch.audits.is_empty()
        {
            return Err("hook runner returned an unaudited decision".into());
        }
        for audit in &mut batch.audits {
            audit.event_id.clone_from(&request.event_id);
            audit.point = point;
            audit.reason = audit.reason.as_deref().map(bounded_hook_reason);
            self.log
                .append(Event::hook(session, audit))
                .await
                .map_err(|error| format!("hook decision could not be recorded: {error}"))?;
        }
        Ok(batch)
    }

    pub(super) async fn apply_post_tool_hook(
        &self,
        session: &Session,
        intent: &ToolIntent,
        mut observation: Observation,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        sink: &dyn StreamSink,
    ) -> Observation {
        let payload = serde_json::json!({
            "intent_id": intent.id,
            "tool": intent.tool,
            "status": observation.status,
            "result": hook_safe_value(&observation.payload),
        });
        let trust = if web_tainted {
            TrustLabel::Web
        } else {
            TrustLabel::Tool
        };
        let failed = observation.status == crate::types::ObsStatus::Error;
        let mut points = vec![medha_extension_api::HookPoint::PostTool];
        if failed {
            points.push(medha_extension_api::HookPoint::ToolFailure);
        }
        let mut contexts = Vec::new();
        let mut problem = None;
        for point in points {
            match self
                .invoke_hook(session, point, trust, payload.clone(), cancel)
                .await
            {
                Ok(batch) => {
                    for notice in &batch.notices {
                        sink.notice(notice);
                    }
                    contexts.extend(batch.contexts);
                    if let crate::hooks::HookDirective::Deny(reason)
                    | crate::hooks::HookDirective::RequestApproval(reason) = batch.directive
                    {
                        problem.get_or_insert(reason);
                    }
                }
                Err(error) => {
                    problem.get_or_insert(error);
                }
            }
        }
        let file_changed = observation.status == crate::types::ObsStatus::Ok
            && matches!(intent.tool.as_str(), "write" | "edit");
        Self::attach_hook_contexts(&mut observation, &contexts);
        if let Some(problem) = problem {
            let prior_status = observation.status.clone();
            let prior_payload = std::mem::take(&mut observation.payload);
            observation.status = crate::types::ObsStatus::Error;
            observation.payload = serde_json::json!({
                "error": format!(
                    "tool completed, but its required post-tool hook did not settle safely: {problem}"
                ),
                "tool_status": prior_status,
                "tool_result": prior_payload,
            });
        }
        if file_changed {
            self.observe_hook(
                session,
                medha_extension_api::HookPoint::FileChange,
                serde_json::json!({
                    "intent_id": intent.id,
                    "tool": intent.tool,
                    "args": hook_safe_value(&intent.args),
                }),
            )
            .await;
        }
        observation
    }

    pub(super) async fn dispatch_one(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        lane: Option<&Lane>,
        sink: &dyn StreamSink,
    ) -> Observation {
        if let Some(blocked) = self.check_outbound_intent(session, intent).await {
            return blocked;
        }
        let radius = self.executor.call_radius(intent);
        // Enforce before policy/human approval so custom AllowAll policies and
        // remembered approvals cannot turn Plan into an editing session.
        let raw = if session.autonomy == crate::types::AutonomyLevel::Plan
            && (radius != Some(BlastRadius::Read) || self.executor.mutation_key(intent).is_some())
        {
            crate::types::Decision::Deny {
                reason: "plan mode permits read-only tools; switch mode to implement".into(),
            }
        } else {
            self.policy.authorize(session.autonomy, intent, radius)
        };
        // Trust-flow escalations must never be auto-approved. That holds for a
        // call policy already sends to a human: its card is marked all the same.
        let containment = self.executor.call_containment(intent).await;
        let mut decision = escalate_for_trust_flow(raw, radius, web_tainted, containment);
        let trust_escalated = follows_web(radius, web_tainted, containment)
            && matches!(decision, crate::types::Decision::Human);
        let mut hook_approval_reason = None;
        if !matches!(decision, crate::types::Decision::Deny { .. }) {
            let trust = if web_tainted {
                TrustLabel::Web
            } else {
                TrustLabel::System
            };
            let payload = serde_json::json!({
                "intent_id": intent.id,
                "tool": intent.tool,
                "blast_radius": radius,
                "args": hook_safe_value(&intent.args),
            });
            let hooks = match self
                .invoke_hook(
                    session,
                    medha_extension_api::HookPoint::PreTool,
                    trust,
                    payload,
                    cancel,
                )
                .await
            {
                Ok(hooks) => hooks,
                Err(error) => {
                    return Observation::error(
                        &intent.id,
                        format!(
                            "tool execution was denied because its pre-tool hook was not auditable: {error}"
                        ),
                    );
                }
            };
            for notice in &hooks.notices {
                sink.notice(notice);
            }
            match hooks.directive {
                crate::hooks::HookDirective::Continue => {}
                crate::hooks::HookDirective::Deny(reason) => {
                    decision = crate::types::Decision::Deny { reason };
                }
                crate::hooks::HookDirective::RequestApproval(reason) => {
                    hook_approval_reason = Some(reason);
                    decision = crate::types::Decision::Human;
                }
            }
        }
        if cancel.is_cancelled() {
            return Observation::error(
                &intent.id,
                "[interrupted] cancelled while running pre-tool hooks",
            );
        }
        let escalated = trust_escalated || hook_approval_reason.is_some();
        if let Err(error) = self
            .log
            .append(Event::policy(session, intent, &decision))
            .await
        {
            return Observation::error(
                &intent.id,
                format!(
                    "tool execution was denied because its policy decision could not be \
                     durably recorded: {error}"
                ),
            );
        }
        if !matches!(decision, crate::types::Decision::Deny { .. }) {
            let access = match self.executor.missing_access(intent) {
                Ok(access) => access,
                Err(error) => return Observation::error(&intent.id, error),
            };
            if !access.is_empty() {
                // This card reviews both the command and its missing capabilities,
                // so a policy Human decision does not create a second prompt.
                let jailed = crate::types::Containment::OsFsJail;
                let access_escalated =
                    escalated || (access.network && follows_web(radius, web_tainted, jailed));
                sink.phase(crate::progress::Phase::AwaitingApproval {
                    action: approval_key(intent),
                });
                let answer = {
                    let _one_gate = self.gate_serial.lock().await;
                    let mut detail = self
                        .executor
                        .preview(intent)
                        .await
                        .unwrap_or_else(|| approval_detail(intent));
                    detail.push_str("\n\nAdditional access required before running:");
                    if access.outside_sandbox {
                        detail.push_str(
                            "\n- Run outside Medha's sandbox, this one time: the command can \
                             read and change anything your user account can",
                        );
                    }
                    if access.network {
                        detail.push_str("\n- Network access");
                    }
                    for path in &access.read_paths {
                        detail.push_str(&format!(
                            "\n- Read {} (including subdirectories)",
                            path.display()
                        ));
                    }
                    for path in &access.write_paths {
                        detail.push_str(&format!(
                            "\n- Read/write {} (including subdirectories)",
                            path.display()
                        ));
                    }
                    if access_escalated {
                        detail.push_str("\nThis command handled untrusted content; approve this invocation only.");
                    }
                    if let Some(reason) = &hook_approval_reason {
                        detail.push_str("\nA pre-tool hook requires approval: ");
                        detail.push_str(reason);
                    }
                    self.executor
                        .grant_access(&access, &detail, access_escalated)
                        .await
                };
                self.observe_hook(
                    session,
                    medha_extension_api::HookPoint::ApprovalDecision,
                    serde_json::json!({
                        "intent_id": intent.id,
                        "tool": intent.tool,
                        "kind": "command_access",
                        "decision": answer.as_ref().map_or("error".to_string(), |value| format!("{value:?}").to_lowercase()),
                    }),
                )
                .await;
                match answer {
                    Err(error) => return Observation::error(&intent.id, error),
                    Ok(crate::NetworkDecision::Deny) => {
                        return Observation::denial(
                            &intent.id,
                            "requested command access was denied; command was not run",
                        );
                    }
                    Ok(_) => {
                        self.running_tool(intent, sink);
                        let observation = crate::execution_access_scope(
                            access,
                            self.execute_with_effect_outbox(session, intent, lane),
                        )
                        .await;
                        return self
                            .apply_post_tool_hook(
                                session,
                                intent,
                                observation,
                                web_tainted,
                                cancel,
                                sink,
                            )
                            .await;
                    }
                }
            }
        }
        match decision {
            crate::types::Decision::Deny { reason } => Observation::denial(&intent.id, reason),
            crate::types::Decision::Human => {
                // Published before the wait for the lane, not after: an agent
                // queued behind another agent's card is still blocked on a
                // person, and a watcher must be able to say so.
                sink.phase(crate::progress::Phase::AwaitingApproval {
                    action: approval_key(intent),
                });
                // Hold the lock through the answer, then drop it before execution so an
                // approved slow tool doesn't block the next card.
                let answer = {
                    let _one_gate = self.gate_serial.lock().await;
                    let mut detail = self
                        .executor
                        .preview(intent)
                        .await
                        .unwrap_or_else(|| approval_detail(intent));
                    if let Some(reason) = &hook_approval_reason {
                        detail.push_str("\n\nA pre-tool hook requires approval: ");
                        detail.push_str(reason);
                    }
                    let action = approval_key(intent);
                    self.gate.confirm(&action, Some(&detail), escalated).await
                };
                self.observe_hook(
                    session,
                    medha_extension_api::HookPoint::ApprovalDecision,
                    serde_json::json!({
                        "intent_id": intent.id,
                        "tool": intent.tool,
                        "kind": "tool",
                        "decision": format!("{answer:?}").to_lowercase(),
                    }),
                )
                .await;
                let approved = answer.approved();
                if approved {
                    self.running_tool(intent, sink);
                    let observation = self
                        .execute_with_net_retry(session, intent, web_tainted, radius, lane)
                        .await;
                    self.apply_post_tool_hook(
                        session,
                        intent,
                        observation,
                        web_tainted,
                        cancel,
                        sink,
                    )
                    .await
                } else {
                    Observation::denial(&intent.id, self.gate.denial_reason().to_string())
                }
            }
            crate::types::Decision::Allow => {
                self.running_tool(intent, sink);
                let observation = self
                    .execute_with_net_retry(session, intent, web_tainted, radius, lane)
                    .await;
                self.apply_post_tool_hook(session, intent, observation, web_tainted, cancel, sink)
                    .await
            }
        }
    }

    /// Publish which tool is running, and on what. A watcher showing "in tool"
    /// with no target cannot tell a wedged read from a long build.
    pub(super) fn running_tool(&self, intent: &ToolIntent, sink: &dyn StreamSink) {
        sink.phase(crate::progress::Phase::InTool {
            tool: intent.tool.clone(),
            target: salient_arg(intent).map(str::to_string),
        });
    }

    /// Run the intent; if it failed because the sandbox denied network, offer a
    /// grant and retry. The first run under net-deny could not exfiltrate, so the
    /// grant admits a *second* action: trust-flow is re-evaluated as if the box's
    /// network were already open (a literal [`Containment::OsFsJail`]), and a
    /// web-tainted consequential action is gated exactly as it would be without
    /// confinement. The retry calls [`Self::execute_with_effect_outbox`] directly
    /// rather than recursing, so a real DNS outage matching the same signature
    /// cannot re-prompt.
    pub(super) async fn execute_with_net_retry(
        &self,
        session: &Session,
        intent: &ToolIntent,
        web_tainted: bool,
        radius: Option<BlastRadius>,
        lane: Option<&Lane>,
    ) -> Observation {
        let obs = self.execute_with_effect_outbox(session, intent, lane).await;
        if !obs.net_denied || !self.executor.allows_network_retry(intent) {
            return obs;
        }
        // The first run is over; other changes need not wait on this answer.
        if let Some(lane) = lane {
            lane.held.lock().unwrap().take();
        }
        // Trust-flow escalation only, not the policy engine: argv is unchanged, so
        // policy would repeat the verdict that already admitted this intent. Given
        // `Allow` the result can only widen to `Human`, never narrow to `Deny`.
        let escalated = follows_web(radius, web_tainted, crate::types::Containment::OsFsJail);
        let decision = {
            let _one_gate = self.gate_serial.lock().await;
            let detail = net_grant_detail(intent, web_tainted, &obs);
            self.executor.grant_network(Some(&detail), escalated).await
        };
        self.observe_hook(
            session,
            medha_extension_api::HookPoint::ApprovalDecision,
            serde_json::json!({
                "intent_id": intent.id,
                "tool": intent.tool,
                "kind": "network_retry",
                "decision": format!("{decision:?}").to_lowercase(),
            }),
        )
        .await;
        match decision {
            crate::NetworkDecision::Deny => obs,
            crate::NetworkDecision::Once => {
                crate::network_once_scope(self.execute_with_effect_outbox(session, intent, lane))
                    .await
            }
            crate::NetworkDecision::Session | crate::NetworkDecision::Persistent => {
                // The grant already flipped the shared network flag; the retry
                // and every later command now reach the network.
                self.execute_with_effect_outbox(session, intent, lane).await
            }
        }
    }
}
