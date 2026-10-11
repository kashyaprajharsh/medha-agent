use super::*;
use crate::{MediaPart, ModelMessage, Role};
use base64::Engine;

/// An image named `label:bytes` is that many zero bytes; `gone` is missing.
struct Zeros;

fn stored(hash: &str) -> Result<usize, String> {
    let size = hash.split_once(':').ok_or("missing artifact")?.1;
    size.parse().map_err(|_| "missing artifact".to_string())
}

impl ArtifactStore for Zeros {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Err("read-only".into())
    }
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let size = stored(hash)?;
        let end = len.map_or(size, |len| (offset + len).min(size));
        Ok(vec![0; end.saturating_sub(offset)])
    }
    fn size(&self, hash: &str) -> Result<usize, String> {
        stored(hash)
    }
}

/// Returns an image exactly as large, in bytes and pixels, as it was allowed.
struct ToLimits;

impl ImageShrinker for ToLimits {
    fn shrink(&self, _: &[u8], budget: usize, max_side: u32) -> Result<Fitted, String> {
        Ok((vec![1; budget], "image/jpeg".into(), max_side))
    }
}

/// A stored image with its longest side in pixels, when that is recorded.
fn image(hash: &str, side: Option<u32>) -> ContentPart {
    ContentPart::Media(MediaPart {
        mime_type: "image/png".into(),
        source: MediaSource::Artifact(hash.into()),
        label: None,
        width: side,
        height: side.map(|_| 10),
        byte_size: None,
        provider_state: Vec::new(),
    })
}

fn message(trust: Option<TrustLabel>, parts: Vec<ContentPart>) -> ModelMessage {
    ModelMessage {
        role: Role::User,
        parts,
        trust,
    }
}

/// What a request carries: per message, each image's size on disk and type, or
/// `None` where a note stands in.
async fn resolved(
    messages: Vec<ModelMessage>,
    sending: Sending,
    max_side: u32,
) -> (Sent, Vec<Vec<Option<(usize, String)>>>) {
    let mut context = CompiledContext {
        model: "m".into(),
        messages: Vec::new(),
        ordered: Some(messages),
        tools: Vec::new(),
    };
    let shrinker: Arc<dyn ImageShrinker> = Arc::new(ToLimits);
    let store = Arc::new(Zeros);
    let sent = resolve_media(&mut context, store, sending, Some(&shrinker), max_side).await;
    let carried = context
        .ordered_messages()
        .iter()
        .map(|message| {
            message
                .parts
                .iter()
                .map(|part| match part {
                    ContentPart::Media(media) => match &media.source {
                        MediaSource::Base64(data) => {
                            let bytes = base64::engine::general_purpose::STANDARD.decode(data);
                            Some((bytes.unwrap().len(), media.mime_type.clone()))
                        }
                        other => panic!("an image left unresolved: {other:?}"),
                    },
                    _ => None,
                })
                .collect()
        })
        .collect();
    (sent, carried)
}

/// A long chat of screenshots: the same old ones stay left out for several
/// turns, a whole tool result at a time, and an attached image is never one.
#[tokio::test]
async fn old_tool_images_go_a_batch_at_a_time_and_attached_ones_never() {
    let shot = |hash: &str| image(hash, Some(100));
    let chat = |results: usize| {
        let mut messages = vec![message(Some(TrustLabel::User), vec![shot("mine:6")])];
        let pair = vec![shot("a:6"), shot("b:6")];
        messages.push(message(Some(TrustLabel::Tool), pair));
        messages.extend((1..results).map(|_| message(Some(TrustLabel::Web), vec![shot("shot:6")])));
        messages
    };
    let mut left_out = Vec::new();
    for results in 17..=28 {
        let (sent, carried) = resolved(chat(results), Sending::Whole, SIDE).await;
        assert!(carried[0][0].is_some(), "the attached image is always sent");
        assert_eq!(
            carried[1][0].is_some(),
            carried[1][1].is_some(),
            "a result's images stay or go together"
        );
        let oldest_gone = carried[1..].iter().take_while(|m| m[0].is_none());
        assert_eq!(
            oldest_gone.count(),
            carried.iter().filter(|m| m[0].is_none()).count(),
            "only the oldest go"
        );
        left_out.push(sent.retired);
    }
    // 1 attached + 2 + 17 single shots is 20; one more starts a batch of eight results.
    assert_eq!(left_out, [0, 0, 9, 9, 9, 9, 9, 9, 9, 9, 9, 17]);

    // Bytes are a hard limit, so even one of the newest three gives way to it.
    let heavy = (0..3)
        .map(|_| {
            let ContentPart::Media(mut big) = shot("big:6") else {
                unreachable!()
            };
            big.byte_size = Some(9_000_000);
            message(Some(TrustLabel::Tool), vec![ContentPart::Media(big)])
        })
        .collect();
    let (sent, carried) = resolved(heavy, Sending::Whole, SIDE).await;
    assert_eq!(sent.retired, 1);
    assert_eq!(
        (carried[0][0].is_none(), carried[2][0].is_some()),
        (true, true)
    );
}

