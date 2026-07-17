//! LLM-backed [`SkillJudge`]: MEDHA's own model reviews the guard's ambiguous
//! (Caution) findings in a single **tool-less** call — the escalation tier of
//! the two-tier skill review (regex first, judge for the gray zone). See
//! [`tools::judge`] for the trait, prompt, and parsing.
//!
//! Tool-less and single-shot on purpose: the judge reads untrusted skill
//! content, so with no tools there is nothing a prompt-injected package could
//! make it *do* — it can only emit a verdict. A timeout or any error surfaces as
//! `Err`, which the installer treats as fail-safe (keep the regex verdict, never
//! block a legitimate skill on a model hiccup).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use kernel::{Block, CompiledContext, Message, Provider};
use tools::judge::{self, JudgeOutcome, JudgeRequest, SkillJudge};

/// A security review must never hang an install; give up after this and fall
/// back to the regex verdict.
const JUDGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Reviews flagged skills with the session's configured model.
pub struct LlmJudge {
    provider: Arc<dyn Provider>,
}

impl LlmJudge {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl SkillJudge for LlmJudge {
    async fn judge(&self, request: JudgeRequest) -> Result<JudgeOutcome, String> {
        let (system, user) = judge::judge_prompt(&request);
        // No tools: a prompt-injected skill has nothing to make the judge *do*.
        let ctx = CompiledContext {
            model: String::new(),
            messages: vec![Message::system(system), Message::user(user)],
            tools: Vec::new(),
        };
        let text = tokio::time::timeout(
            JUDGE_TIMEOUT,
            collect_text(self.provider.as_ref(), &ctx),
        )
        .await
        .map_err(|_| "security review timed out".to_string())??;
        judge::parse_verdict(&text).ok_or_else(|| "security review gave no clear verdict".to_string())
    }
}

/// Drive one model call to completion, concatenating its text (ignoring
/// reasoning/usage/tool blocks — the judge has no tools and returns only JSON).
async fn collect_text(provider: &dyn Provider, ctx: &CompiledContext) -> Result<String, String> {
    let mut stream = provider.stream(ctx).await.map_err(|e| e.to_string())?;
    let mut out = String::new();
    while let Some(block) = stream.next().await {
        if let Block::Text(t) = block.map_err(|e| e.to_string())? {
            out.push_str(&t);
        }
    }
    Ok(out)
}
