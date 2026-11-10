//! One reservation/settlement route for non-chat model requests.

use crate::{
    Block, Budget, CompiledContext, EventLog, Governor, PreparedModelRequest, Pricing, Provider,
    ProviderError, Usage,
};
use futures::{StreamExt, stream::BoxStream};
use std::future::Future;
use std::sync::Arc;

/// Where a side call's usage goes: the chat it was made for.
#[derive(Clone)]
pub enum Recorder {
    /// Written to the chat's log as each call ends.
    Log {
        log: Arc<dyn EventLog>,
        session: ulid::Ulid,
    },
    /// Kept for the chat's first message, so an unused chat leaves no log.
    Held(Arc<std::sync::Mutex<Vec<serde_json::Value>>>),
}

struct Active {
    budget: Budget,
    route: String,
    pricing: Option<Pricing>,
    recorder: Option<Recorder>,
}
tokio::task_local! { static ACTIVE: Active; }

pub async fn scope<T>(
    budget: Budget,
    route: String,
    pricing: Option<Pricing>,
    recorder: Option<Recorder>,
    future: impl Future<Output = T>,
) -> T {
    ACTIVE
        .scope(
            Active {
                budget: budget.pooling(),
                route,
                pricing,
                recorder,
            },
            future,
        )
        .await
}

struct Charge {
    reservation: Option<crate::budgets::ModelReservation>,
    usage: Option<Usage>,
    pricing: Option<Pricing>,
    /// The chat, the side call's task and the model, for its usage record.
    record: Option<(Recorder, &'static str, String)>,
}

impl Charge {
    /// Writes what the side call used, as the main loop writes its own calls.
    async fn record(&mut self) {
        let (Some(usage), Some((recorder, task, identity))) = (self.usage, self.record.take())
        else {
            return;
        };
        let payload = serde_json::json!({
            "task": task,
            "identity": identity,
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "cached_prompt_tokens": usage.cached_prompt_tokens,
            "cache_write_tokens": usage.cache_write_tokens,
            "cost_usd": self.pricing.map(|pricing| pricing.cost(&usage)),
            "cost_indicative": self.pricing.map(|pricing| pricing.indicative),
        });
        match recorder {
            Recorder::Log { log, session } => {
                if let Err(error) = log.append(crate::Event::aux_usage(session, payload)).await {
                    tracing::warn!(%error, task, "could not record a side call's usage");
                }
            }
            Recorder::Held(held) => held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(payload),
        }
    }
}

impl Charge {
    fn finish(&mut self, error: Option<&ProviderError>) -> Result<(), ProviderError> {
        self.reservation
            .take()
            .map(|reservation| reservation.reconcile_attempt(self.usage, self.pricing, error))
            .transpose()
            .map(|_| ())
            .map_err(|stop| {
                ProviderError::Response(format!(
                    "auxiliary call reached the task's {}",
                    stop.label()
                ))
            })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let _ = self.finish(None);
    }
}

pub async fn context(
    provider: &dyn Provider,
    context: &CompiledContext,
    cap: u64,
    task: &'static str,
) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
    if ACTIVE.try_with(|_| ()).is_err() {
        return Err(ProviderError::Response(
            "auxiliary call unavailable: no active task allowance".into(),
        ));
    }
    let request = provider.prepare_request(context)?;
    let bounded = provider.with_output_limit(&request, cap)?;
    let output = bounded
        .as_ref()
        .map(|_| cap)
        .or(provider.requested_output_tokens())
        .or(provider.model_limits().max_output_tokens);
    let request = bounded.unwrap_or(request);
    let count = provider
        .count_input_tokens(&request)
        .await
        .ok()
        .flatten()
        .filter(|count| count.request_fingerprint == request.request_fingerprint);
    if provider.token_accounting_mode() == crate::TokenAccountingMode::Strict
        && count
            .as_ref()
            .is_none_or(|count| count.quality != crate::TokenCountQuality::Authoritative)
    {
        return Err(ProviderError::Response(
            "auxiliary call unavailable: authoritative input accounting required".into(),
        ));
    }
    let media = request
        .context
        .ordered_messages()
        .iter()
        .flat_map(|message| &message.parts)
        .any(|part| matches!(part, crate::ContentPart::Media(_)));
    let input = count.map(|count| count.tokens).or_else(|| {
        if media {
            provider
                .model_limits()
                .max_input_tokens
                .or(provider.model_limits().max_combined_tokens)
        } else {
            Some(request.body.to_string().len() as u64)
        }
    });
    prepared(provider, &request, input, output, task).await
}

pub async fn prepared(
    provider: &dyn Provider,
    request: &PreparedModelRequest,
    input: Option<u64>,
    output: Option<u64>,
    task: &'static str,
) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
    let (budget, pricing, recorder) = ACTIVE
        .try_with(|active| {
            (
                active.budget.clone(),
                if active.route == provider.context_identity() {
                    active.pricing.or_else(|| provider.list_price())
                } else {
                    provider.list_price()
                },
                active.recorder.clone(),
            )
        })
        .map_err(|_| {
            ProviderError::Response("auxiliary call unavailable: no active task allowance".into())
        })?;
    let mut governor = Governor::new(budget).reserve_for_finish();
    if let Some(stop) = governor.check() {
        return Err(ProviderError::Response(format!(
            "auxiliary call unavailable: task {}",
            stop.label()
        )));
    }
    let reservation = governor
        .reserve_model(input, output, pricing)
        .map_err(|stop| {
            ProviderError::Response(format!("auxiliary call unavailable: task {}", stop.label()))
        })?;
    let mut charge = Charge {
        reservation: Some(reservation),
        usage: None,
        pricing,
        record: recorder.map(|recorder| (recorder, task, provider.context_identity())),
    };
    let stream = match provider.stream_prepared(request).await {
        Ok(stream) => stream,
        Err(error) => {
            charge.finish(Some(&error))?;
            return Err(error);
        }
    };
    Ok(futures::stream::unfold(
        (stream, charge, false),
        |(mut stream, mut charge, finished)| async move {
            if finished {
                return None;
            }
            match stream.next().await {
                Some(Ok(Block::Usage(usage))) => {
                    charge.usage = Some(usage);
                    Some((Ok(Block::Usage(usage)), (stream, charge, false)))
                }
                Some(Err(error)) => {
                    let settled = charge.finish(Some(&error));
                    Some((Err(settled.err().unwrap_or(error)), (stream, charge, true)))
                }
                Some(block) => Some((block, (stream, charge, false))),
                None => {
                    charge.record().await;
                    charge
                        .finish(None)
                        .err()
                        .map(|error| (Err(error), (stream, charge, true)))
                }
            }
        },
    )
    .boxed())
}

#[cfg(test)]
#[path = "auxiliary_tests.rs"]
mod tests;
