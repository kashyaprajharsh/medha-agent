use super::*;
use base64::Engine as _;
use kernel::{Provider as _, VisionDescriber};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One canned completion per connection, returning every request body seen.
async fn endpoint(replies: usize) -> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let head = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..head]).to_lowercase();
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < head + len {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            seen.push(serde_json::from_slice(&bytes[head..head + len]).unwrap());
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"A login form with a misaligned submit button."},"finish_reason":"stop"}]}"#;
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
        seen
    });
    (address, server)
}

fn auxiliary(address: &str) -> AuxiliaryVision {
    let provider = providers::ProviderClient::new(address, "aux-key", "local/vision-model");
    provider.set_streaming(false);
    AuxiliaryVision::new(provider)
}

#[tokio::test]
async fn the_auxiliary_model_receives_the_image_and_returns_a_description() {
    kernel::auxiliary::scope(kernel::Budget::default(), String::new(), None, async {
        let (address, server) = endpoint(1).await;
        let aux = auxiliary(&address);

        let description = aux
            .describe("hash-1", "image/png", b"pixels")
            .await
            .unwrap();

        assert_eq!(description, "A login form with a misaligned submit button.");
        assert_eq!(aux.model(), "local/vision-model");
        let request = &server.await.unwrap()[0];
        assert_eq!(request["model"], "local/vision-model");
        let content = request["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[1]["image_url"]["url"],
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(b"pixels")
            )
        );
    })
    .await;
}

#[tokio::test]
async fn the_same_image_is_not_paid_for_twice() {
    kernel::auxiliary::scope(kernel::Budget::default(), String::new(), None, async {
        let (address, server) = endpoint(1).await;
        let aux = auxiliary(&address);

        let first = aux
            .describe("hash-1", "image/png", b"pixels")
            .await
            .unwrap();
        let second = aux
            .describe("hash-1", "image/png", b"pixels")
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            server.await.unwrap().len(),
            1,
            "the cached description must not reach the network"
        );
    })
    .await;
}

/// A real picture too large for a server comes back under the size asked for,
/// still a picture.
#[test]
fn a_picture_refused_for_its_size_is_made_to_fit() {
    use kernel::ImageShrinker as _;
    // Noise does not compress, so this PNG is far over the budget.
    let mut seed = 7u32;
    let pixels: Vec<u8> = (0..1024 * 1024 * 4)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let picture = media::from_rgba(1024, 1024, &pixels).unwrap();
    let budget = kernel::request_images::SMALL;
    assert!(picture.len() > budget * 4);

    let (bytes, kind, _) = Shrinker.shrink(&picture, budget, 8_000).unwrap();
    assert!(bytes.len() <= budget, "{} bytes", bytes.len());
    assert!(media::has_supported_image_header(&bytes));
    assert!(
        matches!(kind.as_str(), "image/png" | "image/jpeg"),
        "{kind}"
    );
}

/// A tall, flat screenshot is tiny in bytes and far too tall: it comes back
/// within the side asked for, and the side reported is the real one.
#[test]
fn a_picture_refused_for_its_height_is_made_to_fit() {
    use kernel::ImageShrinker as _;
    let picture = media::from_rgba(200, 9_000, &vec![255; 200 * 9_000 * 4]).unwrap();
    assert!(picture.len() < 100_000, "{} bytes", picture.len());

    let (bytes, _, side) = Shrinker.shrink(&picture, 3 * 1024 * 1024, 2_000).unwrap();
    let resized = media::normalize(bytes).unwrap();
    assert_eq!(side, resized.width.max(resized.height));
    assert!(side <= 2_000, "{side} pixels");
}

#[tokio::test]
async fn an_unreachable_auxiliary_model_fails_loudly() {
    kernel::auxiliary::scope(kernel::Budget::default(), String::new(), None, async {
        let aux = auxiliary("http://127.0.0.1:1/v1");
        let error = aux
            .describe("hash-1", "image/png", b"pixels")
            .await
            .unwrap_err();
        assert!(error.contains("auxiliary vision request failed"), "{error}");
    })
    .await;
}
