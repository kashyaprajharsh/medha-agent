//! Serves what a chat made to the window on its own scheme: a page into a frame
//! that can reach nothing, and a media file by a link only the window is given.

use std::{
    collections::VecDeque,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const SCHEME: &str = "medha-view";
#[cfg(any(windows, target_os = "android"))]
const BASE: &str = "http://medha-view.localhost";
#[cfg(not(any(windows, target_os = "android")))]
const BASE: &str = "medha-view://localhost";

const MAX_SCREEN: usize = 8 * 1024 * 1024;
const MAX_SCREENS: usize = 64 * 1024 * 1024;
const MAX_LINKS: usize = 256;
const MAX_CHUNK: u64 = 4 * 1024 * 1024;
const MAX_WHOLE: u64 = 64 * 1024 * 1024;

// A screen may style and draw itself and nothing else: no network, no frames, no forms.
const STILL: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:; \
     font-src data:; media-src data: blob:; form-action 'none'; base-uri 'none'";
const RUNNING: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data: blob:; \
     form-action 'none'; base-uri 'none'";

// Only when the person allowed this one page: it may load from and talk to the
// web. It still shares no origin with the app and reaches no file.
const ONLINE: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' https:; \
     style-src 'unsafe-inline' https:; img-src data: blob: https:; font-src data: https:; \
     media-src data: blob: https:; connect-src https:; form-action 'none'; base-uri 'none'";

#[derive(Clone, PartialEq, Debug)]
pub enum Reach {
    Still,
    Running,
    Online,
    /// A screen from a connected server: it reaches the origins that server
    /// declared for it and no others.
    App(Origins),
}

#[derive(Clone, Default, PartialEq, Debug, serde::Deserialize)]
#[serde(default)]
pub struct Origins {
    pub connect: Vec<String>,
    pub resources: Vec<String>,
    pub frames: Vec<String>,
}

/// An origin that may sit in a policy line: a secure scheme, a public hostname,
/// an optional port, and no character that could end the directive or widen it.
/// An address or a local name is refused: a screen has no business with this machine.
fn origin(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("wss://"))
    else {
        return false;
    };
    let rest = rest.strip_prefix("*.").unwrap_or(rest);
    let (host, port) = rest.split_once(':').unwrap_or((rest, "443"));
    let last = host.rsplit('.').next().unwrap_or_default();
    host.contains('.')
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && last.len() >= 2
        && last.chars().all(|c| c.is_ascii_alphabetic())
        && !matches!(last, "localhost" | "local" | "internal" | "lan" | "home")
        && port.parse::<u16>().is_ok()
}

fn policy(reach: &Reach) -> String {
    let Reach::App(origins) = reach else {
        return match reach {
            Reach::Still => STILL,
            Reach::Running => RUNNING,
            _ => ONLINE,
        }
        .to_string();
    };
    let listed = |list: &[String]| {
        let kept: Vec<&str> = list
            .iter()
            .map(String::as_str)
            .filter(|text| origin(text))
            .take(32)
            .collect();
        kept.join(" ")
    };
    let (resources, connect, frames) = (
        listed(&origins.resources),
        listed(&origins.connect),
        listed(&origins.frames),
    );
    let or_none = |list: String| {
        if list.is_empty() {
            "'none'".to_string()
        } else {
            list
        }
    };
    format!(
        "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline' {resources}; \
         style-src 'unsafe-inline' {resources}; img-src data: blob: {resources}; \
         font-src data: {resources}; media-src data: blob: {resources}; connect-src {}; \
         frame-src {}; form-action 'none'; base-uri 'none'",
        or_none(connect),
        or_none(frames)
    )
}

struct Screen {
    id: String,
    html: String,
    policy: String,
    /// How many frames show this page; it is kept once and goes with the last of them.
    shown: usize,
    assets: Arc<Vec<Asset>>,
    fingerprint: [u8; 32],
}

pub struct Asset {
    pub path: String,
    pub mime: String,
    pub data: Vec<u8>,
}

fn asset_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', '%', ':', '?', '#'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
}

fn asset_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/css"
            | "text/javascript"
            | "application/json"
            | "image/svg+xml"
            | "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "font/woff"
            | "font/woff2"
            | "font/ttf"
            | "font/otf"
    )
}

#[derive(Default)]
pub struct Views {
    screens: Mutex<VecDeque<Screen>>,
    links: Mutex<VecDeque<(String, PathBuf)>>,
    bundles: Mutex<VecDeque<(String, Arc<Vec<Asset>>)>>,
}

#[derive(Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    Some(match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        _ => return None,
    })
}