/// What a refused request tries next: its large images smaller, then only the
/// newest and small. An image that cannot be read back is a note, not an error.
#[tokio::test]
async fn refused_images_are_sent_smaller_then_alone_and_a_lost_one_is_a_note() {
    let chat = || {
        let mine = vec![
            image("large:5000000", Some(1_000)),
            image("gone", Some(1_000)),
            image("little:6", Some(100)),
        ];
        let made = vec![image("newest:400000", Some(1_000))];
        vec![message(None, mine), message(Some(TrustLabel::Tool), made)]
    };
    let png = |bytes: usize| Some((bytes, "image/png".to_string()));
    let jpeg = |bytes: usize| Some((bytes, "image/jpeg".to_string()));

    let (sent, carried) = resolved(chat(), Sending::Whole, SIDE).await;
    assert_eq!((sent.images, sent.unreadable), (3, 1));
    assert_eq!(carried[0][1], None, "the lost image became a note");
    assert!(sent.largest > SHRINK_TO, "5 MB on disk is more as sent");
    assert_eq!(
        Sending::Whole.after_refusal(sent, true, SIDE),
        Sending::Shrunk
    );

    // The limit is on the image as sent, a third larger than it is on disk.
    let (sent, carried) = resolved(chat(), Sending::Shrunk, SIDE).await;
    assert_eq!(carried[0][0], jpeg(SHRINK_TO / 4 * 3));
    assert_eq!(sent.largest, SHRINK_TO);
    assert_eq!(carried[0][2], png(6), "small ones are left alone");
    assert_eq!(carried[1][0], png(400_000));
    assert_eq!(
        Sending::Shrunk.after_refusal(sent, true, SIDE),
        Sending::OneSmall
    );

    let (sent, carried) = resolved(chat(), Sending::OneSmall, SIDE).await;
    assert_eq!(carried[0], [None, None, None]);
    assert_eq!(carried[1][0], jpeg(SMALL / 4 * 3));
    assert_eq!(
        (sent.images, sent.largest, sent.widest),
        (1, SMALL, Some(SMALL_SIDE))
    );
    assert_eq!(
        Sending::OneSmall.after_refusal(sent, true, SIDE),
        Sending::AsText
    );

    // One small image refused leaves nothing smaller to try.
    let little = vec![message(None, vec![image("little:6", Some(100))])];
    let (sent, _) = resolved(little, Sending::Whole, SIDE).await;
    assert!(sent.one_small());
    assert_eq!(
        Sending::Whole.after_refusal(sent, true, SIDE),
        Sending::AsText
    );
}

/// A screenshot small in bytes but too tall for its server is resized and still
/// sent; one whose size in pixels is not recorded is checked, not assumed small.
#[tokio::test]
async fn an_image_too_wide_for_its_server_is_resized_not_left_out() {
    for (recorded, max_side) in [(Some(12_000), SIDE), (Some(3_000), 2_000), (None, SIDE)] {
        let tall = || vec![message(None, vec![image("tall:6", recorded)])];
        let (sent, _) = resolved(tall(), Sending::Whole, max_side).await;
        assert!(!sent.one_small(), "{recorded:?}");
        assert_eq!(
            Sending::Whole.after_refusal(sent, true, max_side),
            Sending::Shrunk,
            "{recorded:?}"
        );
        let (sent, carried) = resolved(tall(), Sending::Shrunk, max_side).await;
        assert!(carried[0][0].is_some(), "the image is still sent");
        assert_eq!(sent.widest, Some(max_side));
    }
    // Within both limits, nothing is re-encoded.
    let fine = vec![message(None, vec![image("fine:6", Some(7_000))])];
    let (sent, carried) = resolved(fine, Sending::Shrunk, SIDE).await;
    assert_eq!(carried[0][0], Some((6, "image/png".to_string())));
    assert_eq!(sent.widest, Some(7_000));
}

/// The side a server takes is read from its own words, in pixels or in patches,
/// and a number that makes no sense is not believed.
#[test]
fn the_side_a_server_takes_is_read_from_its_refusal() {
    for (refusal, side) in [
        (
            "At least one of the image dimensions exceed max allowed size: 8000 pixels",
            8_000,
        ),
        (
            "image dimensions exceed max allowed size for many-image requests: 2000 pixels",
            2_000,
        ),
        (
            "Invalid image: 41209 patches after processing, exceeding the limit of 30000",
            5_536,
        ),
        ("max allowed size: 99999 pixels", SIDE),
        ("max allowed size: 100 pixels", SIDE),
        ("invalid request body", SIDE),
    ] {
        assert_eq!(side_limit(refusal), side, "{refusal}");
    }
}
