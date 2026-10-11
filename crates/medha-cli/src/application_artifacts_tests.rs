use super::*;
use kernel::{Event, Observation, Session, ToolIntent, TrustLabel};

#[test]
fn nothing_a_reply_writes_can_name_or_join_an_output() {
    let session = Session::default();
    let reply = |text: &str| Event::model_text(&session, text);
    let events = vec![
        reply("```mermaid artifact=first\nflowchart LR\nA-->B\n```"),
        // The same ID again, on the opening line and inside the block: a model
        // trying to append to the first output, or copying an ID it was shown.
        reply("```mermaid artifact=first\nflowchart TD\nZ-->Q\n```"),
        reply("```mermaid\nartifact=first\nflowchart TD\nZ-->Q\n```"),
    ];
    let records = catalogue(&events);
    assert_eq!(records.len(), 3);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.predecessor, None);
        assert!(!record.artifact_id.contains("first"));
        for other in &records[index + 1..] {
            assert_ne!(record.artifact_id, other.artifact_id);
        }
    }
    assert_eq!(records[0].source.as_deref(), Some("flowchart LR\nA-->B\n"));
    // Whatever is inside the block stays the block's source, unedited.
    assert_eq!(
        records[2].source.as_deref(),
        Some("artifact=first\nflowchart TD\nZ-->Q\n")
    );
}

#[test]
fn files_a_reply_named_are_listed_live_after_its_own_blocks_and_version_by_path() {
    let session = Session::default();
    let naming = |text: &str, paths: &[&str]| {
        let mut event = Event::model_text(&session, text);
        event.payload["outputs"] = json!(
            paths
                .iter()
                .map(|path| json!({"path": path, "kind": "image"}))
                .collect::<Vec<_>>()
        );
        event
    };
    let events = vec![
        naming(
            "```svg\n<svg/>\n```\nMade ![](chart.png) and `plan.svg`",
            &["/w/chart.png", "/w/plan.svg"],
        ),
        naming("Redrew `chart.png`", &["/w/chart.png"]),
    ];
    let records = catalogue(&events);
    assert_eq!(records.len(), 4, "{records:#?}");
    let (block, chart, plan, again) = (&records[0], &records[1], &records[2], &records[3]);
    // A named svg file and the reply's svg block are two outputs in two places.
    assert_eq!(
        (block.kind.as_str(), plan.kind.as_str()),
        ("drawing", "drawing")
    );
    assert_ne!(block.source_index, plan.source_index);
    assert_ne!(block.source_index, chart.source_index);
    for file in [chart, plan, again] {
        assert!(file.snapshot.is_none(), "no copy is kept of a named file");
        assert!(file.path.is_some());
    }
    assert_eq!(chart.artifact_id, "file:/w/chart.png");
    assert_eq!(again.artifact_id, chart.artifact_id);
    assert_eq!(again.predecessor.as_ref(), Some(&chart.revision_id));
}

fn diagram(session: &Session, body: &str) -> Event {
    Event::model_text(session, &format!("```mermaid\nflowchart LR\n{body}\n```"))
}

fn asking(session: &Session, text: &str, about: Option<&ArtifactRevision>) -> Event {
    let mut message = kernel::Message::user(text);
    message.about = about.map(|record| kernel::OutputReference {
        artifact_id: record.artifact_id.clone(),
        revision_id: record.revision_id.clone(),
        label: String::new(),
        lead: 0,
    });
    Event::user_input_message(session, &message)
}