impl Views {
    pub fn bundle(&self, assets: Vec<Asset>) -> Result<String, String> {
        let size: usize = assets.iter().map(|asset| asset.data.len()).sum();
        let mut names = std::collections::HashSet::new();
        if assets.len() > 64
            || size > 32 * 1024 * 1024
            || assets.iter().any(|asset| {
                !asset_path(&asset.path)
                    || !asset_mime(&asset.mime)
                    || asset.data.len() > 4 * 1024 * 1024
                    || !names.insert(&asset.path)
            })
        {
            return Err("Invalid or oversized artifact asset manifest.".into());
        }
        let mut bundles = self.bundles.lock().unwrap();
        let held: usize = bundles
            .iter()
            .flat_map(|(_, assets)| assets.iter())
            .map(|asset| asset.data.len())
            .sum();
        if bundles.len() >= 32 || held + size > MAX_SCREENS {
            return Err("Too many artifact assets are open. Close a preview and try again.".into());
        }
        let id = token()?;
        bundles.push_back((id.clone(), Arc::new(assets)));
        Ok(id)
    }

    pub fn drop_bundle(&self, id: &str) {
        self.bundles
            .lock()
            .unwrap()
            .retain(|(other, _)| other != id);
    }

    /// Lets go of everything the window held. A page that loads again has lost
    /// every link it was given, so nothing it held would ever be dropped.
    pub fn forget_all(&self) {
        self.screens.lock().unwrap().clear();
        self.links.lock().unwrap().clear();
        self.bundles.lock().unwrap().clear();
    }

    pub fn put(&self, html: String, reach: Reach) -> Result<String, String> {
        self.put_bundle(html, reach, None)
    }

    pub fn put_bundle(
        &self,
        html: String,
        reach: Reach,
        bundle: Option<&str>,
    ) -> Result<String, String> {
        if html.len() > MAX_SCREEN {
            return Err("This page is too large to preview.".into());
        }
        let assets = if let Some(id) = bundle {
            self.bundles
                .lock()
                .unwrap()
                .iter()
                .find(|(other, _)| other == id)
                .map(|(_, assets)| assets.clone())
                .ok_or("The artifact's assets were released. Reopen the preview.")?
        } else {
            Arc::new(Vec::new())
        };
        let mut policy = policy(&reach);
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(html.as_bytes());
        digest.update(policy.as_bytes());
        let fingerprint: [u8; 32] = digest.finalize().into();
        let mut screens = self.screens.lock().unwrap();
        // The same page under the same policy is the same thing to serve.
        if let Some(same) = screens.iter_mut().find(|screen| {
            bundle.is_none() && screen.assets.is_empty() && screen.fingerprint == fingerprint
        }) {
            same.shown += 1;
            return Ok(format!("{BASE}/screen/{}", same.id));
        }
        let id = token()?;
        let html = if !matches!(reach, Reach::App(_)) {
            if matches!(reach, Reach::Still) {
                // Only our reporting script runs in a stopped/static preview;
                // author scripts remain blocked by a nonce CSP.
                policy = policy.replace(
                    "sandbox;",
                    &format!("sandbox allow-scripts; script-src 'nonce-{id}';"),
                );
            }
            let monitor = format!(
                "<script nonce=\"{id}\">addEventListener('securitypolicyviolation',e=>parent.postMessage({{kind:'medha-preview-blocked',id:'{id}',uri:e.blockedURI,directive:e.violatedDirective}},'*'))</script>"
            );
            if let Some(end) = html
                .to_ascii_lowercase()
                .find("<head")
                .and_then(|at| html[at..].find('>').map(|end| at + end + 1))
            {
                format!("{}{}{}", &html[..end], monitor, &html[end..])
            } else {
                format!("{monitor}{html}")
            }
        } else {
            html
        };
        if !assets.is_empty() {
            let own = format!("{BASE}/screen/{id}/");
            for directive in ["style-src", "img-src", "font-src", "media-src"] {
                policy = policy.replace(&format!("{directive} "), &format!("{directive} {own} "));
            }
            if !matches!(reach, Reach::Still) {
                policy = policy.replace("script-src ", &format!("script-src {own} "));
            }
            if policy.contains("connect-src ") {
                policy = policy.replace("connect-src ", &format!("connect-src {own} "));
            } else {
                policy.push_str(&format!("; connect-src {own}"));
            }
        }
        let held: usize = screens
            .iter()
            .map(|screen| {
                screen.html.len()
                    + screen
                        .assets
                        .iter()
                        .map(|asset| asset.data.len())
                        .sum::<usize>()
            })
            .sum();
        let size = html.len() + assets.iter().map(|asset| asset.data.len()).sum::<usize>();
        if screens.len() >= 64 || held + size > MAX_SCREENS {
            return Err("Too many previews are open. Close one and try again.".into());
        }
        screens.push_back(Screen {
            id: id.clone(),
            html,
            policy,
            shown: 1,
            assets,
            fingerprint,
        });
        Ok(if bundle.is_some() {
            format!("{BASE}/screen/{id}/index.html")
        } else {
            format!("{BASE}/screen/{id}")
        })
    }

    pub fn drop_screen(&self, url: &str) {
        let id = url
            .split("/screen/")
            .nth(1)
            .unwrap_or_default()
            .split('/')
            .next()
            .unwrap_or_default();
        self.screens.lock().unwrap().retain_mut(|screen| {
            if screen.id == id {
                screen.shown -= 1;
            }
            screen.shown > 0
        });
    }

