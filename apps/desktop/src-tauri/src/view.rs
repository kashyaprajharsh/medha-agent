//! Serves what a chat made to the window on its own scheme: a page into a frame
//! that can reach nothing, and a media file by a link only the window is given.

use std::{
    collections::VecDeque,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
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
}

#[derive(Default)]
pub struct Views {
    screens: Mutex<VecDeque<Screen>>,
    links: Mutex<VecDeque<(String, PathBuf)>>,
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
    pub fn put(&self, html: String, reach: Reach) -> Result<String, String> {
        if html.len() > MAX_SCREEN {
            return Err("This page is too large to preview.".into());
        }
        let policy = policy(&reach);
        let mut screens = self.screens.lock().unwrap();
        // The same page under the same policy is the same thing to serve.
        if let Some(same) = screens
            .iter_mut()
            .find(|screen| screen.policy == policy && screen.html == html)
        {
            same.shown += 1;
            return Ok(format!("{BASE}/screen/{}", same.id));
        }
        let id = token()?;
        screens.push_back(Screen {
            id: id.clone(),
            html,
            policy,
            shown: 1,
        });
        while screens
            .iter()
            .map(|screen| screen.html.len())
            .sum::<usize>()
            > MAX_SCREENS
        {
            screens.pop_front();
        }
        Ok(format!("{BASE}/screen/{id}"))
    }

    pub fn drop_screen(&self, url: &str) {
        let id = url.rsplit('/').next().unwrap_or_default();
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

    fn screen(&self, id: &str) -> Reply {
        let screens = self.screens.lock().unwrap();
        let Some(screen) = screens.iter().find(|screen| screen.id == id) else {
            return plain(404);
        };
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