#[test]
fn a_message_about_an_output_makes_only_the_first_matching_reply_output_its_next_revision() {
    let session = Session::default();
    let mut events = vec![diagram(&session, "A-->B")];
    let first = catalogue(&events).remove(0);
    events.push(asking(&session, "make it blue", Some(&first)));
    events.push(Event::model_text(
        &session,
        "```svg\n<svg/>\n```\n\n```mermaid\nflowchart LR\nA-->C\n```\n\n```mermaid\nflowchart LR\nX-->Y\n```",
    ));
    events.push(asking(&session, "now something else", None));
    events.push(diagram(&session, "P-->Q"));
    let records = catalogue(&events);
    assert_eq!(records.len(), 5);
    let (drawing, second, extra, later) = (&records[1], &records[2], &records[3], &records[4]);
    assert_eq!(second.artifact_id, first.artifact_id);
    assert_eq!(second.predecessor.as_ref(), Some(&first.revision_id));
    for separate in [drawing, extra, later] {
        assert_ne!(separate.artifact_id, first.artifact_id);
        assert_eq!(separate.predecessor, None);
    }

    // Asking about the older version again branches from the one named.
    events.push(asking(&session, "back to the first", Some(&first)));
    events.push(diagram(&session, "A-->D"));
    let third = catalogue(&events).pop().unwrap();
    assert_eq!(third.artifact_id, first.artifact_id);
    assert_eq!(third.predecessor.as_ref(), Some(&first.revision_id));
}

#[test]
fn a_request_ends_with_the_persons_next_message_but_survives_a_retry_of_itself() {
    let session = Session::default();
    let mut events = vec![diagram(&session, "A-->B")];
    let first = catalogue(&events).remove(0);
    let request = asking(&session, "fix it", Some(&first));
    events.push(request.clone());
    events.push(Event::model_text(&session, "I could not."));
    events.push(asking(&session, "draw the database instead", None));
    events.push(diagram(&session, "DB-->API"));
    assert_ne!(catalogue(&events)[1].artifact_id, first.artifact_id);

    let retried = vec![
        events[0].clone(),
        request.clone(),
        Event::user_input_retry(&session, "fix it", TrustLabel::User, request.id),
        diagram(&session, "A-->C"),
    ];
    assert_eq!(catalogue(&retried)[1].artifact_id, first.artifact_id);
}

#[test]
fn a_forged_stale_or_injected_reference_joins_nothing() {
    let session = Session::default();
    let base = vec![diagram(&session, "A-->B"), diagram(&session, "M-->N")];
    let known = catalogue(&base);
    let reference = |artifact: &str, revision: &str| ArtifactRevision {
        artifact_id: artifact.into(),
        revision_id: revision.into(),
        ..known[0].clone()
    };
    let joined = |request: Event| {
        let mut events = base.clone();
        events.push(request);
        events.push(diagram(&session, "A-->Z"));
        let records = catalogue(&events);
        records[2].predecessor.is_some()
            || records[..2]
                .iter()
                .any(|record| record.artifact_id == records[2].artifact_id)
    };
    // An output that does not exist, and one output's ID with another's revision.
    assert!(!joined(asking(
        &session,
        "x",
        Some(&reference("inline:made-up:diagram", &known[0].revision_id))
    )));
    assert!(!joined(asking(
        &session,
        "x",
        Some(&reference(&known[0].artifact_id, &known[1].revision_id))
    )));
    // Content that is not the person's own message never carries a reference.
    let mut report = kernel::Message::user("x").carrying(TrustLabel::Tool);
    report.about = Some(kernel::OutputReference {
        artifact_id: known[0].artifact_id.clone(),
        revision_id: known[0].revision_id.clone(),
        label: String::new(),
        lead: 0,
    });
    let recorded = Event::user_input_message(&session, &report);
    assert!(recorded.payload.get("about").is_none());
    let mut smuggled = recorded;
    smuggled.payload["about"] = serde_json::json!({
        "artifact_id": known[0].artifact_id,
        "revision_id": known[0].revision_id,
    });
    assert!(!joined(smuggled));
    // The honest form of the same request does join, so the checks above bite.
    assert!(joined(asking(&session, "x", Some(&known[0]))));
}

