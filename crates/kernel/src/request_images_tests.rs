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

struct ToBudget;

impl ImageShrinker for ToBudget {
    fn shrink(&self, _: &[u8], budget: usize) -> Result<(Vec<u8>, String), String> {
        Ok((vec![1; budget], "image/jpeg".into()))
    }
}

/// One message of images; `claimed` is the size each says it has.
fn message(trust: Option<TrustLabel>, hashes: &[&str], claimed: Option<usize>) -> ModelMessage {
    ModelMessage {
        role: Role::User,
        parts: hashes
            .iter()
            .map(|hash| {
                ContentPart::Media(MediaPart {
                    mime_type: "image/png".into(),
                    source: MediaSource::Artifact((*hash).into()),
                    label: None,
                    width: None,
                    height: None,
                    byte_size: claimed,
                    provider_state: Vec::new(),
                })
            })
            .collect(),
        trust,
    }
}

async fn resolved(
    messages: Vec<ModelMessage>,
    sending: Sending,
) -> (Sent, Vec<Vec<Option<(usize, String)>>>) {
    let mut context = CompiledContext {
        model: "m".into(),
        messages: Vec::new(),
        ordered: Some(messages),
        tools: Vec::new(),
    };
    let shrinker: Arc<dyn ImageShrinker> = Arc::new(ToBudget);
    let sent = resolve_media(&mut context, Arc::new(Zeros), sending, Some(&shrinker)).await;
    // Per message, each part: an image's decoded size and type, or `None` for a note.
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
    let chat = |results: usize| {
        let mut messages = vec![message(Some(TrustLabel::User), &["mine:6"], None)];
        messages.push(message(Some(TrustLabel::Tool), &["a:6", "b:6"], None));
        messages.extend((1..results).map(|_| message(Some(TrustLabel::Web), &["shot:6"], None)));
        messages
    };
    let mut left_out = Vec::new();
    for results in 17..=28 {
        let (sent, carried) = resolved(chat(results), Sending::Whole).await;
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
        .map(|_| message(Some(TrustLabel::Tool), &["big:6"], Some(9_000_000)))
        .collect();
    let (sent, carried) = resolved(heavy, Sending::Whole).await;
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
        vec![
            message(None, &["large:5000000", "gone", "little:6"], None),
            message(Some(TrustLabel::Tool), &["newest:400000"], None),
        ]
    };

    let (sent, carried) = resolved(chat(), Sending::Whole).await;
    assert_eq!(
        (sent.images, sent.unreadable, sent.largest),
        (3, 1, 5_000_000)
    );
    assert_eq!(Sending::Whole.after_refusal(sent, true), Sending::Shrunk);
    assert_eq!(carried[0][1], None, "the lost image became a note");

    let (sent, carried) = resolved(chat(), Sending::Shrunk).await;
    assert_eq!(carried[0][0], Some((SHRINK_TO, "image/jpeg".into())));
    assert_eq!(
        carried[0][2],
        Some((6, "image/png".into())),
        "small ones are left alone"
    );
    assert_eq!(Sending::Shrunk.after_refusal(sent, true), Sending::OneSmall);

    let (sent, carried) = resolved(chat(), Sending::OneSmall).await;
    assert_eq!(carried[0], [None, None, None]);
    assert_eq!(carried[1][0], Some((SMALL, "image/jpeg".into())));
    assert_eq!((sent.images, sent.largest), (1, SMALL));
    assert_eq!(Sending::OneSmall.after_refusal(sent, true), Sending::AsText);

    // One small image refused leaves nothing smaller to try.
    let (sent, _) = resolved(vec![message(None, &["little:6"], None)], Sending::Whole).await;
    assert_eq!(Sending::Whole.after_refusal(sent, true), Sending::AsText);
}
