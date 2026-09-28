use super::*;
use kernel::{ObsStatus, Observation, Session, ToolIntent, TrustLabel};
use serde_json::json;

fn write(session: &Session, id: &str, tool: &str, status: ObsStatus, payload: Value) -> [Event; 2] {
    [
        Event::model_intent(
            session,
            &ToolIntent {
                id: id.into(),
                tool: tool.into(),
                args: json!({}),
            },
        ),
        Event::tool_obs(
            session,
            &Observation {
                intent_id: id.into(),
                status,
                payload,
                media: Vec::new(),
                relayed_trust: None,
                net_denied: false,
            },
            TrustLabel::System,
        ),
    ]
}

fn with_args(mut pair: [Event; 2], args: Value) -> [Event; 2] {
    pair[0].payload["args"] = args;
    pair
}

#[test]
fn a_file_made_this_session_lists_each_edit_newest_first() {
    let session = Session::new();
    let events: Vec<Event> = [
        with_args(
            write(
                &session,
                "a",
                "edit",
                ObsStatus::Ok,
                json!({"path": "spec.json", "old": "", "new": "a\nb\nc\n"}),
            ),
            json!({"path": "spec.json", "content": "a\nb\nc\n"}),
        ),
        with_args(
            write(
                &session,
                "b",
                "edit",
                ObsStatus::Ok,
                json!({"path": "spec.json", "old": "a\nb\nc\n", "new": "x\ny\nz\n"}),
            ),
            json!({"path": "spec.json", "content": "x\ny\nz\n"}),
        ),
        with_args(
            write(
                &session,
                "c",
                "fs.edit",
                ObsStatus::Ok,
                json!({"path": "spec.json", "old": "x\ny\nz\n", "new": "x\nY\nz\n"}),
            ),
            json!({"path": "spec.json", "old_string": "y", "new_string": "Y"}),
        ),
    ]
    .concat();
    let file = &session_changes(&events)[0];
    let kinds: Vec<_> = file.history.iter().map(|edit| edit.kind).collect();
    assert_eq!(kinds, ["edited", "wrote", "created"]);
    let latest = &file.history[0];
    assert_eq!((latest.added, latest.removed), (1, 1));
    assert!(latest.diff.contains("-y\n+Y"));
    assert_eq!((file.added, file.removed, file.edits), (3, 0, 3));
}

#[test]
fn several_edits_to_one_file_are_one_net_change() {
    let session = Session::new();
    let events: Vec<Event> = [
        write(
            &session,
            "a",
            "edit",
            ObsStatus::Ok,
            json!({"path": "spec.json", "old": "one\ntwo\n", "new": "one\n2\n"}),
        ),
        write(
            &session,
            "b",
            "fs.edit",
            ObsStatus::Ok,
            json!({"path": "spec.json", "old": "one\n2\n", "new": "one\n2\nthree\n"}),
        ),
    ]
    .concat();
    let changes = session_changes(&events);
    assert_eq!(changes.len(), 1);
    let file = &changes[0];
    assert_eq!(
        (file.added, file.removed, file.edits, file.created),
        (2, 1, 2, false)
    );
    assert!(file.diff.contains("-two\n+2\n+three"));
    assert!(file.diff.starts_with("--- spec.json\n+++ spec.json\n"));
}

#[test]
fn a_new_file_is_marked_created_and_files_keep_their_first_touched_order() {
    let session = Session::new();
    let events: Vec<Event> = [
        write(
            &session,
            "a",
            "fs.write",
            ObsStatus::Ok,
            json!({"path": "b.md", "old": null, "new": "# B\n"}),
        ),
        write(
            &session,
            "b",
            "edit",
            ObsStatus::Ok,
            json!({"path": "a.md", "old": "x\n", "new": "y\n"}),
        ),
    ]
    .concat();
    let changes = session_changes(&events);
    let paths: Vec<_> = changes.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["b.md", "a.md"]);
    assert!(changes[0].created);
    assert_eq!((changes[0].added, changes[0].removed), (1, 0));
}

#[test]
fn failed_writes_reads_and_reverted_files_are_not_changes() {
    let session = Session::new();
    let events: Vec<Event> = [
        write(
            &session,
            "a",
            "edit",
            ObsStatus::Error,
            json!({"error": "no such file"}),
        ),
        write(
            &session,
            "b",
            "read",
            ObsStatus::Ok,
            json!({"path": "c.md", "old": "a", "new": "b"}),
        ),
        write(
            &session,
            "c",
            "edit",
            ObsStatus::Ok,
            json!({"path": "d.md", "old": "same\n", "new": "other\n"}),
        ),
        write(
            &session,
            "d",
            "edit",
            ObsStatus::Ok,
            json!({"path": "d.md", "old": "other\n", "new": "same\n"}),
        ),
    ]
    .concat();
    assert!(session_changes(&events).is_empty());
}
