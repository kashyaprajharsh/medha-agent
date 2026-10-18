//! One reservation/settlement route for non-chat model requests.

use crate::{
    Block, Budget, CompiledContext, Governor, PreparedModelRequest, Pricing, Provider,
    ProviderError, Usage,
};
use futures::{StreamExt, stream::BoxStream};
use std::future::Future;

struct Active {
    budget: Budget,
    route: String,
    pricing: Option<Pricing>,
}
tokio::task_local! { static ACTIVE: Active; }

pub async fn scope<T>(
    budget: Budget,
    route: String,
    pricing: Option<Pricing>,
    future: impl Future<Output = T>,
) -> T {
    ACTIVE
        .scope(
            Active {
                budget: budget.pooling(),
                route,
                pricing,
            },
            future,
        )
        .await
}

struct Charge {
    reservation: Option<crate::budgets::ModelReservation>,
    usage: Option<Usage>,
    pricing: Option<Pricing>,
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
    prepared(provider, &request, input, output).await
}

pub async fn prepared(
    provider: &dyn Provider,
    request: &PreparedModelRequest,
    input: Option<u64>,
    output: Option<u64>,
) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
    let (budget, pricing) = ACTIVE
        .try_with(|active| {
            (
                active.budget.clone(),
                if active.route == provider.context_identity() {
                    active.pricing.or_else(|| provider.list_price())
                } else {
                    provider.list_price()
                },
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
                None => charge
                    .finish(None)
                    .err()
                    .map(|error| (Err(error), (stream, charge, true))),
            }
        },
    )
    .boxed())
}

#[cfg(test)]
#[path = "auxiliary_tests.rs"]
mod tests;
