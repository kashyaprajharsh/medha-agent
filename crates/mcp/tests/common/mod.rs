use tokio::{io::AsyncReadExt, net::TcpStream};

/// Whether a request says who sent it. Some vendors refuse one that does not,
/// so the stand-in servers refuse it too.
#[allow(dead_code)]
pub fn named(request: &str) -> bool {
    request.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("user-agent") && value.trim().starts_with("Medha/")
        })
    })
}

/// Reads through the declared body because TCP reads may be partial.
pub async fn read_http_request(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let content_length = String::from_utf8_lossy(&buf[..head_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= head_end + 4 + content_length {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}
