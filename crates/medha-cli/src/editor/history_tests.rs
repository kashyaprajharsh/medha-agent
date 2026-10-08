use super::*;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;

fn record(id: &str, kind: &str, payload: Value) -> protocol::HistoryRecord {
    protocol::HistoryRecord {
        id: id.into(),
        kind: kind.into(),
        payload,
        trust: "User".into(),
        source: "operator".into(),
    }
}

#[tokio::test]
async fn canonical_text_reasoning_and_tool_calls_are_replayed_once_across_compaction() {
    let (output, input) = tokio::io::duplex(1024 * 1024);
    let (writer, output_task) = crate::chat::output_writer(output, 256);
    let reader = tokio::spawn(async move {
        let mut input = tokio::io::BufReader::new(input).lines();
        let mut frames = Vec::new();
        while let Some(line) = input.next_line().await.unwrap() {
            frames.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        frames
    });
    let mut replay = Replay::default();
    for event in [
        record(
            "input",
            "user.message",
            json!({"text":"first", "attachments":null}),
        ),
        record(
            "retry",
            "user.message",
            json!({"text":"first", "attachments":null, "retry_of":"input"}),
        ),
        record("reason", "model.reasoning", json!({"text":"thought"})),
        record("compat", "model.text", json!({"text":"answer"})),
        record(
            "canonical",
            "model.message",
            json!({"parts":[
            {"type":"reasoning", "text":"thought"}, {"type":"text", "text":"answer"},
            {"type":"tool_call", "id":"call", "tool":"read", "args":{"path":"file"}}]}),
        ),
        record(
            "intent",
            "model.intent",
            json!({"id":"call", "tool":"read", "args":{"path":"file"}}),
        ),
        record(
            "obs",
            "tool.observation",
            json!({"intent_id":"call", "tool":"read", "status":"ok", "payload":{"text":"file contents"}}),
        ),
        record(
            "compact",
            "context.compaction",
            json!({"summary":"do not replace visible history"}),
        ),
        record("second", "user.message", json!({"text":"second"})),
    ] {
        replay.record(&writer, "session", event).await;
    }
    replay.flush(&writer, "session").await;
    output_task.finish(&writer).await;
    let frames = reader.await.unwrap();
    let content = |kind: &str| -> String {
        frames
            .iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == kind)
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect()
    };
    assert_eq!(content("user_message_chunk"), "firstsecond");
    assert_eq!(content("agent_message_chunk"), "answer");
    assert_eq!(content("agent_thought_chunk"), "thought");
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call")
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["params"]["update"]["status"] == "completed")
            .count(),
        1
    );
}
