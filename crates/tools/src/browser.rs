//! `read` with `render: true`: headless Chromium, throwaway profile, offline unless granted.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{MEDIA, RELAYED_TRUST, ToolError, arg_str};

const RENDER_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct BrowserScreenshot {
    pub(crate) sbx: Arc<sandbox::WorkspaceSandbox>,
    pub(crate) artifacts: Arc<dyn kernel::ArtifactStore>,
    pub(crate) browser: PathBuf,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Target {
    /// A file inside the workspace, relative to its root.
    Workspace(PathBuf),
    Url(String),
}

fn is_url(target: &str) -> bool {
    let lower = target.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// A URL needs the same network grant a shell command would.
pub(crate) fn needs_network(args: &Value) -> bool {
    args.get("render").and_then(Value::as_bool) == Some(true)
        && args.get("path").and_then(Value::as_str).is_some_and(is_url)
}

pub(crate) async fn parse_target(
    target: &str,
    sbx: &sandbox::WorkspaceSandbox,
) -> Result<Target, ToolError> {
    let target = target.trim();
    if is_url(target) {
        return Ok(Target::Url(target.to_string()));
    }
    if target.contains("://") {
        return Err(ToolError::Args(
            "target must be a workspace file or an http(s) URL".into(),
        ));
    }
    let resolved = sbx
        .resolve(target)
        .await
        .map_err(|error| ToolError::Args(format!("{target}: {error}")))?;
    let root = sbx
        .root()
        .canonicalize()
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    let relative = resolved
        .strip_prefix(&root)
        .map_err(|_| ToolError::Args(format!("{target} is outside the workspace")))?;
    if !resolved.is_file() {
        return Err(ToolError::Args(format!("{target} is not a file")));
    }
    Ok(Target::Workspace(relative.to_path_buf()))
}

/// The first installed Chromium-family browser, or `MEDHA_BROWSER`.
pub(crate) fn find_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MEDHA_BROWSER").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let bundles = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    if let Some(found) = bundles
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    {
        return Some(found);
    }
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "brave-browser",
    ];
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Flags for one isolated render; offline, everything but `page_host` hits a dead proxy.
pub(crate) fn browser_args(
    profile: &Path,
    out: &Path,
    url: &str,
    size: (u32, u32),
    page_host: Option<&str>,
    network: bool,
) -> Vec<String> {
    let mut args = vec![
        "--headless=new".to_string(),
        format!("--user-data-dir={}", profile.display()),
        "--use-mock-keychain".into(),
        "--password-store=basic".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--disable-sync".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-crash-reporter".into(),
        "--disable-breakpad".into(),
        "--mute-audio".into(),
        "--hide-scrollbars".into(),
        format!("--window-size={},{}", size.0, size.1),
        format!("--screenshot={}", out.display()),
    ];
    // Windows CI has no hardware graphics session. Software rendering keeps
    // the renderer sandbox and avoids depending on its GPU driver/session.
    #[cfg(windows)]
    args.push("--disable-gpu".into());
    args.extend(["--enable-logging=stderr".into(), "--log-level=1".into()]);
    let own = page_host
        .and_then(|host| host.rsplit_once(':'))
        .map(|(name, _)| name);
    let mut rules: Vec<String> = own
        .map(|name| format!("MAP {name} 127.0.0.1"))
        .into_iter()
        .collect();
    if !network {
        let bypass = page_host.map(|host| format!(";{host}")).unwrap_or_default();
        rules.push("MAP * ~NOTFOUND".into());
        args.extend([
            "--proxy-server=http://127.0.0.1:9".to_string(),
            format!("--proxy-bypass-list=<-loopback>{bypass}"),
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        ]);
    }
    if !rules.is_empty() {
        args.push(format!("--host-resolver-rules={}", rules.join(", ")));
    }
    args.push(url.to_string());
    args
}

/// Serves the workspace at the root of an unguessable `<token>.localhost` origin.
pub(crate) struct PageServer {
    /// `<token>.localhost:<port>`; only requests naming it are served.
    pub(crate) host: String,
    stop: Arc<AtomicBool>,
}

impl PageServer {
    pub(crate) fn start(root: &Path) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let token = ulid::Ulid::new().to_string().to_ascii_lowercase();
        let host = format!("{token}.localhost:{}", addr.port());
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (stop, host) = (stop.clone(), host.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = serve(stream, &root, &host);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
        }
        Ok(Self { host, stop })
    }

    pub(crate) fn url(&self, relative: &Path) -> String {
        let path: Vec<String> = relative
            .components()
            .map(|part| urlencoding::encode(&part.as_os_str().to_string_lossy()).into_owned())
            .collect();
        format!("http://{}/{}", self.host, path.join("/"))
    }
}

