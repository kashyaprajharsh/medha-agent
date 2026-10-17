use super::*;
use crate::{BpeCounter, PipelineEngine};
use kernel::{CompiledContext, ContextEngine, PreparedModelRequest, Protocol, TokenCountQuality};
use serde_json::json;

#[test]
fn estimates_include_tool_arguments_and_bound_binary_images() {
    let context = CompiledContext {
        model: "m".into(),
        messages: vec![],
        ordered: None,
        tools: vec![],
    };
    let plain = PreparedModelRequest::new(
        Protocol::OpenAiChat,
        "m",
        json!({"messages": [{"content":"hello"}]}),
        context,
    );
    let tool = plain.with_body(json!({"messages": [{"content":"hello"}], "tools":[{"description":"a fairly long tool schema".repeat(100)}]}));
    let engine = PipelineEngine::default();
    let a = engine.estimate_prepared(&plain).unwrap();
    let b = engine.estimate_prepared(&tool).unwrap();
    assert_eq!(a.quality, TokenCountQuality::LocalEstimate);
    assert!(b.tokens > a.tokens + 100);
    assert_eq!(b.request_fingerprint, tool.request_fingerprint);
    let image = plain.with_body(json!({"messages":[{"image_url":{"url":format!("data:image/png;base64,{}", "a".repeat(1024*1024))}}]}));
    let counter = BpeCounter::o200k();
    assert!(count(&image, &counter) < 5000);
    let raw = count(&plain, &counter);
    let engine = PipelineEngine::default();
    engine.estimate_prepared(&plain).unwrap();
    engine.update_usage((raw * 2) as u32, (raw * 2) as u32);
    let calibrated = engine.estimate_prepared(&plain).unwrap();
    assert_eq!(calibrated.tokens, raw * 2 + (raw * 2).div_ceil(10) + 256);
}

#[test]
#[ignore = "release-only measurement on real source text"]
fn release_estimate_cost_on_real_source() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "run with --release"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let text = std::fs::read_to_string(root.join("crates/kernel/src/loop_.rs")).unwrap();
    let request = PreparedModelRequest::new(
        Protocol::OpenAiChat,
        "m",
        json!({"messages":[{"role":"user","content":text}]}),
        CompiledContext {
            model: "m".into(),
            messages: vec![],
            ordered: None,
            tools: vec![],
        },
    );
    let engine = PipelineEngine::default();
    let start = std::time::Instant::now();
    let estimate = engine.estimate_prepared(&request).unwrap();
    let cold = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..20 {
        std::hint::black_box(engine.estimate_prepared(&request).unwrap());
    }
    println!(
        "release local estimate: source_bytes={}, estimated_input_tokens={}, cold_us={}, warm_mean_us={}",
        text.len(),
        estimate.tokens,
        cold.as_micros(),
        start.elapsed().as_micros() / 20
    );
}
