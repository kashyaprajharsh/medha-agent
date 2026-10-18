use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn complete_turn(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
        finishing: bool,
        modified_files: bool,
    ) -> Flow<()> {
        let dispatch_wall_deadline = state.gov.deadline();
        // Cancelled during dispatch: every admitted intent has settled
        // (real or synthesized observation, logged above) — stop here.
        // The verifier is skipped deliberately: the user asked to stop,
        // and a build/test run can be long.
        if matches!(state.gov.check(), Some(crate::budgets::BudgetStop::Wall)) {
            if let Some(q) = interrupts.as_mut() {
                Self::return_unapplied_steers(q, cx.sink);
            }
            return Ok(ControlFlow::Break((
                std::mem::take(&mut history.messages),
                StopReason::Budget(crate::budgets::BudgetStop::Wall),
            )));
        }
        if cx.cancel.is_cancelled() {
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

        let mut completion_verified = false;
        // Required checks run again at completion, including resumed sessions
        // and text-only claims. Plan never launches a verification command.
        if !cx.planning && (modified_files || (finishing && self.verifier.required())) {
            let verification = tokio::select! {
                biased;
                _ = wait_for_deadline(dispatch_wall_deadline) => {
                    if let Some(q) = interrupts.as_mut() {
                        Self::return_unapplied_steers(q, cx.sink);
                    }
                    return Ok(ControlFlow::Break((std::mem::take(&mut history.messages),
                        StopReason::Budget(crate::budgets::BudgetStop::Wall),
                    )));
                }
                _ = cx.cancel.cancelled() => None,
                report = self.verifier.check(cx.cancel) => report,
            };
            // Cancellation can arrive while the verifier is running. Its
            // process tree has now settled; stop instead of injecting a
            // synthetic verifier failure into a turn the user cancelled.
            if cx.cancel.is_cancelled() {
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
            let verification = verification.or_else(|| {
                self.verifier
                    .required()
                    .then(|| crate::verify::VerifyReport {
                        ok: false,
                        summary: "required verification returned no result".into(),
                        output:
                            "Configure a working verification command before completing this task."
                                .into(),
                    })
            });
            if let Some(rep) = verification {
                completion_verified = rep.ok;
                cx.sink.verify(rep.ok, &rep.summary);
                let mut tail: Vec<&str> = rep.output.lines().rev().take(40).collect();
                tail.reverse();
                let feedback = format!(
                    "[verifier] {} — {}\n{}",
                    if rep.ok { "PASS" } else { "FAIL" },
                    rep.summary,
                    tail.join("\n")
                );
                // Verifier output is tool-produced, and can contain arbitrary
                // build-script/test output. Labelling it as User launders that
                // text into the most-trusted instruction channel.
                let mut event = Event::user_input(cx.session, &feedback, TrustLabel::Tool);
                event.payload["verifier"] = serde_json::json!({
                    "ok": rep.ok,
                    "summary": rep.summary,
                    "output": tail.join("\n"),
                });
                self.save_feedback(history, event, feedback).await?;
            }
        }
        if finishing
            && self
                .task_completion_hooks(
                    cx.session,
                    hook_points::CompletionHookState {
                        verified: completion_verified,
                        continuations: state.hook_continuations,
                        origin: history.window_events.first().copied(),
                    },
                    cx.cancel,
                    &mut history.messages,
                    &mut history.ordered_messages,
                )
                .await?
        {
            state.hook_continuations += 1;
            history.window_taint = history.window_taint.min(TrustLabel::Tool);
            return Ok(ControlFlow::Continue(()));
        }
        if finishing {
            if let Some(q) = interrupts.as_mut() {
                Self::return_unapplied_steers(q, cx.sink);
            }
            let reason = if !cx.planning && self.verifier.required() && !completion_verified {
                StopReason::VerificationFailed
            } else {
                StopReason::Finished
            };
            return Ok(ControlFlow::Break((
                std::mem::take(&mut history.messages),
                reason,
            )));
        }
        Ok(ControlFlow::Continue(()))
    }
}
