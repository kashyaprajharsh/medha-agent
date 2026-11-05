//! Browser sign-in plumbing every sign-in shares: a one-shot loopback listener that catches the
//! redirect, and the system browser. Only a person's action should reach either.

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const DONE: &str = "<!doctype html><meta charset=utf-8><title>Medha</title>\
<body style=\"font:16px system-ui;padding:3rem\">\
<p>Signed in. You can close this tab and return to Medha.</p>";
const FAILED: &str =
    "<!doctype html><meta charset=utf-8><p>Sign-in did not finish. Return to Medha for details.";
const WAITING: &str = "<!doctype html><meta charset=utf-8><p>Waiting for the sign-in redirect…";

/// A listener on 127.0.0.1, bound before the browser opens: its port is part of the redirect.
pub struct Listener {
    inner: TcpListener,
    port: u16,
}

/// Bind `preferred` when it is free, else any free port.
pub async fn bind(preferred: Option<u16>) -> std::io::Result<Listener> {
    let inner = match preferred {
        Some(port) => match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind("127.0.0.1:0").await?,
        },
        None => TcpListener::bind("127.0.0.1:0").await?,
    };
    let port = inner.local_addr()?.port();
    Ok(Listener { inner, port })
}

/// The redirect's decoded query: it carries a grant (`code` and `state`) or an `error`.
pub struct Redirect(Vec<(String, String)>);

impl Redirect {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

impl Listener {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Answer requests until one with this sign-in's `state` carries the grant or an error. Anything
    /// else (a favicon, a stray page, another sign-in) is told to wait, and a silent connection is dropped.
    pub async fn redirect(self, state: &str) -> std::io::Result<Redirect> {
        let (found, mut arrived) = tokio::sync::mpsc::channel(1);
        loop {
            let (stream, peer) = tokio::select! {
                accepted = self.inner.accept() => accepted?,
                Some(redirect) = arrived.recv() => return Ok(redirect),
            };
            // Bound to loopback already; anything else is refused rather than parsed.
            if !peer.ip().is_loopback() {
                continue;
            }
            let (found, state) = (found.clone(), state.to_owned());
            tokio::spawn(async move {
                if let Some(redirect) = answer(stream, &state).await {
                    let _ = found.send(redirect).await;
                }
            });
        }
    }
}

/// One connection: its redirect when it ends this sign-in, after the browser is answered.
async fn answer(stream: tokio::net::TcpStream, state: &str) -> Option<Redirect> {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut request),
    );
    if !matches!(read.await, Ok(Ok(_))) {
        return None;
    }
    let target = request.split_whitespace().nth(1).unwrap_or_default();
    let redirect = Redirect(query_pairs(target));
    let ours = redirect.get("state") == Some(state);
    let granted = ours && redirect.get("code").is_some();
    let failed = ours && redirect.get("error").is_some();
    let body = match (failed, granted) {
        (true, _) => FAILED,
        (false, true) => DONE,
        (false, false) => WAITING,
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    (failed || granted).then_some(redirect)
}

/// The `state` an authorization link carries, which only its own redirect brings back.
pub fn state_of(link: &str) -> Option<String> {
    url::Url::parse(link)
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
}

/// Percent-decoded query pairs from a raw request target (`/callback?a=b`).
fn query_pairs(target: &str) -> Vec<(String, String)> {
    url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map(|url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Best effort: callers show the URL first, so a headless machine still works.
/// Windows uses `rundll32`, not `cmd /C start`: `cmd` treats `&` as a command
/// separator, which truncated authorization URLs and ran the remainder.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (program, args) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, args) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (program, args): (&str, Vec<&str>) = ("", Vec::new());

    if !program.is_empty() {
        let _ = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