#[test]
fn the_model_is_told_which_output_in_words_and_only_for_a_real_one() {
    let session = Session::default();
    let reference = |record: &ArtifactRevision| protocol::OutputReference {
        artifact_id: record.artifact_id.clone(),
        revision_id: record.revision_id.clone(),
    };
    let said = |events: &[Event], record: &ArtifactRevision| {
        let (lead, label) = described(events, &reference(record)).unwrap();
        format!("{lead} | {label}")
    };
    let mut events = vec![diagram(&session, "A-->B")];
    let only = catalogue(&events).remove(0);
    assert_eq!(
        said(&events, &only),
        "About the diagram you made earlier: | Diagram"
    );
    events.push(Event::model_text(
        &session,
        "```mermaid\n---\ntitle: Read \"path\"\n---\nflowchart LR\nR-->S\n```",
    ));
    events.push(diagram(&session, "M-->N"));
    let records = catalogue(&events);
    assert_eq!(
        said(&events, &records[1]),
        "About the diagram \"Read path\" you made earlier: | Diagram: Read path"
    );
    // Two untitled diagrams share a label, so place tells them apart.
    assert_eq!(
        said(&events, &records[2]),
        "About diagram 3 of the 3 you made in this chat: | Diagram"
    );
    events.push(asking(&session, "again", Some(&records[1])));
    events.push(diagram(&session, "R-->T"));
    assert_eq!(
        said(&events, &records[1]),
        "About the diagram \"Read path\" you made earlier, version 1 of 2: | Diagram: Read path"
    );
    // A surface shows the person's words and the label, never the sentence.
    let mut sent = kernel::Message::user("About the diagram you made earlier:\n\nmake it blue");
    sent.about = Some(kernel::OutputReference {
        artifact_id: only.artifact_id.clone(),
        revision_id: only.revision_id.clone(),
        label: "Diagram".into(),
        lead: "About the diagram you made earlier:\n\n".len(),
    });
    assert_eq!(sent.shown(), ("make it blue", Some("Diagram")));
    sent.about.as_mut().unwrap().lead = 10_000;
    assert_eq!(sent.shown().1, None);
    let mut unknown = reference(&records[1]);
    unknown.revision_id = "missing:code:0".into();
    assert_eq!(described(&events, &unknown), None);
    for text in events
        .iter()
        .filter_map(|event| event.payload["text"].as_str())
    {
        assert!(!text.contains("medha-output-"));
    }
}

#[test]
fn inline_indices_match_rendered_typed_fences_and_identical_replies_remain_distinct() {
    let session = Session::default();
    let text = "```\nordinary code\n```\n\n```rust\nlet x = 1;\n```\n\n```mermaid,example\nflowchart LR\nA-->B\n```";
    let first = Event::model_text(&session, text);
    let second = Event::model_text(&session, text);
    let records = catalogue(&[first.clone(), second.clone()]);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].source_index, 1);
    assert_eq!(records[0].source_event, first.id.to_string());
    assert_eq!(records[1].source_event, second.id.to_string());
    assert_ne!(records[0].artifact_id, records[1].artifact_id);
    assert_ne!(records[0].revision_id, records[1].revision_id);
}

#[test]
fn reused_provider_call_ids_do_not_merge_independent_drawings() {
    let session = Session::default();
    let event = || {
        let mut event = Event::observed_tool(
            &session,
            &Observation::ok("same-call", json!({})),
            TrustLabel::Tool,
            "mcp__draw__view",
        );
        event.payload["screen"] = json!({"server":"drawing", "tool":"view", "resource":"ui://draw", "input":{}, "result":{}});
        event
    };
    let first = event();
    let second = event();
    let records = catalogue(&[first, second]);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].tool_call, records[1].tool_call);
    assert_ne!(records[0].artifact_id, records[1].artifact_id);
    assert_ne!(records[0].source_event, records[1].source_event);
    assert!(records.iter().all(|record| record.predecessor.is_none()));
}