    /// `path` must already be confined to the chat's folder.
    pub fn link(&self, path: PathBuf) -> Result<String, String> {
        if media_type(&path).is_none() {
            return Err("This file is not a video or a sound.".into());
        }
        let mut links = self.links.lock().unwrap();
        if let Some((id, _)) = links.iter().find(|(_, linked)| *linked == path) {
            return Ok(format!("{BASE}/file/{id}"));
        }
        let id = token()?;
        links.push_back((id.clone(), path));
        if links.len() > MAX_LINKS {
            links.pop_front();
        }
        Ok(format!("{BASE}/file/{id}"))
    }

    pub fn respond(&self, path: &str, range: Option<&str>) -> Reply {
        let mut parts = path.trim_start_matches('/').splitn(2, '/');
        match (parts.next(), parts.next()) {
            (Some("screen"), Some(id)) => self.screen(id),
            (Some("file"), Some(id)) => self.file(id, range),
            _ => plain(404),
        }
    }

    fn screen(&self, path: &str) -> Reply {
        let (id, asset) = path.split_once('/').unwrap_or((path, ""));
        let screens = self.screens.lock().unwrap();
        let Some(screen) = screens.iter().find(|screen| screen.id == id) else {
            return plain(404);
        };
        if !asset.is_empty() && asset != "index.html" {
            // Percent encoding is decoded by the URL adapter only for ordinary
            // names; no arbitrary path, traversal or filesystem lookup occurs.
            let Ok(asset) = percent_name(asset) else {
                return plain(404);
            };
            let Some(asset) = screen.assets.iter().find(|entry| entry.path == asset) else {
                return plain(404);
            };
            return Reply {
                status: 200,
                headers: vec![
                    ("Content-Type", asset.mime.clone()),
                    ("X-Content-Type-Options", "nosniff".into()),
                    ("Access-Control-Allow-Origin", "*".into()),
                    ("Cache-Control", "no-store".into()),
                ],
                body: asset.data.clone(),
            };
        }
        let policy = screen.policy.as_str();
        Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html; charset=utf-8".into()),
                ("Content-Security-Policy", policy.into()),
                ("X-Content-Type-Options", "nosniff".into()),
                ("Referrer-Policy", "no-referrer".into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: screen.html.clone().into_bytes(),
        }
    }

    fn file(&self, id: &str, range: Option<&str>) -> Reply {
        let found = self
            .links
            .lock()
            .unwrap()
            .iter()
            .find(|(linked, _)| linked == id)
            .cloned();
        let Some((_, path)) = found else {
            return plain(404);
        };
        let Some(kind) = media_type(&path) else {
            return plain(404);
        };
        let Ok(mut file) = fs::File::open(&path) else {
            return plain(404);
        };
        let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let Some((start, end)) = span(range, size) else {
            let mut reply = plain(416);
            reply
                .headers
                .push(("Content-Range", format!("bytes */{size}")));
            return reply;
        };
        let mut body = vec![0u8; (end - start + 1) as usize];
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_exact(&mut body).is_err() {
            return plain(404);
        }
        let mut headers = vec![
            ("Content-Type", kind.to_string()),
            ("Accept-Ranges", "bytes".into()),
            ("X-Content-Type-Options", "nosniff".into()),
            ("Cache-Control", "no-store".into()),
        ];
        if range.is_some() {
            headers.push(("Content-Range", format!("bytes {start}-{end}/{size}")));
        }
        Reply {
            status: if range.is_some() { 206 } else { 200 },
            headers,
            body,
        }
    }
}

fn percent_name(path: &str) -> Result<String, ()> {
    // Asset names with spaces/non-ASCII characters arrive URL-encoded. Reject
    // encoded separators and traversal, then compare the whole manifest key.
    let mut bytes = Vec::new();
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hex = |byte: u8| {
                (byte as char)
                    .to_digit(16)
                    .map(|digit| digit as u8)
                    .ok_or(())
            };
            let decoded = hex(input.next().ok_or(())?)? * 16 + hex(input.next().ok_or(())?)?;
            if matches!(decoded, b'/' | b'\\' | b'%') {
                return Err(());
            }
            bytes.push(decoded);
        } else {
            bytes.push(byte);
        }
    }
    let name = String::from_utf8(bytes).map_err(|_| ())?;
    if !asset_path(&name) {
        return Err(());
    }
    Ok(name)
}

fn plain(status: u16) -> Reply {
    Reply {
        status,
        headers: vec![("Cache-Control", "no-store".into())],
        body: Vec::new(),
    }
}

/// The inclusive bytes to send: the asked range, cut to one chunk, or `None` when it cannot be met.
fn span(range: Option<&str>, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let last = size - 1;
    let Some(range) = range else {
        return (size <= MAX_WHOLE).then_some((0, last));
    };
    let (from, to) = range.trim().strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = match (from.trim(), to.trim()) {
        ("", suffix) => (size.saturating_sub(suffix.parse::<u64>().ok()?), last),
        (from, "") => (from.parse().ok()?, last),
        (from, to) => (from.parse().ok()?, to.parse::<u64>().ok()?.min(last)),
    };
    (start <= end && start <= last).then(|| (start, end.min(start + MAX_CHUNK - 1)))
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
