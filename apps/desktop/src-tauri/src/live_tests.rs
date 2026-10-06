use super::*;
use std::sync::mpsc::sync_channel;

fn clean(frame: Value) -> Value {
    sanitize(frame, &mut Steps::default())
}

#[test]
fn a_later_page_of_a_stored_read_is_named_after_its_file() {
    let mut steps = Steps::default();
    sanitize(
        json!({ "method": "event", "params": { "kind": "tool.call", "id": "r1", "tool": "read",
        "args": { "path": "docs/WHAT_IS_MEDHA.md" } } }),
        &mut steps,
    );
    sanitize(
        json!({ "method": "event", "params": { "kind": "tool.observation", "id": "r1", "tool": "read", "ok": true,
        "payload": { "content": "…", "hash": "64d557f8bb" } } }),
        &mut steps,
    );
    let page = sanitize(
        json!({ "method": "event", "params": { "kind": "tool.call", "id": "r2", "tool": "read",
        "args": { "hash": "64d557f8bb", "offset": 14000, "length": 20000 } } }),
        &mut steps,
    );
    assert_eq!(
        page["params"]["target"],
        "docs/WHAT_IS_MEDHA.md · 14,000–34,000"
    );
}

#[test]
fn a_tool_call_is_a_verb_and_target_not_raw_args() {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": { "kind": "tool.call", "id": "c1", "tool": "agent.spawn",
            "args": { "name": "read-medha-doc", "objective": "Read the doc" } }
    });
    let sent = clean(frame);
    assert_eq!(sent["params"]["verb"], "Started agent");
    assert_eq!(sent["params"]["target"], "read-medha-doc");
    assert_eq!(sent["params"]["input_label"], "Task");
    assert_eq!(sent["params"]["input"], "Read the doc");
    assert!(sent["params"].get("args").is_none());
}

#[test]
fn a_plan_update_carries_its_steps() {
    let sent = clean(
        json!({ "method": "event", "params": { "kind": "tool.call", "id": "p1", "tool": "update_plan",
        "args": { "steps": [{ "title": "Map the diff", "status": "completed" }] } } }),
    );
    assert_eq!(sent["params"]["plan"]["steps"][0]["status"], "completed");
}

#[test]
fn a_tool_result_carries_readable_output_and_a_reason_when_it_fails() {
    let ok = clean(
        json!({ "method": "event", "params": { "kind": "tool.observation", "id": "c1", "tool": "shell.exec", "ok": true,
        "payload": { "stdout": "2 files\n", "stderr": "", "exit_code": 0 } } }),
    );
    assert_eq!(ok["params"]["output"], "2 files");
    assert!(ok["params"].get("payload").is_none());
    assert!(ok["params"].get("detail").is_none());
    let failed = clean(
        json!({ "method": "event", "params": { "kind": "tool.observation", "id": "c2", "tool": "read", "ok": false,
        "payload": { "error": "read failed: No such file" } } }),
    );
    assert_eq!(failed["params"]["detail"], "read failed: No such file");
    let unavailable = clean(json!({ "method": "event", "params": {
        "kind": "tool.observation", "id": "c3", "tool": "mcp__remote__search", "ok": false,
        "payload": { "error_code": "tool_unavailable", "error": "Tool is unavailable",
            "available_tools": ["read"] }
    } }));
    assert_eq!(unavailable["params"]["error_code"], "tool_unavailable");
    assert_eq!(unavailable["params"]["detail"], "Tool is unavailable");
    assert!(unavailable["params"].get("output").is_none());
}

#[test]
fn other_frames_pass_through_untouched() {
    let frame = json!({ "method": "event", "params": { "kind": "model.text", "delta": "hi" } });
    assert_eq!(clean(frame.clone()), frame);
}

fn text(delta: &str) -> Value {
    json!({ "method": "event", "params": { "kind": "model.text", "delta": delta } })
}

#[test]
fn streamed_text_is_rendered_as_markdown_so_far() {
    let mut segment = String::new();
    render(text("**Bold"), &mut segment);
    let frame = render(text("** and `code`"), &mut segment);
    let html = frame["params"]["html"].as_str().unwrap();
    assert!(html.contains("<strong>Bold</strong>"));
    assert!(html.contains("<code>code</code>"));
}

#[test]
fn a_tool_call_starts_a_new_text_segment() {
    let mut segment = String::new();
    render(text("Let me look."), &mut segment);
    render(
        json!({ "method": "event", "params": { "kind": "tool.call", "tool": "ls" } }),
        &mut segment,
    );
    let frame = render(text("Found it."), &mut segment);
    assert_eq!(frame["params"]["html"], "<p>Found it.</p>");
}

#[test]
fn a_restarted_response_does_not_reuse_abandoned_markdown() {
    let mut segment = String::new();
    render(text("Abandoned **response"), &mut segment);
    render(
        json!({ "method": "event", "params": { "kind": "model.restarted" } }),
        &mut segment,
    );
    let frame = render(text("Replacement."), &mut segment);
    assert_eq!(frame["params"]["html"], "<p>Replacement.</p>");
}

#[test]
fn desktop_controls_do_not_expose_arbitrary_tool_dispatch() {
    let sessions = LiveSessions::new(PathBuf::from("."));
    for method in ["tool.call", "shell.exec", "agent.spawn", "unknown"] {
        assert_eq!(
            sessions.request("draft", method, json!({})).unwrap_err(),
            format!("{method} is not a desktop request")
        );
    }
}

