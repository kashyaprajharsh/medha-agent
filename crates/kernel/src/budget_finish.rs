use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn finish_budget(
        &self,
        session: &Session,
        messages: &mut Vec<Message>,
        governor: &mut crate::budgets::Governor,
        sink: &dyn StreamSink,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), KernelError> {
        const FALLBACK: &str = "The task budget has no room for another working request. Saved work and the conversation are kept.";
        let mut context = CompiledContext {
            model: String::new(),
            messages: messages.clone(),
            ordered: None,
            tools: vec![],
        };
        context.messages.push(Message::user("The task's working budget is exhausted. Give a short factual handover of completed work, checks and anything unfinished. Do not request or call tools.").carrying(TrustLabel::System));
        let mut request = self
            .provider
            .prepare_request(&crate::provenance::for_model(&context))
            .map_err(|error| KernelError::Provider(error.to_string()))?;
        let control = crate::CompileControl::new(cancel.clone(), governor.deadline());
        let count = match self.validated_preflight(&request, &control).await {
            Ok(Some(count)) => count,
            _ => {
                sink.notice(FALLBACK);
                return Ok(());
            }
        };
        let Some(cap) = governor.begin_wrap_up(count.tokens.saturating_add(32), self.price())
        else {
            sink.notice(FALLBACK);
            return Ok(());
        };
        let Some(bounded) = self
            .provider
            .with_output_limit(&request, cap)
            .ok()
            .flatten()
        else {
            sink.notice(FALLBACK);
            return Ok(());
        };
        request = bounded;
        if let Some(quiet) = self
            .provider
            .with_request_reasoning(
                &request,
                &crate::ReasoningConfig {
                    enabled: Some(false),
                    effort: None,
                },
            )
            .ok()
            .flatten()
        {
            request = quiet;
        }
        let count = match self.validated_preflight(&request, &control).await {
            Ok(Some(count)) => count,
            _ => {
                sink.notice(FALLBACK);
                return Ok(());
            }
        };
        let Ok(reservation) = governor.reserve_model(Some(count.tokens), Some(cap), self.price())
        else {
            sink.notice(FALLBACK);
            return Ok(());
        };
        sink.notice("The working budget is nearly used. Using its reserved allowance for one short handover.");
        let mut cut = crate::cut_off::CutOff::default();
        let mut join = crate::seam::Join::default();
        match self
            .stream_turn(
                &request,
                sink,
                cancel,
                governor.deadline(),
                &mut cut,
                &mut join,
            )
            .await
        {
            Ok((text, _, _, canonical, usage, _)) => {
                let accounting = reservation.reconcile(usage, self.price());
                if !text.is_empty() {
                    let event = self.log.append(Event::model_text(session, &text)).await?;
                    sink.text_saved(event.id);
                    self.log
                        .append(Event::model_message(session, &strip_tool_calls(&canonical)))
                        .await?;
                    messages.push(Message::assistant_calls(text, vec![]));
                } else {
                    sink.notice(FALLBACK);
                }
                if let Some(price) = self.price() {
                    sink.cost(governor.cost_usd(), price.indicative);
                }
                accounting.map_err(KernelError::Budget)?;
            }
            Err(_) => {
                reservation
                    .reconcile(None, self.price())
                    .map_err(KernelError::Budget)?;
                sink.notice(FALLBACK);
            }
        }
        Ok(())
    }
}