/// Screens are versions of one document only when their server says so in the
/// standard metadata, and never across two different server connections.
/// Fields a particular server happens to return link nothing.
#[test]
fn only_a_server_declared_document_links_screens_and_never_across_connections() {
    let session = Session::default();
    let screen = |identity: &str, document: Option<&str>| {
        let mut event = Event::observed_tool(
            &session,
            &Observation::ok("call", json!({})),
            TrustLabel::Tool,
            "mcp__draw__view",
        );
        event.payload["screen"] = json!({"identity":identity,"server":"drawing","resource":"ui://draw",
            "input":{"elements":"[{\"type\":\"restoreCheckpoint\",\"id\":\"a\"}]"},
            "result":{"structuredContent":{"checkpointId":"a"}}});
        if let Some(id) = document {
            event.payload["screen"]["result"]["_meta"] = json!({"io.medha.artifact":{"id":id}});
        }
        event
    };
    let records = catalogue(&[
        screen("server", Some("doc")),
        screen("server", None),
        screen("server", Some("doc")),
        screen("reconfigured", Some("doc")),
    ]);
    assert_eq!(records[2].artifact_id, records[0].artifact_id);
    assert_eq!(
        records[2].predecessor.as_ref(),
        Some(&records[0].revision_id)
    );
    assert_ne!(records[1].artifact_id, records[0].artifact_id);
    assert!(records[1].predecessor.is_none());
    assert_ne!(records[3].artifact_id, records[0].artifact_id);
}

#[test]
fn only_successful_writes_or_explicit_publications_are_file_outputs() {
    let session = Session::default();
    let call = |id: &str, tool: &str| {
        Event::model_intent(
            &session,
            &ToolIntent {
                id: id.into(),
                tool: tool.into(),
                args: json!({"path":"page.html"}),
            },
        )
    };
    let observation = |id: &str, text: &str| {
        Event::tool_obs(
            &session,
            &Observation::ok(id, json!({"path":"page.html", "new":text})),
            TrustLabel::Tool,
        )
    };
    let events = vec![
        call("read", "read"),
        observation("read", "input"),
        call("write", "write"),
        observation("write", "version one"),
        call("edit", "edit"),
        observation("edit", "version two"),
    ];
    let records = catalogue(&events);
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].snapshot.as_ref().unwrap().size,
        "version one".len()
    );
    assert_eq!(
        records[1].snapshot.as_ref().unwrap().size,
        "version two".len()
    );
    assert_ne!(
        records[0].snapshot.as_ref().unwrap().hash,
        records[1].snapshot.as_ref().unwrap().hash
    );
    assert_eq!(
        records[1].predecessor.as_ref(),
        Some(&records[0].revision_id)
    );
    assert!(catalogue(&[Event::model_text(&session, "I read `page.html`.")]).is_empty());
}

#[test]
fn failed_screens_are_not_outputs() {
    let session = Session::default();
    let screen = |id: &str, ok: bool| {
        let mut event =
            Event::tool_obs(&session, &Observation::ok(id, json!({})), TrustLabel::Tool);
        event.payload["screen"] = json!({"server":"drawing", "tool":"create_view", "resource":"ui://draw", "input":{}, "result":{"isError":!ok}});
        event
    };
    let records = catalogue(&[
        screen("a", true),
        screen("failed", false),
        screen("b", true),
    ]);
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].artifact_id, records[1].artifact_id);
}