#[test]
fn history_shows_what_the_assistant_said_as_markdown_and_nothing_else() {
    let page = render_history(json!({ "next_cursor": "c", "events": [
        { "kind": "user", "text": "**mine**" },
        { "kind": "assistant", "text": "**bold** <img src=x onerror=alert(1)>" },
        { "kind": "reasoning", "text": "*thinking*" },
    ]}));
    let html = page["events"][1]["html"].as_str().unwrap();
    assert!(html.contains("<strong>bold</strong>") && !html.contains("<img"));
    assert!(page["events"][0].get("html").is_none());
    assert!(page["events"][2].get("html").is_none());
    assert_eq!(page["next_cursor"], "c");
}

#[test]
fn streamed_html_escapes_model_markup() {
    let mut segment = String::new();
    let frame = render(text("<img src=x onerror=alert(1)>"), &mut segment);
    assert!(!frame["params"]["html"].as_str().unwrap().contains("<img"));
}

#[test]
fn only_ulids_can_be_resumed() {
    assert!(is_ulid("01M3CEMMYGTBYNFBAERDWV1A0W"));
    assert!(!is_ulid("--no-sandbox"));
    assert!(!is_ulid("01M3CEMMYGTBYNFBAERDWV1A0W --no-sandbox"));
}

#[test]
fn keys_are_plain_tokens() {
    assert!(is_token("draft-3"));
    assert!(is_token("01M3CEMMYGTBYNFBAERDWV1A0W"));
    assert!(!is_token(""));
    assert!(!is_token("../etc"));
}

#[test]
fn streaming_flushes_on_deadlines_controls_metadata_and_size_boundaries() {
    let now = Instant::now();
    let mut stream = StreamFrames::default();
    assert!(stream.push(text("**first"), now).is_empty());
    assert!(stream.push(text(" second**"), now).is_empty());
    let approval = json!({"method": "approval", "params": {"gate_id": 1}});
    let ready = stream.push(approval.clone(), now);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0]["params"]["delta"], "**first second**");
    assert_eq!(ready[1], approval);
    assert!(stream.deadline.is_none());
    assert!(stream.push(text("old"), now).is_empty());
    let ready = stream.push(text("new"), now + STREAM_INTERVAL);
    assert_eq!(ready[0]["params"]["delta"], "old");
    assert_eq!(stream.flush().unwrap()["params"]["delta"], "new");
    assert!(stream.push(text("a"), now).is_empty());
    let ready = stream.push(
        json!({"method": "event", "params": {"kind": "model.reasoning", "delta": "b"}}),
        now,
    );
    assert_eq!(ready[0]["params"]["delta"], "a");
    assert_eq!(stream.flush().unwrap()["params"]["kind"], "model.reasoning");
    assert!(stream.push(text("c"), now).is_empty());
    let mut tagged = text("d");
    tagged["params"]["tag"] = json!("different");
    assert_eq!(stream.push(tagged, now)[0]["params"]["delta"], "c");
    stream.flush();
    let ready = stream.push(text(&"x".repeat(STREAM_BYTES)), now);
    assert_eq!(ready.len(), 1);
    assert!(stream.pending.is_none());
}

#[test]
fn stream_batching_preserves_all_text_and_reduces_rendering_work() {
    let now = Instant::now();
    let mut stream = StreamFrames::default();
    let delta = "Some **Markdown** and `code`.\n";
    let frames: Vec<_> = (0..1000).map(|_| text(delta)).collect();
    let start = Instant::now();
    let mut old_segment = String::new();
    let mut old_bytes = 0;
    for frame in &frames {
        old_bytes += render(frame.clone(), &mut old_segment).to_string().len();
    }
    let unbatched = start.elapsed();
    let start = Instant::now();
    let mut segment = String::new();
    let mut ready = Vec::new();
    for frame in frames {
        ready.extend(stream.push(frame, now));
    }
    ready.extend(stream.flush()); // Also covers an EOF with pending text.
    let count = ready.len();
    let mut bytes = 0;
    for frame in ready {
        bytes += render(frame, &mut segment).to_string().len();
    }
    let batched = start.elapsed();
    assert_eq!(segment, old_segment);
    assert_eq!(segment, delta.repeat(1000));
    assert_eq!(count, 1);
    assert!(bytes * 100 < old_bytes);
    eprintln!(
        "1000-chunk replay: unbatched={unbatched:?}, batched={batched:?}, serialized bytes={old_bytes}->{bytes}, renders=1000->{count}"
    );
}

#[test]
fn idle_stream_flushes_without_waiting_for_another_chunk_and_eof_keeps_the_tail() {
    let (send, receive) = sync_channel(4);
    let (emitted, observed) = sync_channel(4);
    let worker =
        std::thread::spawn(move || pump_stream(receive, |frame| emitted.send(frame).unwrap()));
    send.send(text("first")).unwrap();
    // A paused provider must still show its text; no new input triggers this.
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(2)).unwrap()["params"]["delta"],
        "first"
    );
    send.send(text("tail")).unwrap();
    drop(send);
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(2)).unwrap()["params"]["delta"],
        "tail"
    );
    worker.join().unwrap();
    assert!(observed.try_recv().is_err());
}

#[test]
fn steering_splits_the_markdown_segment_without_repeating_the_earlier_response() {
    let now = Instant::now();
    let mut stream = StreamFrames::default();
    let mut segment = String::new();
    assert!(stream.push(text("Before **steering**."), now).is_empty());
    let boundary = json!({"method":"event", "params":{"kind":"message.steered", "content":"change direction"}});
    let ready = stream.push(boundary.clone(), now);
    assert_eq!(ready.len(), 2);
    let before = render(ready[0].clone(), &mut segment);
    assert_eq!(
        before["params"]["html"],
        "<p>Before <strong>steering</strong>.</p>"
    );
    assert_eq!(render(ready[1].clone(), &mut segment), boundary);
    let after = render(text("After steering."), &mut segment);
    assert_eq!(after["params"]["html"], "<p>After steering.</p>");
}
