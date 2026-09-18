use super::*;
use serde_json::json;

#[derive(Default)]
struct MemArtifacts(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl kernel::ArtifactStore for MemArtifacts {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = format!("h{}", bytes.len());
        self.0.lock().unwrap().insert(hash.clone(), bytes.to_vec());
        Ok(hash)
    }
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let map = self.0.lock().unwrap();
        let data = map.get(hash).ok_or("not found")?;
        let start = offset.min(data.len());
        let end = len.map_or(data.len(), |len| (start + len).min(data.len()));
        Ok(data[start..end].to_vec())
    }
    fn size(&self, hash: &str) -> Result<usize, String> {
        Ok(self.0.lock().unwrap().get(hash).map_or(0, Vec::len))
    }
}

struct Fixture {
    tool: ImageView,
    artifacts: Arc<dyn kernel::ArtifactStore>,
    dir: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!("medha-imgview-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let artifacts: Arc<dyn kernel::ArtifactStore> = Arc::new(MemArtifacts::default());
    Fixture {
        tool: ImageView {
            sbx: Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap()),
            artifacts: artifacts.clone(),
        },
        artifacts,
        dir,
    }
}

fn write_png(dir: &std::path::Path, name: &str) -> Vec<u8> {
    let bytes = media::from_rgba(3, 2, &[200; 3 * 2 * 4]).unwrap();
    std::fs::write(dir.join(name), &bytes).unwrap();
    bytes
}

#[tokio::test]
async fn the_model_gets_the_picture_not_a_text_extract() {
    let f = fixture();
    let bytes = write_png(&f.dir, "shot.png");

    let payload = f
        .tool
        .execute(&json!({ "path": "shot.png" }))
        .await
        .unwrap();

    assert_eq!(payload["width"], 3);
    assert_eq!(payload["height"], 2);
    assert_eq!(payload["mime"], "image/png");
    let media: Vec<kernel::MediaPart> =
        serde_json::from_value(payload[MEDIA].clone()).expect("media travels as parts");
    let kernel::MediaSource::Artifact(hash) = &media[0].source else {
        panic!("images must be stored, never inlined into the payload")
    };
    assert_eq!(f.artifacts.get(hash, 0, None).unwrap(), bytes);
    assert!(
        !payload.to_string().contains("base64"),
        "pixels must not enter the payload the model reads"
    );
}

#[tokio::test]
async fn the_envelope_is_lifted_out_of_what_the_model_reads() {
    let f = fixture();
    write_png(&f.dir, "shot.png");
    let mut payload = f
        .tool
        .execute(&json!({ "path": "shot.png" }))
        .await
        .unwrap();

    let media = take_media(&mut payload);

    assert_eq!(media.len(), 1);
    assert!(
        payload.get(MEDIA).is_none(),
        "the private key must be stripped before the model sees the result"
    );
}

#[tokio::test]
async fn a_text_file_is_refused_with_a_usable_reason() {
    let f = fixture();
    std::fs::write(f.dir.join("notes.txt"), b"not an image").unwrap();

    let error = f
        .tool
        .execute(&json!({ "path": "notes.txt" }))
        .await
        .expect_err("text is not an image");

    let message = error.to_string();
    assert!(message.contains("notes.txt"), "{message}");
    assert!(message.contains("expected a PNG"), "{message}");
}

#[tokio::test]
async fn a_missing_file_says_so_rather_than_returning_nothing() {
    let f = fixture();
    let error = f
        .tool
        .execute(&json!({ "path": "nope.png" }))
        .await
        .expect_err("a missing file cannot be viewed");
    assert!(error.to_string().contains("nope.png"), "{error}");
}
