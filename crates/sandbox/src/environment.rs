//! Transport settings follow execution authority, independently of model HTTP.

use crate::exec::{ExecError, ExecRequest};

pub const TRANSPORT: [&str; 7] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
];

pub fn is_transport(name: &str) -> bool {
    TRANSPORT.contains(&name)
}

pub(crate) fn prepare(request: &ExecRequest, network: bool) -> Result<ExecRequest, ExecError> {
    prepare_with(request, network, |name| std::env::var(name).ok())
}

fn prepare_with(
    request: &ExecRequest,
    network: bool,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<ExecRequest, ExecError> {
    let mut request = request.clone();
    if !network {
        request.env.retain(|(name, _)| !is_transport(name));
        return Ok(request);
    }
    for name in TRANSPORT {
        let value = request
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| read(name));
        request.env.retain(|(key, _)| key != name);
        let Some(mut value) = value.filter(|value| !value.is_empty()) else {
            continue;
        };
        if name == "SSL_CERT_FILE" {
            let path = std::path::Path::new(&value).canonicalize().map_err(|_| {
                ExecError::Unavailable(
                    "SSL_CERT_FILE must identify a readable certificate file".into(),
                )
            })?;
            if !path.is_file() || permissions::is_protected(&path) {
                return Err(ExecError::Unavailable(
                    "SSL_CERT_FILE cannot open a credential path or a directory".into(),
                ));
            }
            request.read_roots.push(path.clone());
            value = path.to_string_lossy().into_owned();
            if value.contains([',', '\n', '\r']) {
                return Err(ExecError::Unavailable(
                    "SSL_CERT_FILE path cannot be represented safely in a container mount".into(),
                ));
            }
        } else {
            kernel::redact::remember_stored(&value);
            if let Ok(url) = url::Url::parse(&value)
                && let Some(password) = url.password()
            {
                kernel::redact::remember_stored(password);
            }
        }
        request.env.push((name.into(), value));
    }
    Ok(request)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn clear_denied(command: &mut std::process::Command, network: bool) {
    if !network {
        for name in TRANSPORT {
            command.env_remove(name);
        }
    }
}

pub(crate) fn container_arguments(request: &ExecRequest, network: bool) -> Vec<String> {
    if !network {
        return Vec::new();
    }
    let mut args = Vec::new();
    for (name, value) in &request.env {
        if !is_transport(name) {
            continue;
        }
        if name == "SSL_CERT_FILE" {
            args.extend([
                "--mount".into(),
                format!("type=bind,src={value},dst=/run/medha-ca.pem,readonly"),
                "--env".into(),
                "SSL_CERT_FILE=/run/medha-ca.pem".into(),
            ]);
        } else {
            args.extend(["--env".into(), name.clone()]);
        }
    }
    args
}

pub fn redact(text: &str) -> String {
    kernel::redact::Secrets::default().redact(text).into_owned()
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
