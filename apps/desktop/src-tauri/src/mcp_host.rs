//! One `medha mcp-host` per app, restarted at the same address so chats reattach on their own.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

struct Endpoint {
    address: String,
    token: String,
}

static HOST: OnceLock<Option<Endpoint>> = OnceLock::new();

pub fn env() -> Option<(String, String)> {
    HOST.get_or_init(start)
        .as_ref()
        .map(|host| (host.address.clone(), host.token.clone()))
}

fn random_hex(bytes: usize) -> Option<String> {
    let mut random = vec![0u8; bytes];
    getrandom::fill(&mut random).ok()?;
    Some(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn start() -> Option<Endpoint> {
    let token = random_hex(32)?;
    // The address is visible to other local users, so it shares nothing with the token.
    let address = address(&random_hex(8)?);
    let backend = crate::service::backend_executable().ok()?;
    let (host_address, host_token) = (address.clone(), token.clone());
    std::thread::spawn(move || supervise(backend, host_address, host_token));
    wait_until_listening(&address);
    Some(Endpoint { address, token })
}

fn supervise(backend: std::path::PathBuf, address: String, token: String) {
    let mut delay = Duration::from_millis(500);
    loop {
        let started = std::time::Instant::now();
        let spawned = Command::new(&backend)
            .arg("mcp-host")
            .arg("--socket")
            .arg(&address)
            .env("MEDHA_MCP_HOST_TOKEN", &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            // The host exits when this pipe closes, which happens when the app does.
            let stdin = child.stdin.take();
            let _ = child.wait();
            drop(stdin);
        }
        if started.elapsed() > Duration::from_secs(30) {
            delay = Duration::from_millis(500);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}

#[cfg(unix)]
fn address(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("medha-{name}"))
        .join("mcp.sock")
        .display()
        .to_string()
}

#[cfg(windows)]
fn address(name: &str) -> String {
    format!(r"\\.\pipe\medha-mcp-{name}")
}

fn wait_until_listening(address: &str) {
    #[cfg(unix)]
    for _ in 0..40 {
        if std::path::Path::new(address).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(windows)]
    {
        let _ = address;
        std::thread::sleep(Duration::from_millis(300));
    }
}