impl Drop for PageServer {
    fn drop(&mut self) {
        // Signalled, not joined: a stalled connection must not hold up the render.
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, root: &Path, host: &str) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut line = head.lines().next().unwrap_or_default().split(' ');
    let (method, target) = (
        line.next().unwrap_or_default(),
        line.next().unwrap_or_default(),
    );
    let respond = |stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]| {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
             Connection: close\r\n\r\n",
            body.len()
        )?;
        if method != "HEAD" {
            stream.write_all(body)?;
        }
        Ok(())
    };
    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", b"");
    }
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let authorized = head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("host") && value.trim().eq_ignore_ascii_case(host)
        })
    });
    let page = authorized.then(|| open_page(root, path)).flatten();
    let Some((mut file, real)) = page else {
        return respond(&mut stream, "404 Not Found", "text/plain", b"not found");
    };
    let mut body = Vec::new();
    match file.read_to_end(&mut body) {
        Ok(_) => respond(&mut stream, "200 OK", content_type(&real), &body),
        Err(_) => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Opens first, then checks where the handle really is, so no path swap can escape.
pub(crate) fn open_page(root: &Path, request_path: &str) -> Option<(std::fs::File, PathBuf)> {
    let decoded = urlencoding::decode(request_path.strip_prefix('/')?).ok()?;
    if decoded.contains('\0') {
        return None;
    }
    let mut candidate = root.join(decoded.as_ref());
    if candidate.is_dir() {
        candidate.push("index.html");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // Non-blocking so a FIFO cannot hang the open; regular-file reads are unaffected.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(&candidate).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let real = opened_path(&file)?;
    (real.starts_with(root) && !sandbox::is_protected(&real)).then_some((file, real))
}

#[cfg(target_os = "macos")]
fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX bytes.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    buf.truncate(buf.iter().position(|&byte| byte == 0)?);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(windows)]
fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };
    let mut buf = vec![0u16; 32_768];
    // SAFETY: the handle stays open for the call and the buffer length is passed with it.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    (len > 0 && len < buf.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])))
}

/// Elsewhere nothing is served, rather than a path that was never verified.
#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn opened_path(_: &std::fs::File) -> Option<PathBuf> {
    None
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A private folder for one render, removed when the render ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("medha-browser-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dimension(
    args: &Value,
    key: &str,
    default: u32,
    range: std::ops::RangeInclusive<u32>,
) -> Result<u32, ToolError> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| range.contains(value))
            .ok_or_else(|| {
                ToolError::Args(format!(
                    "{key} must be an integer from {} to {}",
                    range.start(),
                    range.end()
                ))
            }),
    }
}

