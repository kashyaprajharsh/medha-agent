//! A private local channel between Medha's own processes: a Unix socket (a
//! named pipe on Windows), newline-delimited JSON frames, and a proof by each
//! side that it holds the token, without the token crossing the channel.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 16 * 1024 * 1024;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// The names each side signs under, so one side's proof is no use as the other's.
#[derive(Clone, Copy)]
pub struct Roles {
    pub host: &'static str,
    pub guest: &'static str,
}

/// Why a guest gave up on a host.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Closed,
    Silent,
    Unproven,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> bool {
    let mut line = frame.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok() && writer.flush().await.is_ok()
}

/// One frame, or `None` at end of stream or when a peer overruns the cap.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Value> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if read == 0 || line.len() > MAX_FRAME {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nonce() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(hex(&bytes))
}

/// Each side proves it holds the token without sending it.
fn proof(token: &str, role: &str, nonce: &str) -> String {
    mac(token, &format!("{role}:{nonce}"))
}

fn mac(secret: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(message.as_bytes());
    hex(&mac.finalize().into_bytes())
}

async fn answer<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Value> {
    tokio::time::timeout(HELLO_TIMEOUT, read_frame(reader))
        .await
        .ok()
        .flatten()
}

/// The guest's half. The host proves itself first, so whatever took its address
/// learns nothing. Returns the host's answer to the guest's own proof.
pub async fn greet<R, W>(
    reader: &mut R,
    writer: &mut W,
    token: &str,
    roles: Roles,
) -> Result<Value, Refusal>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let ours = nonce().ok_or(Refusal::Closed)?;
    let hello = json!({"id": 0, "method": "hello", "params": {"nonce": ours}});
    if !write_frame(writer, &hello).await {
        return Err(Refusal::Closed);
    }
    let challenge = answer(reader).await.ok_or(Refusal::Silent)?;
    let offered = challenge["result"]["proof"].as_str().unwrap_or_default();
    let theirs = challenge["result"]["nonce"].as_str().unwrap_or_default();
    if theirs.len() != ours.len() || !same_secret(offered, &proof(token, roles.host, &ours)) {
        return Err(Refusal::Unproven);
    }
    let prove = json!({"id": 1, "method": "prove",
        "params": {"proof": proof(token, roles.guest, theirs)}});
    if !write_frame(writer, &prove).await {
        return Err(Refusal::Closed);
    }
    answer(reader).await.ok_or(Refusal::Silent)
}

/// The host's half. Returns the id its first answer must carry, or `None` for a
/// peer that did not prove it holds the token.
pub async fn admit<R, W>(reader: &mut R, writer: &mut W, token: &str, roles: Roles) -> Option<Value>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = answer(reader).await?;
    let theirs = hello["params"]["nonce"].as_str().unwrap_or_default();
    let ours = nonce()?;
    if hello["method"] != "hello" || theirs.len() != ours.len() {
        return None;
    }
    let challenge = json!({"id": hello["id"],
        "result": {"proof": proof(token, roles.host, theirs), "nonce": ours}});
    if !write_frame(writer, &challenge).await {
        return None;
    }
    let prove = answer(reader).await?;
    let offered = prove["params"]["proof"].as_str().unwrap_or_default();
    if prove["method"] != "prove" || !same_secret(offered, &proof(token, roles.guest, &ours)) {
        return None;
    }
    Some(prove["id"].clone())
}

#[cfg(unix)]
pub async fn connect(address: &str) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(address).await
}

/// An address already taken: a guest that connects from now on is not refused.
#[cfg(unix)]
pub struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
pub fn bind(address: &str) -> std::io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(address);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener(listener))
}

#[cfg(unix)]
impl Listener {
    pub async fn serve(
        self,
        mut accept: impl FnMut(tokio::net::UnixStream),
    ) -> std::io::Result<()> {
        loop {
            let (stream, _) = self.0.accept().await?;
            accept(stream);
        }
    }
}

#[cfg(unix)]
pub async fn listen(
    address: &str,
    accept: impl FnMut(tokio::net::UnixStream),
) -> std::io::Result<()> {
    bind(address)?.serve(accept).await
}

#[cfg(windows)]
pub async fn connect(
    address: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(address)
}

/// An address already taken: a guest that connects from now on is not refused.
#[cfg(windows)]
pub struct Listener {
    address: String,
    server: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
pub fn bind(address: &str) -> std::io::Result<Listener> {
    let server = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(address)?;
    Ok(Listener {
        address: address.to_string(),
        server,
    })
}

#[cfg(windows)]
impl Listener {
    pub async fn serve(
        mut self,
        mut accept: impl FnMut(tokio::net::windows::named_pipe::NamedPipeServer),
    ) -> std::io::Result<()> {
        use tokio::net::windows::named_pipe::ServerOptions;
        loop {
            self.server.connect().await?;
            let next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.address)?;
            accept(std::mem::replace(&mut self.server, next));
        }
    }
}

#[cfg(windows)]
pub async fn listen(
    address: &str,
    accept: impl FnMut(tokio::net::windows::named_pipe::NamedPipeServer),
) -> std::io::Result<()> {
    bind(address)?.serve(accept).await
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
