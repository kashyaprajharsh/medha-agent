//! Loopback-only synthetic HTTP fixtures. Never uses an installed credential.
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type RecordedRequest = (String, Vec<u8>);

pub struct Response {
    pub status: &'static str,
    pub headers: String,
    pub body: String,
    pub delay: std::time::Duration,
}
impl Response {
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: "200 OK",
            headers: "Content-Type: application/json\r\n".into(),
            body: body.into(),
            delay: std::time::Duration::ZERO,
        }
    }
    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            headers: "Content-Type: text/event-stream\r\n".into(),
            ..Self::json(body)
        }
    }
}
pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn scripted(responses: Vec<Response>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                let (head, body) = loop {
                    let mut chunk = [0; 8192];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "request ended early");
                    input.extend_from_slice(&chunk[..n]);
                    assert!(input.len() < 1024 * 1024);
                    if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8(input[..end].to_vec()).unwrap();
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if input.len() >= end + 4 + length {
                            break (head, input[end + 4..end + 4 + length].to_vec());
                        }
                    }
                };
                seen.lock().unwrap().push((head, body));
                tokio::time::sleep(response.delay).await;
                let header = format!(
                    "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.headers,
                    response.body.len()
                );
                if socket.write_all(header.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(response.body.as_bytes()).await;
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