impl BrowserScreenshot {
    /// `read` with `render: true`: a screenshot of `path`, a workspace page or URL.
    pub(crate) async fn render(&self, args: &Value) -> Result<Value, ToolError> {
        let requested = arg_str(args, "path")?;
        let size = (
            dimension(args, "width", 1280, 320..=3840)?,
            dimension(args, "height", 800, 240..=4000)?,
        );
        let target = parse_target(&requested, &self.sbx).await?;
        let network = !self.sbx.denies_network();
        if matches!(target, Target::Url(_)) && !network {
            return Err(ToolError::Failed(
                "network access was not granted; the page was not opened".into(),
            ));
        }
        let scratch = Scratch::new().map_err(|error| ToolError::Failed(error.to_string()))?;
        let server = match &target {
            Target::Workspace(_) => Some(
                PageServer::start(self.sbx.root())
                    .map_err(|error| ToolError::Failed(format!("page server: {error}")))?,
            ),
            Target::Url(_) => None,
        };
        let url = match (&target, &server) {
            (Target::Workspace(relative), Some(server)) => server.url(relative),
            (Target::Url(url), _) => url.clone(),
            _ => unreachable!("a workspace target always has a server"),
        };
        let out = scratch.0.join("screenshot.png");
        let mut command = tokio::process::Command::new(&self.browser);
        command
            .args(browser_args(
                &scratch.0.join("profile"),
                &out,
                &url,
                size,
                server.as_ref().map(|server| server.host.as_str()),
                network,
            ))
            .stdin(std::process::Stdio::null());
        let process = sandbox::exec::spawn_background(command, false)
            .map_err(|error| ToolError::Failed(format!("could not start the browser: {error}")))?;
        // Headless browsers can outlive their screenshot, so stop at the image.
        let started = Instant::now();
        let mut last_size = 0;
        while started.elapsed() < RENDER_TIMEOUT && process.is_running() {
            let size_now = std::fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0);
            if size_now > 0 && size_now == last_size {
                break;
            }
            last_size = size_now;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let timed_out = started.elapsed() >= RENDER_TIMEOUT && process.is_running();
        let early_exit = (!process.is_running()).then(|| process.exit_code());
        // Asked to quit first: a killed browser leaves its temp folders behind.
        #[cfg(unix)]
        if let Some(pid) = process.pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: signals only the browser this call started.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            process.wait_until(Duration::from_secs(3)).await;
        }
        process.kill();
        process.wait().await;
        drop(server);
        let raw = std::fs::read(&out).map_err(|_| {
            let (stdout, stderr) = process.snapshot();
            let tail = |text: &str| text.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            let outcome = if timed_out {
                "the browser did not produce a screenshot within 30s".into()
            } else if let Some(code) = early_exit {
                let status = code
                    .map(|code| code.to_string())
                    .unwrap_or("unknown".into());
                format!("the browser exited with code {status} without a screenshot")
            } else {
                "the browser produced no readable screenshot".into()
            };
            ToolError::Failed(format!(
                "{outcome}; browser {}; stdout: {}; stderr: {}",
                self.browser.display(),
                tail(&stdout),
                tail(&stderr)
            ))
        })?;
        let image = tokio::task::spawn_blocking(move || media::normalize(raw))
            .await
            .map_err(|error| ToolError::Failed(format!("image decode task failed: {error}")))?
            .map_err(|error| ToolError::Failed(format!("screenshot: {error}")))?;
        let (width, height, mime) = (image.width, image.height, image.mime);
        let byte_size = image.bytes.len();
        let hash = Arc::clone(&self.artifacts)
            .put_async(image.bytes)
            .await
            .map_err(ToolError::Failed)?;
        let part = kernel::MediaPart {
            mime_type: mime.to_string(),
            source: kernel::MediaSource::Artifact(hash),
            label: Some(format!("screenshot of {requested}")),
            width: Some(width),
            height: Some(height),
            byte_size: Some(byte_size),
            provider_state: Vec::new(),
        };
        // With network any frame may be remote, whatever the top-level target.
        let trust = if network {
            kernel::TrustLabel::Web
        } else {
            kernel::TrustLabel::Tool
        };
        Ok(json!({
            "path": requested,
            "width": width,
            "height": height,
            "network": network,
            MEDIA: [serde_json::to_value(part).map_err(|e| ToolError::Failed(e.to_string()))?],
            RELAYED_TRUST: trust,
        }))
    }
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod tests;
