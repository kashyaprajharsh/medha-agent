use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    /// Execute one already-admitted intent while preserving the kernel's
    /// cancellation invariant: a call either settles with its real result or
    /// gets one synthesized interrupted observation. Calls that have not
    /// started when cancellation arrives are never dispatched.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_admitted(
        &self,
        session: &Session,
        intent: ToolIntent,
        web_tainted: bool,
        cancel: tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        settle_deadline: Arc<std::sync::OnceLock<tokio::time::Instant>>,
        lane: Option<&Lane>,
        sink: &dyn StreamSink,
    ) -> (
        String,
        String,
        Observation,
        Option<crate::context::DiscoveredContext>,
    ) {
        let deadline_elapsed =
            wall_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        let obs = if deadline_elapsed {
            Observation::error(
                &intent.id,
                "[interrupted] task wall-clock deadline elapsed before tool execution started",
            )
        } else if cancel.is_cancelled() {
            Observation::error(
                &intent.id,
                "[interrupted] cancelled by user before tool execution started",
            )
        } else {
            // The dispatch future is never dropped by cancellation itself: the
            // tool gets TOOL_SETTLE_GRACE to finish and keep its real
            // observation. Only after the grace is it dropped and replaced by
            // a synthetic result, preserving intent → observation.
            let trust = if web_tainted {
                TrustLabel::Web
            } else {
                TrustLabel::Tool
            };
            let fut = async {
                let (mut obs, review) = crate::egress::scope(
                    self.egress.clone(),
                    web_tainted,
                    crate::provenance::execution_trust_scope(
                        trust,
                        self.dispatch_one(session, &intent, web_tainted, &cancel, lane, sink),
                    ),
                )
                .await;
                if let Some(review) = review {
                    sink.notice(&format!("Outbound request blocked: {}. Use /outbound allow {} to approve this exact payload.", review["reason"].as_str().unwrap_or("private content"), review["id"].as_str().unwrap_or_default()));
                    obs.payload["outbound"] = serde_json::json!({"id":review["id"]});
                }
                obs
            };
            tokio::pin!(fut);
            tokio::select! {
                biased;
                _ = wait_for_deadline(wall_deadline) => {
                    Observation::error(
                        &intent.id,
                        "[interrupted] task wall-clock deadline elapsed during tool execution",
                    )
                }
                obs = &mut fut => obs,
                _ = cancel.cancelled() => {
                    let shared_settle_deadline = *settle_deadline
                        .get_or_init(|| tokio::time::Instant::now() + self.settle_grace);
                    let settle_deadline = wall_deadline
                        .map(|deadline| deadline.min(shared_settle_deadline))
                        .unwrap_or(shared_settle_deadline);
                    match tokio::time::timeout_at(settle_deadline, &mut fut).await {
                        Ok(obs) => obs,
                        Err(_) => Observation::error(
                            &intent.id,
                            "[interrupted] cancelled by user; tool did not settle within the grace window",
                        ),
                    }
                }
            }
        };
        // Context discovery is a consequence of a real successful path touch,
        // never of merely asking for a path. Requiring the tool's settled
        // payload to echo the path excludes denied/schema-invalid/failed calls
        // and tools whose `path` argument was not actually used.
        let discovered = match (&self.progressive_context, successful_observed_path(&obs)) {
            (Some(loader), Some(path)) => {
                tokio::select! {
                    biased;
                    _ = wait_for_deadline(wall_deadline) => None,
                    _ = cancel.cancelled() => None,
                    discovered = loader.discover(path) => discovered,
                }
            }
            _ => None,
        };
        (intent.id, intent.tool, obs, discovered)
    }

    /// Run a batch of read-only calls to completion, then hand back every result.
    ///
    /// Draining before persisting anything is load-bearing, not tidiness. The
    /// obvious shape — persist each result as the batch yields it — deadlocks:
    /// `persist_admitted` awaits an append, the event log admits appends through
    /// one fair queue, and the batch's remaining futures are already waiting in
    /// that queue for their own policy decisions. The queue hands the next permit
    /// to a batch future, but nothing will poll it: `Buffered` is polled only by
    /// the loop that has just left it to await an append of its own. The turn
    /// stops there, with a prefix of decisions written, no observations, and
    /// nothing running — a live session sat like that for minutes.
    ///
    /// So the batch is polled to exhaustion first, and appends happen afterwards
    /// when no other future is holding a place in the queue. Reads have no
    /// mutation guard to span, so nothing is weakened by the delay; every
    /// admitted intent still reaches its observation before the next request.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn settle_reads(
        &self,
        session: &Session,
        intents: Vec<ToolIntent>,
        unavailable: &mut std::collections::HashMap<String, Observation>,
        web_tainted: bool,
        cancel: &tokio_util::sync::CancellationToken,
        wall_deadline: Option<tokio::time::Instant>,
        settle_deadline: &Arc<std::sync::OnceLock<tokio::time::Instant>>,
        sink: &dyn StreamSink,
    ) -> Vec<(
        String,
        String,
        Observation,
        Option<crate::context::DiscoveredContext>,
    )> {
        stream::iter(intents)
            .map(|intent| {
                let settled = unavailable.remove(&intent.id);
                let cancel = cancel.clone();
                let settle_deadline = Arc::clone(settle_deadline);
                async move {
                    match settled {
                        Some(obs) => (intent.id, intent.tool, obs, None),
                        None => {
                            self.execute_admitted(
                                session,
                                intent,
                                web_tainted,
                                cancel,
                                wall_deadline,
                                settle_deadline,
                                None,
                                sink,
                            )
                            .await
                        }
                    }
                }
            })
            .buffered(self.max_parallel_tools)
            .collect()
            .await
    }

    /// Make one settled execution durable and feed its observation back. The
    /// caller holds any mutation guard across this and [`Self::execute_admitted`],
    /// closing the side-effect → event-log gap.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_admitted(
        &self,
        session: &Session,
        id: String,
        tool: String,
        mut obs: Observation,
        discovered: Option<crate::context::DiscoveredContext>,
        adjusted: &std::collections::HashMap<String, String>,
        room: &mut usize,
        window_events: &mut Vec<ulid::Ulid>,
        window_taint: &mut TrustLabel,
        web_tainted: &mut bool,
        ordered_messages: &mut Vec<ModelMessage>,
        messages: &mut Vec<Message>,
        sink: &dyn StreamSink,
    ) -> Result<(), KernelError> {
        // Label web-tool output as untrusted content: a fetched page must
        // not be treated like a local file read. A tool relaying content it did
        // not produce declares that content's label, and the weaker wins.
        // An External tool answers from outside this machine's trust boundary —
        // an MCP server is at least as untrusted as a fetched page.
        let trust = match (
            self.executor.category(&tool),
            self.executor.blast_radius(&tool),
        ) {
            (Some(ToolCategory::Web), _) | (_, Some(BlastRadius::External)) => TrustLabel::Web,
            _ => TrustLabel::Tool,
        };
        let mut trust = obs
            .relayed_trust
            .map_or(trust, |relayed| relayed_source(trust, relayed));
        // A result naming a stored output that came from outside is that text again.
        let read_back = obs.payload.get("hash").and_then(serde_json::Value::as_str);
        if let Some(relayed) =
            read_back.and_then(|hash| self.outside_stored.lock().unwrap().get(hash).copied())
        {
            trust = relayed_source(trust, relayed);
        }
        // The model reads its result, so it also reads what Medha changed in
        // the call, and can send the right types itself next time.
        if let Some(note) = adjusted.get(&id)
            && let Some(payload) = obs.payload.as_object_mut()
        {
            payload.insert("adjusted".into(), serde_json::json!(note));
        }
        if let Some(discovered) = discovered {
            trust = trust.min(discovered.trust);
            let context_event = self
                .log
                .append(Event::context_file(
                    session,
                    &discovered.path,
                    &discovered.content,
                    discovered.blocked,
                    discovered.trust,
                ))
                .await?;
            window_events.push(context_event.id);
            *window_taint = window_taint.min(discovered.trust);
            attach_discovered_context(&mut obs, &discovered);
        }
        // Persist applied memory before its observation so replay cannot miss it.
        let applied = if matches!(obs.status, crate::types::ObsStatus::Ok) && is_memory_tool(&tool)
        {
            obs.payload
                .as_object_mut()
                .and_then(|payload| payload.remove("applied"))
        } else {
            None
        };
        if let Some(op) = applied.filter(|op| op.is_object()) {
            self.log.append(Event::memory_write(session, op)).await?;
        }
        let stored = self.fit_for_model(&mut obs, room).await;
        let mut event = Event::observed_tool(session, &obs, trust, &tool);
        if let Some(hash) = &stored {
            event.payload["stored"] = serde_json::json!(hash);
            if trust == TrustLabel::Skill || trust.rank() < TrustLabel::Tool.rank() {
                self.outside_stored
                    .lock()
                    .unwrap()
                    .entry(hash.clone())
                    .and_modify(|label| *label = label.min(trust))
                    .or_insert(trust);
            }
        }
        let event = self.log.append(event).await?;
        let screen = event.payload.get(crate::events::TOOL_SCREEN_FIELD).cloned();
        crate::events::strip_private_observation_fields(&mut obs.payload);
        window_events.push(event.id);
        *window_taint = window_taint.min(trust);
        // Once untrusted web content lands, taint the following provider turn
        // so consequential actions derived from it get escalated.
        if matches!(trust, TrustLabel::Web) {
            *web_tainted = true;
        }
        let ok = matches!(obs.status, crate::types::ObsStatus::Ok);
        sink.tool_result_saved(event.id, &id, &tool, ok, &obs.payload, stored.as_deref());
        if let Some(screen) = &screen {
            sink.tool_screen(&id, screen);
        }
        let message =
            Message::tool_result(&id, crate::events::observation_for_model(&event.payload))
                .carrying(trust);
        ordered_messages.push(message.ordered());
        messages.push(message);
        if !obs.media.is_empty() {
            let carrier = crate::events::tool_media_message(&id, std::mem::take(&mut obs.media))
                .carrying(trust);
            ordered_messages.push(carrier.ordered());
            messages.push(carrier);
        }
        Ok(())
    }
}
