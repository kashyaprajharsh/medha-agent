use super::*;
use crate::{ImageSupport, ProviderCaps, ToolCallStrategy};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Model {
    calls: AtomicUsize,
    caps: ProviderCaps,
    mode: u8,
}

impl Model {
    fn new(mode: u8) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            caps: ProviderCaps {
                images: ImageSupport::Unsupported,
                caching: false,
                max_ctx: None,
                tool_calls: ToolCallStrategy::Native,
            },
            mode,
        }
    }
}

#[async_trait::async_trait]
impl Provider for Model {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }
    async fn stream(
        &self,
        _: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.mode == 1 {
            return Err(ProviderError::NotSent("connect refused".into()));
        }
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_prompt_tokens: None,
            cache_write_tokens: None,
        };
        let mut blocks = vec![Ok(Block::Usage(usage)), Ok(Block::Text("answer".into()))];
        if self.mode == 2 {
            blocks.push(Err(ProviderError::Stream("after usage".into())));
        }
        Ok(futures::stream::iter(blocks).boxed())
    }
}

fn request(model: &Model) -> PreparedModelRequest {
    model
        .prepare_request(&CompiledContext {
            model: "test".into(),
            messages: Vec::new(),
            ordered: None,
            tools: Vec::new(),
        })
        .unwrap()
}

#[tokio::test]
async fn auxiliary_work_uses_the_same_pool_and_keeps_actual_usage_on_stream_failure() {
    let model = Model::new(2);
    let budget = Budget {
        max_tokens: Some(100),
        ..Budget::default()
    }
    .pooling();
    let request = request(&model);
    scope(budget.clone(), model.context_identity(), None, async {
        let result = prepared(&model, &request, Some(20), Some(20))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(result.last().unwrap().is_err());
    })
    .await;
    let governor = Governor::new(budget);
    assert_eq!(governor.task_tokens(), 15);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unfunded_work_does_not_send_and_proven_rejection_returns_its_reservation() {
    let model = Model::new(1);
    let budget = Budget {
        max_tokens: Some(100),
        ..Budget::default()
    }
    .pooling();
    let request = request(&model);
    scope(budget.clone(), model.context_identity(), None, async {
        assert!(
            prepared(&model, &request, Some(100), Some(1))
                .await
                .is_err()
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert!(
            prepared(&model, &request, Some(20), Some(20))
                .await
                .is_err()
        );
    })
    .await;
    assert_eq!(Governor::new(budget).task_tokens(), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn startup_spend_is_claimed_once_and_later_messages_get_fresh_pools() {
    let base = Budget {
        max_tokens: Some(100),
        ..Budget::default()
    }
    .for_startup();
    let mut startup = Governor::new(base.clone());
    startup.record_tokens(15, 0.0);
    let first = base.claim_startup().unwrap();
    assert_eq!(Governor::new(first).task_tokens(), 15);
    assert!(base.claim_startup().is_none());
    assert_eq!(Governor::new(base.with_fresh_pool()).task_tokens(), 0);
}
