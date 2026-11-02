//! Reading a large file or a stored result to its end on a small model only ever pages: no page is stored again.
#![cfg(unix)]

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use kernel::{
    AllowAll, AutoDeny, Block, Budget, CompileResult, CompiledContext, ContextEngine, InMemoryLog,
    Kernel, Message, NoVerify, Provider, ProviderCaps, ProviderError, Session, StreamSink,
    ToolCallStrategy, ToolIntent,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tools::ToolRegistry;

const STORED: &str = "The whole result is stored";

/// Follows every continuation it is shown: file pages, then a large command's stored output.
struct Reader {
    caps: ProviderCaps,
    calls: Mutex<(usize, Option<String>, Vec<String>)>,
}

impl Reader {
    fn call(index: usize, tool: &str, args: Value) -> Vec<Block> {
        vec![Block::ToolIntent(ToolIntent {
            id: format!("call-{index}"),
            tool: tool.into(),
            args,
        })]
    }

    fn next(&self, last: &str) -> Vec<Block> {
        let mut calls = self.calls.lock().unwrap();
        let (count, hash, results) = &mut *calls;
        *count += 1;
        assert!(*count < 150, "reading never finished: {:?}", results.last());
        if *count > 1 {
            results.push(last.to_owned());
        }
        if *count == 1 {
            return Self::call(*count, "read", json!({"path": "big.txt"}));
        }
        // A file and a stored result are paged alike: by path, from the offset each page names.
        let paging = hash.clone().unwrap_or_else(|| "big.txt".into());
        if let Some(offset) = last.split("continue with offset=").nth(1) {
            let offset: u64 = offset.trim_end_matches(']').trim().parse().unwrap();
            return Self::call(*count, "read", json!({"path": paging, "offset": offset}));
        }
        if hash.is_none() && !last.contains(STORED) {
            return Self::call(
                *count,
                "shell.exec",
                json!({"command": "seq 1 30000", "network": false}),
            );
        }
        if hash.is_none()
            && let Some(stored) = last.split("read path=\"").nth(1)
        {
            let stored = stored.split('"').next().unwrap().to_owned();
            *hash = Some(stored.clone());
            return Self::call(*count, "read", json!({"path": stored}));
        }
        vec![Block::Text("done".into())]
    }
}

#[async_trait]
impl Provider for Reader {
    fn capabilities(&self) -> &ProviderCaps {
        &self.caps
    }
    async fn stream(
        &self,
        ctx: &CompiledContext,
    ) -> Result<BoxStream<'static, Result<Block, ProviderError>>, ProviderError> {
        let last = ctx
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(stream::iter(self.next(&last).into_iter().map(Ok)).boxed())
    }
}

struct Passthrough;

#[async_trait]
impl ContextEngine for Passthrough {
    async fn compile(&self, messages: &[Message], _max_ctx: Option<u32>) -> CompileResult {
        CompileResult {
            messages: messages.to_vec(),
            source_indices: (0..messages.len()).map(Some).collect(),
            compacted: false,
            summarized: false,
            before_tokens: 0,
            after_tokens: 0,
            overflow: false,
            summary: None,
        }
    }
}

struct NoopSink;
impl StreamSink for NoopSink {}

#[tokio::test]
async fn a_small_model_pages_files_and_stored_results_to_the_end_without_storing_a_page() {
    // 8,000 tokens lands on the smallest result limit; 128,000 on the largest.
    for window in [8_000, 32_000, 128_000] {
        read_all(window).await;
    }
}

async fn read_all(window: u32) {
    let dir = test_support::scratch("medha-paging-e2e");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let line = "a line of a large file, long enough to matter\n";
    std::fs::write(workspace.join("big.txt"), line.repeat(6_000)).unwrap();
    let artifacts: Arc<dyn kernel::ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(dir.join("artifacts")).unwrap());
    // As every chat's sandbox is set up: the store is readable, so a note's path can be followed.
    let sandbox = Arc::new(
        sandbox::WorkspaceSandbox::new_jailed(&workspace)
            .unwrap()
            .with_readable_roots(&[dir.join("artifacts")]),
    );
    let reader = Arc::new(Reader {
        caps: ProviderCaps {
            images: kernel::ImageSupport::Unsupported,
            caching: false,
            max_ctx: Some(window),
            tool_calls: ToolCallStrategy::Native,
        },
        calls: Mutex::default(),
    });
    let kernel = Kernel::new(
        reader.clone(),
        Arc::new(InMemoryLog::new()),
        Arc::new(ToolRegistry::with_workspace(sandbox, artifacts.clone())),
        Arc::new(Passthrough),
        artifacts,
        Arc::new(AllowAll),
        Arc::new(AutoDeny),
        Arc::new(NoVerify),
    );
    kernel
        .run_session(
            &Session::new(),
            vec![Message::user("read it all")],
            Budget::default(),
            &NoopSink,
            None,
        )
        .await
        .unwrap();
    let (_, hash, results) = &*reader.calls.lock().unwrap();
    let stored: Vec<_> = results
        .iter()
        .filter(|result| result.contains(STORED))
        .collect();
    assert_eq!(
        stored.len(),
        1,
        "only the command's own output is stored: {stored:?}"
    );
    assert!(stored[0].starts_with("$ seq 1 30000"), "{}", stored[0]);
    assert!(hash.is_some(), "the stored output was read back");
    assert!(
        results.iter().any(|result| result.contains("6000|")),
        "the file was read to its last line"
    );
    let last = results.last().unwrap();
    assert!(
        last.trim_end().ends_with("|30000"),
        "the stored output was read to its last line: {last}"
    );
}
