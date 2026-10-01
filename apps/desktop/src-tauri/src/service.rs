use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The read-only `medha desktop-service` process: session list and history.
pub struct Service {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Service {
    pub fn start(workspace: &Path) -> Result<Self, String> {
        let mut child = Command::new(backend_executable()?)
            .arg("desktop-service")
            .arg("--workspace")
            .arg(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("Could not start Medha backend: {error}"))?;
        let input = child.stdin.take().ok_or("Backend stdin unavailable")?;
        let output = child.stdout.take().ok_or("Backend stdout unavailable")?;
        let mut service = Self {
            child,
            input,
            output: BufReader::new(output),
            next_id: 1,
        };
        let hello = service.request("hello", None, None)?;
        if hello["protocol_version"] != 1 {
            return Err("Medha backend protocol version is incompatible".into());
        }
        Ok(service)
    }

    pub fn request(
        &mut self,
        method: &str,
        session_id: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Value, String> {
        self.exchange(json!({ "method": method, "session_id": session_id, "cursor": cursor }))
    }

    pub fn request_params(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.exchange(json!({ "method": method, "params": params }))
    }

    fn exchange(&mut self, mut request: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = json!(id);
        writeln!(self.input, "{request}")
            .and_then(|()| self.input.flush())
            .map_err(|error| format!("Backend request failed: {error}"))?;
        let mut line = String::new();
        if self
            .output
            .read_line(&mut line)
            .map_err(|error| format!("Backend reply failed: {error}"))?
            == 0
        {
            return Err("Medha backend stopped unexpectedly".into());
        }
        let response: Value = serde_json::from_str(&line)
            .map_err(|error| format!("Invalid backend reply: {error}"))?;
        if response["id"] != id {
            return Err("Backend reply ID did not match request".into());
        }
        if let Some(error) = response["error"].as_str() {
            return Err(error.to_owned());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "Backend reply has no result".into())
    }
}

#[cfg(test)]
impl Service {
    /// Any child with pipes: lifecycle tests never send it a request.
    pub(crate) fn stand_in() -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn backend_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MEDHA_DESKTOP_BACKEND") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "medha.exe" } else { "medha" };
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .filter(|path| path.is_file())
    {
        return Ok(path);
    }
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!(
            "medha-{}{}",
            env!("TAURI_ENV_TARGET_TRIPLE"),
            suffix
        ));
    binary
        .is_file()
        .then_some(binary)
        .ok_or_else(|| "Medha backend is missing. Run npm run prepare:backend.".into())
}
