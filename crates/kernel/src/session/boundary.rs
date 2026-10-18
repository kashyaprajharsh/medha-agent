use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn turn_boundary(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
    ) -> Flow<()> {
        // Turn boundary: honor a pending cancel first (queued steers go
        // BACK to the surface, not into a turn that won't run), then
        // inject queued steers as user messages.
        if let Some(q) = interrupts.as_mut() {
            if q.cancel_requested() {
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
            let mut person_steered = false;
            for (s, trust) in q.drain_steers() {
                let e = self
                    .log
                    .append(Event::user_input(cx.session, &s, trust))
                    .await?;
                if trust == TrustLabel::User {
                    person_steered = true;
                    history.window_events.clear();
                    history.window_taint = trust;
                } else {
                    history.window_taint = history.window_taint.min(trust);
                }
                history.window_events.push(e.id);
                history.logged_events.push(e);
                history.history_scope = None;
                // A sub-agent's report arrives on this queue too, and it is
                // worth what the agent touched, not what the operator says.
                if matches!(trust, TrustLabel::Web) {
                    history.web_tainted = true;
                }
                self.log
                    .append(Event::interrupt(cx.session, "steer", Some(&s)))
                    .await
                    .ok();
                cx.sink.steered(&s);
                let mut message = Message::user(s);
                if trust != TrustLabel::User {
                    message.trust = Some(trust);
                }
                history.ordered_messages.push(message.ordered());
                history.messages.push(message);
            }
            if person_steered {
                self.begin_context_message(cx.session, &mut history.logged_events, cx.sink)
                    .await?;
            }
        }

        // Budget gate: stop gracefully before a turn if any ceiling is hit (I4).
        if let Some(stop) = state.gov.check() {
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
            return Ok(ControlFlow::Break((
                std::mem::take(&mut history.messages),
                StopReason::Budget(stop),
            )));
        }
        state.gov.record_turn();
        Ok(ControlFlow::Continue(()))
    }
}
