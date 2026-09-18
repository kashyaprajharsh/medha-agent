use super::*;
use base64::Engine;

/// 1×1 PNG, small enough to assert on the exact wire payload.
fn pixel_bytes() -> Vec<u8> {
    media::from_rgba(1, 1, &[10, 20, 30, 255]).unwrap()
}

fn pixel_base64() -> String {
    base64::engine::general_purpose::STANDARD.encode(pixel_bytes())
}

#[tokio::test]
async fn a_dropped_screenshot_survives_the_file_disappearing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("Screen Shot 1.txt");
    std::fs::write(&path, pixel_bytes()).unwrap();
    let store = Arc::new(store::FileArtifactStore::open(temp.path().join("artifacts")).unwrap());
    let attachments = ingest(vec![path.clone()], store.clone()).await.unwrap();
    std::fs::remove_file(&path).unwrap();

    let attachment = &attachments[0];
    assert_eq!(attachment.part.mime_type, "image/png");
    assert_eq!(attachment.label, "Screen Shot 1.txt");
    assert_eq!(attachment.source.as_deref(), Some(path.as_path()));
    assert_eq!((attachment.width, attachment.height), (1, 1));
    assert!(attachment.note.is_none());
    assert_eq!(
        attachment.summary(),
        format!(
            "Screen Shot 1.txt  1×1 · {}",
            human_size(pixel_bytes().len())
        )
    );
    assert_eq!(
        label_for(Path::new("/a/a-very-long-screenshot-name-indeed.png")),
        "a-very-long-screenshot-name-…"
    );
    let MediaSource::Artifact(hash) = &attachment.part.source else {
        panic!("attachments must travel as artifact references")
    };
    assert_eq!(store.get(hash, 0, None).unwrap(), pixel_bytes());
}

#[tokio::test]
async fn admission_reports_which_attachment_failed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("notes.txt");
    std::fs::write(&path, b"not an image").unwrap();
    let store = Arc::new(store::FileArtifactStore::open(temp.path().join("artifacts")).unwrap());
    let error = format!("{:#}", ingest(vec![path], store).await.unwrap_err());
    assert!(
        error.contains("notes.txt") && error.contains("expected a PNG"),
        "{error}"
    );
}

#[tokio::test]
async fn identical_images_are_stored_once() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(store::FileArtifactStore::open(temp.path().join("artifacts")).unwrap());
    let mut paths = Vec::new();
    for name in ["a.png", "b.png"] {
        let path = temp.path().join(name);
        std::fs::write(&path, pixel_bytes()).unwrap();
        paths.push(path);
    }
    let attachments = ingest(paths, store).await.unwrap();
    assert_eq!(attachments[0].part.source, attachments[1].part.source);
}

#[tokio::test]
async fn more_than_the_limit_is_refused_before_any_file_is_read() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(store::FileArtifactStore::open(temp.path().join("artifacts")).unwrap());
    let paths = vec![temp.path().join("missing.png"); MAX_PER_MESSAGE + 1];
    let error = ingest(paths, store).await.unwrap_err().to_string();
    assert!(error.contains("at most 4"), "{error}");
}

struct NoTools;

#[async_trait::async_trait]
impl kernel::Executor for NoTools {
    fn specs(&self) -> Vec<kernel::ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _: &kernel::ToolIntent) -> kernel::Observation {
        panic!("no tools expected")
    }
}

#[tokio::test]
async fn custom_chat_endpoint_receives_image_after_compaction_and_restart() {
    use kernel::{EventLog, Provider};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(n) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break n + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
            assert!(headers.starts_with("post /custom/v1/chat/completions "));
            assert!(headers.contains("authorization: bearer fixture-key"));
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + len {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            requests.push(
                serde_json::from_slice::<serde_json::Value>(&bytes[header_end..header_end + len])
                    .unwrap(),
            );
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"Image received"},"finish_reason":"stop"}]}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        requests
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("image.png");
    std::fs::write(&path, pixel_bytes()).unwrap();
    let store = Arc::new(store::FileArtifactStore::open(temp.path().join("artifacts")).unwrap());
    let images: Vec<_> = ingest(vec![path.clone()], store.clone())
        .await
        .unwrap()
        .into_iter()
        .map(|attachment| attachment.part)
        .collect();
    std::fs::remove_file(path).unwrap();
    let log = Arc::new(kernel::InMemoryLog::new());
    let session = kernel::Session::new();
    let runtime = || {
        let provider = providers::OpenAiCompat::new(
            format!("http://{address}/custom/v1"),
            "fixture-key",
            "local/custom-vision-model",
        )
        .with_max_ctx(16_000);
        provider.set_streaming(false);
        kernel::Kernel::new(
            Arc::new(provider),
            log.clone(),
            Arc::new(NoTools),
            Arc::new(context::PipelineEngine::with_counter(
                context::CompactionPolicy::default(),
                Arc::new(context::HeuristicCounter),
            )),
            store.clone(),
            Arc::new(kernel::AllowAll),
            Arc::new(kernel::AutoDeny),
            Arc::new(kernel::NoVerify),
        )
    };
    let mut history = vec![kernel::Message::system("Describe images accurately.")];
    history.extend(
        (0..80).map(|i| kernel::Message::user(format!("request {i}: {}", "x".repeat(800)))),
    );
    history[30].attachments = images.clone();
    let (_, stop) = runtime()
        .run_session(
            &session,
            history,
            kernel::Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, kernel::StopReason::Finished);
    let events = log.events(session.id).await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == kernel::EventKind::Compaction)
    );
    assert!(
        !events
            .iter()
            .any(|event| event.payload.to_string().contains(&pixel_base64())),
        "binary image data must not enter durable events"
    );
    let (_, stop) = runtime()
        .run_session(
            &session,
            vec![kernel::Message::user("Look again after restart")],
            kernel::Budget::default(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stop, kernel::StopReason::Finished);
    let requests = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    for request in requests {
        assert_eq!(request["model"], "local/custom-vision-model");
        let urls: Vec<_> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter_map(|part| part["image_url"]["url"].as_str())
            .collect();
        assert_eq!(urls, [format!("data:image/png;base64,{}", pixel_base64())]);
    }
}
