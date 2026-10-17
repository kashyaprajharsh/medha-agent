use std::{io::Read, net::TcpStream};

pub fn request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "incomplete HTTP fixture request");
        request.extend_from_slice(&chunk[..read]);
        assert!(request.len() <= 2_000_000, "HTTP fixture request too large");
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            assert!(length <= 2_000_000);
            if request.len() >= end + 4 + length {
                return request;
            }
        }
    }
}