#[test]
fn large_inline_sources_leave_small_metadata_pages_and_remain_addressable() {
    let session = Session::default();
    let events = [Event::model_text(
        &session,
        &format!("```mermaid\n{}\n```", "x".repeat(PAGE_BYTES)),
    )];
    let page = page(catalogue(&events), 0).unwrap();
    assert_eq!(page.omitted, 0);
    assert_eq!(page.revisions.len(), 1);
    assert!(page.revisions[0].source.is_none());
    assert!(page.revisions[0].source_digest.is_some());
    assert!(serde_json::to_vec(&page).unwrap().len() < 2048);
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn revision_reader_preserves_bytes_and_enforces_conversation_and_manifest_membership() {
    use base64::Engine;
    use kernel::EventLog;
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let store: Arc<dyn kernel::ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(dir.path().join("artifacts")).unwrap());
    let session = Session::new();
    let css = store
        .clone()
        .put_async(b".v1 {color:red}".to_vec())
        .await
        .unwrap();
    let mut revisions = Vec::new();
    for text in ["<html>version one</html>", "<html>version two</html>"] {
        let hash = store
            .clone()
            .put_async(text.as_bytes().to_vec())
            .await
            .unwrap();
        let event = Event::observed_tool(
            &session,
            &Observation::ok(
                "write",
                json!({tools::outputs::FIELD:{"path":"/synthetic/page.html","kind":"page","hash":hash,"size":text.len(),"assets":[{"path":"style.css","hash":css,"size":b".v1 {color:red}".len(),"mime":"text/css"}]}}),
            ),
            TrustLabel::Tool,
            "write",
        );
        revisions.push(format!("{}:file", log.append(event).await.unwrap().id));
    }
    let read_request = |session: String, revision: String, asset: Option<String>, offset: usize| {
        protocol::ReadArtifact {
            session_id: session,
            revision_id: revision,
            asset,
            offset,
        }
    };
    let first = read(
        &log,
        &store,
        read_request(session.id.to_string(), revisions[0].clone(), None, 0),
    )
    .await
    .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(first["data"].as_str().unwrap())
            .unwrap(),
        b"<html>version one</html>"
    );
    let asset = read(
        &log,
        &store,
        read_request(
            session.id.to_string(),
            revisions[1].clone(),
            Some("style.css".into()),
            0,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(asset["data"].as_str().unwrap())
            .unwrap(),
        b".v1 {color:red}"
    );
    assert!(
        read(
            &log,
            &store,
            read_request(Session::new().id.to_string(), revisions[0].clone(), None, 0)
        )
        .await
        .is_err()
    );
    assert!(
        read(
            &log,
            &store,
            read_request(
                session.id.to_string(),
                revisions[0].clone(),
                Some("../private".into()),
                0
            )
        )
        .await
        .is_err()
    );
    assert!(
        read(
            &log,
            &store,
            read_request(
                session.id.to_string(),
                revisions[0].clone(),
                None,
                usize::MAX
            )
        )
        .await
        .is_err()
    );
    let records = catalogue(&log.checked_events(session.id).await.unwrap());
    assert_eq!(records[1].predecessor.as_ref(), Some(&revisions[0]));
    assert_eq!(
        serde_json::to_value(&records).unwrap(),
        serde_json::to_value(catalogue(&log.output_events(session.id).await.unwrap())).unwrap()
    );
}

#[tokio::test]
async fn an_arbitrary_tool_cannot_forge_a_file_publication_manifest() {
    use kernel::EventLog;
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let session = Session::new();
    let result = Observation::ok(
        "remote",
        json!({tools::outputs::FIELD:{"path":"private.html","kind":"page","hash":"00","size":1}}),
    );
    let event = log
        .append(Event::observed_tool(
            &session,
            &result,
            TrustLabel::Tool,
            "mcp__remote__read",
        ))
        .await
        .unwrap();
    assert!(catalogue(std::slice::from_ref(&event)).is_empty());
    assert!(
        file(&log, &session.id.to_string(), &format!("{}:file", event.id))
            .await
            .unwrap_err()
            .contains("built-in")
    );
}

#[tokio::test]
async fn large_inline_revision_reads_are_chunked_without_losing_unicode() {
    use base64::Engine;
    use kernel::EventLog;
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let store: Arc<dyn kernel::ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(dir.path().join("artifacts")).unwrap());
    let session = Session::new();
    let source = format!("<html><body>{}</body></html>\n", "ह🙂".repeat(180_000));
    let event = log
        .append(Event::model_text(
            &session,
            &format!("```html\n{source}```"),
        ))
        .await
        .unwrap();
    let revision = format!("{}:code:0", event.id);
    let manifest = file(&log, &session.id.to_string(), &revision)
        .await
        .unwrap();
    assert_eq!(manifest.size, source.len());
    let mut bytes = Vec::new();
    while bytes.len() < manifest.size {
        let part = read(
            &log,
            &store,
            protocol::ReadArtifact {
                session_id: session.id.to_string(),
                revision_id: revision.clone(),
                asset: None,
                offset: bytes.len(),
            },
        )
        .await
        .unwrap();
        let part = base64::engine::general_purpose::STANDARD
            .decode(part["data"].as_str().unwrap())
            .unwrap();
        assert!(part.len() <= READ_CHUNK);
        bytes.extend(part);
    }
    assert_eq!(bytes, source.as_bytes());
}

