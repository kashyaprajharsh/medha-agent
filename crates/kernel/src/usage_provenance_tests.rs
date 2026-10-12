use super::*;
use crate::{CompiledContext, Message, Provider, ProviderCaps, RequestScope, TrustLabel};

struct FixtureProvider(ProviderCaps);

#[async_trait::async_trait]
impl Provider for FixtureProvider {
    fn capabilities(&self) -> &ProviderCaps {
        &self.0
    }
    async fn stream(
        &self,
        _: &CompiledContext,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<crate::Block, crate::ProviderError>>,
        crate::ProviderError,
    > {
        unreachable!()
    }
}

#[test]
fn restored_usage_matches_the_same_source_boundaries_and_rejects_changed_content() {
    let provider = FixtureProvider(ProviderCaps {
        images: crate::ImageSupport::Unsupported,
        caching: true,
        max_ctx: None,
        tool_calls: crate::ToolCallStrategy::Native,
    });
    let scope = RequestScope {
        conversation: "fixture".into(),
        unattended: false,
    };
    let messages = vec![
        Message::system("Policy"),
        Message::tool_result("page", "Fetched content").carrying(TrustLabel::Web),
    ];
    let mut raw = CompiledContext {
        model: "fixture".into(),
        ordered: Some(messages.iter().map(Message::ordered).collect()),
        messages,
        tools: Vec::new(),
    };
    let sent = crate::provenance::for_model(&raw).into_owned();
    let prepared = provider.prepare_request_in(&sent, &scope).unwrap();
    let anchor = UsageAnchor::capture(
        provider.context_identity(),
        &prepared,
        &Usage {
            prompt_tokens: 100,
            ..Default::default()
        },
        None,
    );
    raw.messages.push(Message::user("Continue"));
    raw.ordered
        .as_mut()
        .unwrap()
        .push(Message::user("Continue").ordered());
    let restored = anchor.matching_context(&provider, &raw, &scope).unwrap();
    assert_eq!(restored.messages[1].content, "Fetched content");
    raw.messages[1].content = "Changed fetched content".into();
    raw.ordered.as_mut().unwrap()[1] = raw.messages[1].ordered();
    assert!(anchor.matching_context(&provider, &raw, &scope).is_none());
}
