
use super::*;

/// What keeps a quiet stream alive. A provider that builds a large tool
/// call before sending it may deliver nothing but empty deltas for minutes
/// and is still at work; one that only holds the connection open is not.
#[test]
fn any_delivered_event_is_activity_and_a_held_open_line_is_not() {
    let mut decoder = SseDecoder::default();
    let delivered = |bytes: &[u8], decoder: &mut SseDecoder| {
        decoder
            .push(bytes)
            .unwrap()
            .iter()
            .any(SseEvent::is_delivery)
    };
    for alive in [
            &br#"data: {"choices":[{"delta":{"content":"working"}}]}"#[..],
            br#"data: {"choices":[{"delta":{}}]}"#,
            br#"data: {"choices":[],"usage":{"prompt_tokens":10}}"#,
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{"}}]}}]}"#,
            b"data: [DONE]",
        ] {
            let record = [alive, b"\n\n"].concat();
            assert!(delivered(&record, &mut decoder), "{alive:?}");
        }
    for held_open in [
        &b": keep-alive\n\n"[..],
        b":\n\n",
        b"data:\n\n",
        b"data:   \n\n",
    ] {
        assert!(!delivered(held_open, &mut decoder), "{held_open:?}");
    }
}

#[test]
fn frames_lf_crlf_cr_and_mixed_endings() {
    for bytes in [
        b"data: one\n\n".as_slice(),
        b"data: one\r\n\r\n".as_slice(),
        b"data: one\r\r".as_slice(),
        b"data: one\r\n\n".as_slice(),
    ] {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.push(bytes).unwrap(),
            vec![SseEvent {
                event: None,
                data: "one".into(),
            }]
        );
    }
}

#[test]
fn arbitrary_byte_fragmentation_preserves_typed_multiline_events() {
    let fixture = b"event: content.delta\r\ndata: {\"value\":\r\ndata: \"hello\"}\r\n\r\n";
    for split in 0..=fixture.len() {
        let mut decoder = SseDecoder::default();
        let mut events = decoder.push(&fixture[..split]).unwrap();
        events.extend(decoder.push(&fixture[split..]).unwrap());
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("content.delta".into()),
                data: "{\"value\":\n\"hello\"}".into(),
            }],
            "split at byte {split}"
        );
    }
}

#[test]
fn finish_decodes_an_unterminated_final_record_and_ignores_comments() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b": keepalive\n").unwrap().is_empty());
    assert_eq!(
        decoder.finish(),
        None,
        "a comment-only record is not a protocol event"
    );

    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: final").unwrap().is_empty());
    assert_eq!(
        decoder.finish(),
        Some(SseEvent {
            event: None,
            data: "final".into(),
        })
    );
}

#[test]
fn unterminated_frames_cannot_grow_without_bound() {
    let mut decoder = SseDecoder::default();
    assert!(
        decoder
            .push(&vec![b'x'; MAX_FRAME_BYTES])
            .unwrap()
            .is_empty()
    );
    assert!(decoder.push(b"x").is_err());
    assert!(decoder.buffer.is_empty());
}
