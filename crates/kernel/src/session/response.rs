use super::*;

impl<P: Provider, L: EventLog> Kernel<P, L> {
    pub(super) async fn save_response(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        request: &mut RequestState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
        turn: Turn,
    ) -> Flow<Accepted> {
        // Before this round's tools run: could an image reach the model at
        // all? What a refusal just taught is already counted.
        self.executor.images_reach_model(
            self.vision.is_some() || self.route_reads_images().unwrap_or(false),
        );
        let (assistant, canonical, intents, usage, turn_interrupted) = turn;
        if let Some(u) = usage {
            self.context.update_usage(u.prompt_tokens, u.total_tokens);
        }
        if let Some(p) = self.price() {
            let (cost, indicative) = self.reported_cost(&state.gov, p.indicative);
            cx.sink.cost(cost, indicative);
        }
        // Interrupted mid-stream, or cancelled between the stream ending
        // and dispatch: keep visible content but drop un-admitted calls.
        // Persist the exact ordered remainder only after that decision, so
        // replay can never contain a call which dispatch never admitted.
        if turn_interrupted || cx.cancel.is_cancelled() {
            let canonical = strip_tool_calls(&canonical);
            if !assistant.content.is_empty() {
                let mut reply = self
                    .reply_text(cx.session, &assistant.content, cx.started)
                    .await;
                if let Some((was, now)) = &state.stood_in {
                    reply.payload["stand_in"] = serde_json::json!({ "model": now, "for": was });
                }
                let saved = self.log.append(reply).await?;
                cx.sink.text_saved(saved.id);
            }
            // An empty cancelled turn can create invalid adjacent roles on replay.
            if !canonical.parts.is_empty() {
                self.log
                    .append(Event::model_message(cx.session, &canonical))
                    .await?;
                history.ordered_messages.push(canonical);
                // Keep compatibility and canonical histories aligned.
                history
                    .messages
                    .push(Message::assistant_calls(assistant.content, Vec::new()));
            } else if !assistant.content.is_empty() {
                // Preserve compatibility text omitted from the canonical view.
                history
                    .messages
                    .push(Message::assistant_calls(assistant.content, Vec::new()));
            }
            if matches!(state.gov.check(), Some(crate::budgets::BudgetStop::Wall)) {
                if let Some(q) = interrupts.as_mut() {
                    Self::return_unapplied_steers(q, cx.sink);
                }
                return Ok(ControlFlow::Break((
                    std::mem::take(&mut history.messages),
                    StopReason::Budget(crate::budgets::BudgetStop::Wall),
                )));
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
        }
        // No words and no action: not an answer, and never saved as one.
        let empty =
            assistant.content.trim().is_empty() && intents.is_empty() && request.go_on.is_none();
        let thought = canonical
            .parts
            .iter()
            .any(|part| matches!(part, ContentPart::Reasoning(_)));
        // A piece that only wrote the end of the last one again adds nothing:
        // the reply was already whole, and the turn ends as it is.
        let said_again = empty && state.join.cut > 0;
        // The text so far is carried only while the reply is still going on.
        match &request.go_on {
            Some(_) => state.join.written.push_str(&assistant.content),
            None if !empty || said_again => state.join.written.clear(),
            None => {}
        }
        // A sub-agent sharing this connection may have moved it to a stand-in:
        // the answer is still marked, and the person is still told.
        if let (None, Some((was, now))) = (&state.stood_in, self.provider.standing_in()) {
            cx.sink.notice(&format!(
                "{was} could not be used, so {now} is answering for the rest of this turn."
            ));
            state.stood_in = Some((was, now));
        }
        if !assistant.content.is_empty() && !empty {
            let mut reply = self
                .reply_text(cx.session, &assistant.content, cx.started)
                .await;
            if let Some((was, now)) = &state.stood_in {
                reply.payload["stand_in"] = serde_json::json!({ "model": now, "for": was });
            }
            let saved = self.log.append(reply).await?;
            cx.sink.text_saved(saved.id);
        }
        // A stream that died before any text leaves no turn of the model's to save.
        let nothing_kept =
            matches!(request.go_on, Some(GoOn::Stalled(_))) && canonical.parts.is_empty();
        if !empty && !nothing_kept {
            // Thinking this server takes back is part of the next request's size.
            if thought && self.provider.replays_reasoning() {
                let thinking = canonical.parts.iter().filter_map(|part| match part {
                    ContentPart::Reasoning(part) => part.text.as_deref(),
                    _ => None,
                });
                self.context
                    .note_thinking_sent_back(&thinking.collect::<String>());
            }
            // Text followed only by tidying up is a finished answer.
            state.tidied_after_text = !assistant.content.trim().is_empty()
                && !intents.is_empty()
                && intents
                    .iter()
                    .all(|call| crate::empty_reply::tidies_up(&call.tool));
            self.log
                .append(Event::model_message(cx.session, &canonical))
                .await?;
            history.ordered_messages.push(canonical);
            history.messages.push(assistant);
        }
        Ok(ControlFlow::Continue(Accepted {
            intents,
            usage,
            empty,
            thought,
            said_again,
        }))
    }

    pub(super) async fn continue_response(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        request: &mut RequestState,
    ) -> Flow<bool> {
        // A reply that stopped early is saved above; the model is asked to go
        // on from it, unless it keeps stopping or the chat has no room left.
        if let Some(why) = request.go_on.take() {
            state.cut_offs += 1;
            let cut_offs = state.cut_offs;
            let window = self.provider.context_window().map(u64::from);
            let rounds = crate::cut_off::ROUNDS - 1;
            let no_room = |prompt: Option<u64>| {
                prompt.zip(window).is_some_and(|(prompt, window)| {
                    prompt.saturating_add(crate::cut_off::HEADROOM) >= window
                })
            };
            let mut stopped = "cut off";
            let (asked, telling, full) = match why {
                GoOn::Cut(prompt) => (
                    crate::cut_off::CONTINUE.to_owned(),
                    format!(
                        "The reply was cut off at the model's output limit. Asking it to \
                             continue ({cut_offs}/{rounds})…"
                    ),
                    no_room(prompt),
                ),
                GoOn::Dropped(prompt) => {
                    stopped = "interrupted";
                    (
                        crate::cut_off::RESUME.to_owned(),
                        format!(
                            "The connection dropped while the model was writing. What it \
                                 wrote is kept; asking it to continue ({cut_offs}/{rounds})…"
                        ),
                        no_room(prompt),
                    )
                }
                GoOn::Stalled(tools) => {
                    let limits = self.provider.model_limits();
                    let allowance = self
                        .provider
                        .requested_output_tokens()
                        .or(limits.max_output_tokens);
                    (
                        crate::cut_off::split_advice(&tools, allowance),
                        format!(
                            "The connection kept dropping while the model wrote a large \
                                 action. Asking it to work in smaller steps ({cut_offs}/{rounds})…"
                        ),
                        false,
                    )
                }
            };
            if state.cut_offs < crate::cut_off::ROUNDS && !full {
                let mut event = Event::user_input(cx.session, &asked, TrustLabel::Tool);
                event.payload["length_continuation"] = serde_json::json!(state.cut_offs);
                self.save_feedback(history, event, asked).await?;
                cx.sink.notice(&telling);
                return Ok(ControlFlow::Continue(true));
            }
            cx.sink.notice(&match full {
                true => format!(
                    "The reply was {stopped}, and the chat already fills the model's \
                         window, so it was not continued. What was written is kept."
                ),
                false => format!(
                    "The reply was still {stopped} after being continued. What was \
                         written is kept."
                ),
            });
        }
        Ok(ControlFlow::Continue(false))
    }

    pub(super) async fn retry_empty_response(
        &self,
        cx: &RunContext<'_>,
        history: &mut History,
        state: &mut RunState,
        interrupts: &mut Option<crate::interrupts::InterruptQueue>,
        accepted: &Accepted,
    ) -> Flow<bool> {
        let Accepted {
            intents,
            usage,
            empty,
            thought,
            said_again,
        } = accepted;
        let (usage, empty, thought, said_again) = (*usage, *empty, *thought, *said_again);
        // An empty reply after an answer and its tidying up ends the turn as
        // it is. Any other is nudged or asked for again, then another model
        // is tried, and only then is the person told.
        if empty && !state.tidied_after_text && !said_again {
            let after_tools = history
                .messages
                .last()
                .is_some_and(|m| m.role == Role::Tool);
            if after_tools && !state.empty_nudged {
                state.empty_nudged = true;
                let asked = crate::empty_reply::NUDGE;
                let mut event = Event::user_input(cx.session, asked, TrustLabel::Tool);
                event.payload["empty_reply"] = serde_json::json!(true);
                self.save_feedback(history, event, asked).await?;
                cx.sink.notice(
                    "The model answered its tools' results with nothing. Asking it to carry on…",
                );
                return Ok(ControlFlow::Continue(true));
            }
            // Twice in a row with no output at all: a third try would be the same.
            let silent = !thought && usage.is_none_or(|used| used.completion_tokens == 0);
            let certain = silent && state.empty_was_silent;
            state.empty_was_silent = silent;
            let cost = self
                .price()
                .zip(usage)
                .map(|(price, used)| price.cost(&used));
            let tries = match cost {
                Some(cost) if cost >= crate::empty_reply::COSTLY_USD => 1,
                _ => crate::empty_reply::TRIES,
            };
            if state.empty_tries < tries && !certain {
                state.empty_tries += 1;
                let empty_tries = state.empty_tries;
                // A model that thought is on a server that answers: no wait helps it.
                let wait = match thought {
                    true => std::time::Duration::ZERO,
                    false => crate::empty_reply::wait(state.empty_tries, jitter_nanos()),
                };
                cx.sink.notice(&match thought {
                    true => format!(
                        "The model thought but wrote no answer. Asking again \
                             ({empty_tries}/{tries})…"
                    ),
                    false => format!(
                        "The model answered with nothing. Asking again in {}s \
                             ({empty_tries}/{tries})…",
                        wait.as_secs()
                    ),
                });
                let heard = interrupts.as_ref().map(|q| q.activity());
                if !matches!(
                    waited_out(wait, cx.cancel, heard, cx.sink).await,
                    Waited::Stopped
                ) {
                    return Ok(ControlFlow::Continue(true));
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
            }
            let rest = crate::recovery::Rest::NextMessage;
            let Some((was, now)) = self.provider.fall_back("", rest) else {
                return Err(KernelError::Provider(crate::empty_reply::GAVE_UP.into()));
            };
            cx.sink.notice(&format!(
                "{was} kept answering with nothing. Using {now} for the rest of this \
                     turn; your next message tries {was} again."
            ));
            state.stood_in = Some((was, now));
            (state.empty_tries, state.empty_was_silent) = (0, false);
            return Ok(ControlFlow::Continue(true));
        }
        if !empty {
            (state.empty_tries, state.empty_was_silent) = (0, false);
            // Each round of tools may be followed by one nudge of its own.
            state.empty_nudged &= intents.is_empty();
        }
        Ok(ControlFlow::Continue(false))
    }
}
