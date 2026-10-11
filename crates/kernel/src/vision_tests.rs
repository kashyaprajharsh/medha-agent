use super::*;
use crate::{ArtifactStore, CompiledContext, MediaPart, ModelMessage, Role, TextPart};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct MemoryArtifacts(Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl ArtifactStore for MemoryArtifacts {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = format!("h{}", bytes.len());
        self.0.lock().unwrap().insert(hash.clone(), bytes.to_vec());
        Ok(hash)
    }
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let store = self.0.lock().unwrap();
        let bytes = store.get(hash).ok_or("missing artifact")?;
        let end = len.map_or(bytes.len(), |len| (offset + len).min(bytes.len()));
        Ok(bytes[offset.min(bytes.len())..end].to_vec())
    }
    fn size(&self, hash: &str) -> Result<usize, String> {
        Ok(self.0.lock().unwrap().get(hash).ok_or("missing")?.len())
    }
}

struct Describer {
    calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl VisionDescriber for Describer {
    async fn describe(&self, hash: &str, mime: &str, bytes: &[u8]) -> Result<String, String> {
        self.calls.lock().unwrap().push(hash.to_string());
        Ok(format!("a {mime} of {} bytes", bytes.len()))
    }
    fn model(&self) -> &str {
        "aux/vision"
    }
}

fn context_with(hashes: &[&str]) -> CompiledContext {
    CompiledContext {
        model: "text-only".into(),
        messages: Vec::new(),
        ordered: Some(
            hashes
                .iter()
                .map(|hash| ModelMessage {
                    role: Role::User,
                    parts: vec![
                        ContentPart::Text(TextPart {
                            text: "what is this?".into(),
                            provider_state: Vec::new(),
                        }),
                        ContentPart::Media(MediaPart {
                            mime_type: "image/png".into(),
                            source: MediaSource::Artifact((*hash).into()),
                            label: None,
                            width: None,
                            height: None,
                            byte_size: None,
                            provider_state: Vec::new(),
                        }),
                    ],
                    trust: None,
                })
                .collect(),
        ),
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn an_image_a_model_cannot_see_travels_as_a_labelled_description() {
    let store: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifacts::default());
    let hash = store.put(b"pixels").unwrap();
    let describer = Describer {
        calls: Mutex::new(Vec::new()),
    };
    let mut context = context_with(&[&hash]);

    let done = describe_media(&mut context, Arc::clone(&store), Some(&describer)).await;

    assert_eq!((done.described, done.not_shown), (1, 0));
    let parts = &context.ordered.as_ref().unwrap()[0].parts;
    assert!(
        !parts
            .iter()
            .any(|part| matches!(part, ContentPart::Media(_))),
        "no media may reach a route that cannot carry it"
    );
    let ContentPart::Text(described) = &parts[1] else {
        panic!("the image must become text")
    };
    assert_eq!(
        described.text,
        "[image unavailable to this model. aux/vision described it: a image/png of 6 bytes]"
    );
}

#[tokio::test]
async fn the_same_image_is_described_once_per_request() {
    let store: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifacts::default());
    let hash = store.put(b"pixels").unwrap();
    let describer = Describer {
        calls: Mutex::new(Vec::new()),
    };
    let mut context = context_with(&[&hash, &hash]);

    assert_eq!(
        describe_media(&mut context, Arc::clone(&store), Some(&describer))
            .await
            .described,
        2
    );
    assert_eq!(describer.calls.lock().unwrap().len(), 1);
}

/// A text model with no vision model beside it still gets its turn: the image
/// is replaced by a note it reads, never sent and never silently dropped. The
/// same holds when the vision model cannot be reached.
#[tokio::test]
async fn an_image_no_model_can_read_becomes_a_note_and_the_turn_goes_on() {
    let store: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifacts::default());
    let hash = store.put(b"pixels").unwrap();
    let texts = |context: &crate::CompiledContext| -> Vec<String> {
        context.ordered.as_ref().unwrap()[0]
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.clone()),
                ContentPart::Media(_) => panic!("an image reached a model that cannot read it"),
                _ => None,
            })
            .collect()
    };

    let mut context = context_with(&[&hash]);
    let done = describe_media(&mut context, Arc::clone(&store), None).await;
    assert_eq!((done.described, done.not_shown, done.failure), (0, 1, None));
    assert!(
        texts(&context).contains(
            &"[image not shown: this model cannot read images, and no vision model is configured \
          to describe it]"
                .to_owned()
        )
    );

    struct Unreachable;
    #[async_trait::async_trait]
    impl VisionDescriber for Unreachable {
        async fn describe(&self, _: &str, _: &str, _: &[u8]) -> Result<String, String> {
            Err("connection refused".into())
        }
        fn model(&self) -> &str {
            "aux/vision"
        }
    }
    let mut context = context_with(&[&hash]);
    let done = describe_media(&mut context, store, Some(&Unreachable)).await;
    assert_eq!(done.not_shown, 1);
    assert_eq!(done.failure.as_deref(), Some("connection refused"));
    assert!(texts(&context).contains(
        &"[image not shown: this model cannot read images, and aux/vision could not describe it]"
            .to_owned()
    ));
}