#[tokio::test]
async fn a_large_screen_is_listed_small_and_read_back_exactly_as_recorded() {
    use base64::Engine;
    use kernel::EventLog;
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let store: Arc<dyn kernel::ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(dir.path().join("artifacts")).unwrap());
    let session = Session::new();
    let elements = json!([{"id":"text","text":"ह🙂".repeat(180_000)}]).to_string();
    let mut event = Event::tool_obs(
        &session,
        &Observation::ok("draw", json!({})),
        TrustLabel::Tool,
    );
    event.payload["screen"] = json!({"server":"drawing","resource":"ui://draw","input":{"elements":elements},"result":{}});
    let event = log.append(event).await.unwrap();
    let listed = page(catalogue(std::slice::from_ref(&event)), 0).unwrap();
    assert_eq!(listed.omitted, 0);
    assert!(listed.revisions[0].screen.is_none());
    let revision = &listed.revisions[0].revision_id;
    let manifest = file(&log, &session.id.to_string(), revision).await.unwrap();
    assert_eq!(
        manifest.hash,
        listed.revisions[0].snapshot.as_ref().unwrap().hash
    );
    assert!(serde_json::to_vec(&listed).unwrap().len() < 2048);
    let mut bytes = Vec::new();
    while bytes.len() < manifest.size {
        let result = read(
            &log,
            &store,
            protocol::ReadArtifact {
                session_id: session.id.to_string(),
                revision_id: revision.clone(),
                asset: None,
                offset: bytes.len(),
            },
        )
        .await
        .unwrap();
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(result["data"].as_str().unwrap())
            .unwrap();
        assert!(chunk.len() <= READ_CHUNK);
        bytes.extend(chunk);
    }
    let screen: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(screen, event.payload["screen"]);
}

#[tokio::test]
async fn historical_text_writes_are_read_from_verified_history_without_reading_disk() {
    use base64::Engine;
    use kernel::EventLog;
    let dir = tempfile::tempdir().unwrap();
    let log = store::SqliteLog::open(dir.path().join("events.db")).unwrap();
    let store: Arc<dyn kernel::ArtifactStore> =
        Arc::new(store::FileArtifactStore::open(dir.path().join("artifacts")).unwrap());
    let session = Session::new();
    log.append(Event::model_intent(
        &session,
        &ToolIntent {
            id: "old-edit".into(),
            tool: "edit".into(),
            args: json!({"path":"missing.html"}),
        },
    ))
    .await
    .unwrap();
    let event = log
        .append(Event::tool_obs(
            &session,
            &Observation::ok(
                "old-edit",
                json!({"path":"missing.html","new":"<html>exact old version</html>"}),
            ),
            TrustLabel::Tool,
        ))
        .await
        .unwrap();
    let records = catalogue(&log.checked_events(session.id).await.unwrap());
    assert_eq!(records.len(), 1);
    assert_eq!(log.output_events(session.id).await.unwrap().len(), 2);
    assert!(!records[0].snapshot.as_ref().unwrap().missing.is_empty());
    let data = read(
        &log,
        &store,
        protocol::ReadArtifact {
            session_id: session.id.to_string(),
            revision_id: format!("{}:file", event.id),
            asset: None,
            offset: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data["data"].as_str().unwrap())
            .unwrap(),
        b"<html>exact old version</html>"
    );
}
