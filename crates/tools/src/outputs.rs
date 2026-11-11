//! What makes a written file an output: its kind and a bounded, explicitly
//! referenced asset manifest. Reading a file never publishes it as an output.

use regex::Regex;
use sandbox::WorkspaceSandbox;
use scraper::{Html, Selector};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

pub const FIELD: &str = kernel::events::TOOL_ARTIFACT_FIELD;
const MAX_ASSETS: usize = 64;
const MAX_ASSET: usize = 4 * 1024 * 1024;
const MAX_BUNDLE: usize = 32 * 1024 * 1024;

pub fn kind(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "html" | "htm" => "page",
            "mmd" => "diagram",
            "svg" => "drawing",
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "image",
            "mp4" | "m4v" | "mov" | "webm" => "video",
            "mp3" | "wav" | "m4a" | "ogg" | "oga" | "flac" => "audio",
            "pdf" => "pdf",
            "docx" => "document",
            "csv" | "tsv" | "xlsx" => "table",
            "pptx" => "deck",
            _ => return None,
        },
    )
}

fn mime(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "css" => "text/css",
            "js" | "mjs" => "text/javascript",
            "json" => "application/json",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "ttf" => "font/ttf",
            "otf" => "font/otf",
            _ => return None,
        },
    )
}

static URLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:url\(\s*|@import\s+|(?:from|import)\s*|import\(\s*)["']?([^\s"'<>);]+)"#)
        .expect("asset references")
});

fn references(text: &str, html: bool) -> Vec<String> {
    let mut found: Vec<String> = URLS
        .captures_iter(text)
        .take(257)
        .map(|cap| cap[1].to_owned())
        .collect();
    if html {
        let document = Html::parse_document(text);
        let selector =
            Selector::parse("[src],link[href],[poster],[srcset]").expect("asset elements");
        for element in document.select(&selector) {
            if found.len() >= 257 {
                break;
            }
            for attribute in ["src", "href", "poster"] {
                if let Some(value) = element.value().attr(attribute) {
                    found.push(value.to_owned());
                }
            }
            if let Some(set) = element.value().attr("srcset") {
                found.extend(
                    set.split(',')
                        .take(64)
                        .filter_map(|entry| entry.split_whitespace().next())
                        .map(str::to_owned),
                );
            }
        }
    }
    found.truncate(257);
    found
}

fn relative(from: &Path, requested: &str) -> Option<PathBuf> {
    let requested = requested.split(['?', '#']).next()?.trim();
    if requested.is_empty()
        || requested.len() > 1024
        || requested.starts_with('/')
        || requested.contains(':')
        || requested.contains('\\')
    {
        return None;
    }
    let decoded = urlencoding::decode(requested).ok()?;
    if decoded.contains(['\\', ':', '%', '?', '#'])
        || requested.to_ascii_lowercase().contains("%2f")
    {
        return None;
    }
    let mut path = from.to_path_buf();
    for step in Path::new(decoded.as_ref()).components() {
        match step {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !path.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(path)
}

/// What a written document needs to be shown again. Its text is already in the
/// write's own recorded result, so nothing is copied: this lists the local
/// assets it loads, which a preview reads from the document's folder. They are
/// listed only if already allowed and inside that folder, never through a
/// prompt or a wider grant.
pub async fn manifest(sbx: &WorkspaceSandbox, path: &Path, bytes: &[u8]) -> Result<Value, String> {
    let kind = kind(path).ok_or("this format has no output preview")?;
    let mut assets = Vec::new();
    let mut missing = Vec::new();
    let size = bytes.len();
    let mut total = size;
    let mut pending = VecDeque::new();
    if kind == "page"
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        let mut end = text.len().min(2 * 1024 * 1024);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            missing.push("Local asset discovery is limited to the first 2 MiB of HTML; later dependencies were left out.".into());
        }
        let urls = references(&text[..end], true);
        if urls.len() > 256 {
            missing
                .push("More than 256 asset references; later dependencies were left out.".into());
        }
        pending.extend(urls.into_iter().take(256).map(|url| (PathBuf::new(), url)));
    }
    let base = path.parent().ok_or("output has no directory")?;
    let mut seen = HashSet::new();
    while let Some((from, url)) = pending.pop_front() {
        let Some(relative) = relative(&from, &url) else {
            // Remote URLs are controlled by preview CSP. Escaping local paths
            // are reported rather than being read through the asset loader.
            if url.starts_with("../") || url.starts_with('/') && !url.starts_with("//") {
                missing.push(url.chars().take(300).collect());
            }
            continue;
        };
        if !seen.insert(relative.clone()) {
            continue;
        }
        if seen.len() > MAX_ASSETS {
            missing.push("More than 64 local assets; remaining assets were left out.".into());
            break;
        }
        let name = relative.to_string_lossy().replace('\\', "/");
        let Some(mime) = mime(&relative) else {
            missing.push(name);
            continue;
        };
        let candidate = base.join(&relative);
        let Some(approved) = sbx
            .resolve_if_permitted(&candidate.to_string_lossy())
            .await
            .filter(|path| path.starts_with(base))
        else {
            missing.push(name);
            continue;
        };
        let size = match tokio::fs::metadata(&approved).await {
            Ok(meta) if meta.is_file() => usize::try_from(meta.len()).unwrap_or(usize::MAX),
            _ => {
                missing.push(name);
                continue;
            }
        };
        if size > MAX_ASSET || total.saturating_add(size) > MAX_BUNDLE {
            missing.push(name);
            continue;
        }
        total += size;
        // Only stylesheets and scripts are read, to follow what they load in turn.
        if matches!(mime, "text/css" | "text/javascript")
            && let Ok(data) = sbx.read_bytes_resolved(&approved, MAX_ASSET).await
            && let Ok(text) = std::str::from_utf8(&data)
        {
            let urls = references(text, false);
            if urls.len() > 256 {
                missing.push(format!(
                    "More than 256 references in {name}; later dependencies were left out."
                ));
            }
            pending.extend(urls.into_iter().take(256).map(|url| {
                (
                    relative.parent().unwrap_or(Path::new("")).to_path_buf(),
                    url,
                )
            }));
        }
        assets.push(json!({"path": name, "size": size, "mime": mime}));
    }
    missing.truncate(MAX_ASSETS);
    Ok(
        json!({"path": path.to_string_lossy(), "kind": kind, "size": size, "assets": assets, "missing": missing}),
    )
}

/// Most files one reply can bring forward as outputs.
const MAX_NAMED: usize = 8;
/// A filesystem that keeps whole seconds can date a file just before the run began.
const CLOCK_SLACK: f64 = 2.0;

/// Paths a reply points at: image and link targets, and names in code spans.
/// Entries a reply's look for changed files may visit, so a large folder cannot slow a turn's end.
const MAX_SCANNED: usize = 20_000;

/// What a reply names: files it links or embeds, which ask for them to be shown, and files it
/// only mentions in code, which never add an output but put the run's own first.
fn named_paths(text: &str) -> (Vec<String>, Vec<String>) {
    use pulldown_cmark::{Event, Parser, Tag};
    let (mut linked, mut mentioned) = (Vec::new(), Vec::new());
    for part in Parser::new(text) {
        let (named, into) = match part {
            Event::Start(Tag::Image { dest_url, .. } | Tag::Link { dest_url, .. }) => (
                urlencoding::decode(&dest_url)
                    .ok()
                    .map(|path| path.into_owned()),
                &mut linked,
            ),
            Event::Code(code) => (Some(code.into_string()), &mut mentioned),
            _ => continue,
        };
        let Some(path) = named.map(|path| path.trim().to_owned()) else {
            continue;
        };
        if (3..=1024).contains(&path.len())
            && !path.contains("://")
            && !path.contains(['*', '\n', '#', '?'])
            && kind(Path::new(&path)).is_some()
            && !into.contains(&path)
        {
            into.push(path);
        }
    }
    (linked, mentioned)
}

/// Seconds since the epoch at which a file last changed.
fn changed_at(meta: &std::fs::Metadata) -> Option<f64> {
    let time = meta.modified().ok()?;
    Some(
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64(),
    )
}

/// Viewable files in the workspace that changed since `since`, newest first, with the same
/// folders left out as the workspace tools leave out.
fn changed_files(root: &Path, since: f64) -> Vec<PathBuf> {
    let mut found: Vec<(f64, PathBuf)> = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .filter_entry(|entry| !crate::skip_dir(entry))
        .build();
    for entry in walk.take(MAX_SCANNED).flatten() {
        let path = entry.path();
        if kind(path).is_none() || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(changed) = entry.metadata().ok().as_ref().and_then(changed_at)
            && changed >= since - CLOCK_SLACK
        {
            found.push((changed, path.to_path_buf()));
        }
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    found.into_iter().map(|(_, path)| path).collect()
}

/// The outputs of a reply. A file it links or embeds is shown even when the run left it alone,
/// so a person asking to see a file again gets it. A file it names in code is shown only when
/// the run created or changed it, found by its path or by its file name alone, since a reply
/// often leaves out the folder. A file the run only read keeps its old time and is never shown,
/// and nothing the reply does not name is added. Every file passes the same permission check as
/// any read.
pub async fn named(sbx: &WorkspaceSandbox, text: &str, since: f64) -> Vec<Value> {
    let (linked, mentioned) = named_paths(text);
    let root = sbx.root().to_path_buf();
    let changed = if mentioned.is_empty() {
        Vec::new()
    } else {
        tokio::task::spawn_blocking({
            let root = root.clone();
            move || changed_files(&root, since)
        })
        .await
        .unwrap_or_default()
    };
    let named_by_reply = |path: &PathBuf| {
        mentioned.iter().any(|name| {
            path.ends_with(name)
                || Path::new(name)
                    .file_name()
                    .is_some_and(|file| path.file_name() == Some(file))
        })
    };
    let candidates = linked
        .iter()
        .map(|path| root.join(path))
        .chain(changed.into_iter().filter(named_by_reply));
    let mut outputs: Vec<Value> = Vec::new();
    for candidate in candidates {
        if outputs.len() >= MAX_NAMED {
            break;
        }
        let Some(resolved) = sbx.resolve_if_permitted(&candidate.to_string_lossy()).await else {
            continue;
        };
        let Ok(meta) = tokio::fs::metadata(&resolved).await else {
            continue;
        };
        let (true, Some(kind)) = (meta.is_file(), kind(&resolved)) else {
            continue;
        };
        let path = resolved.to_string_lossy().into_owned();
        if outputs.iter().all(|other| other["path"] != path) {
            outputs.push(json!({"path": path, "kind": kind}));
        }
    }
    outputs
}

/// A manifest failure must not turn an already successful edit into a failed
/// tool result (which could cause the model to repeat the mutation).
pub async fn attach(result: &mut Value, sbx: &WorkspaceSandbox, path: &Path, text: &str) {
    if kind(path).is_none() {
        return;
    }
    match manifest(sbx, path, text.as_bytes()).await {
        Ok(output) => result[FIELD] = output,
        Err(error) => result["preview_notice"] = json!(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_resolve_within_the_manifest_and_cannot_escape() {
        assert_eq!(
            relative(Path::new("css"), "../images/logo.png?v=1"),
            Some(PathBuf::from("images/logo.png"))
        );
        for url in [
            "../../secret",
            "/etc/passwd",
            "file:///secret",
            "%2fetc/passwd",
            "https://example.com/a",
        ] {
            assert!(relative(Path::new("css"), url).is_none(), "{url}");
        }
    }

    #[tokio::test]
    async fn a_page_lists_its_local_assets_recursively_and_copies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("css")).unwrap();
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::write(
            root.join("css/style.css"),
            "@import './base.css'; body{background:url('../images/logo.png')}",
        )
        .unwrap();
        std::fs::write(root.join("css/base.css"), "h1{color:red}").unwrap();
        std::fs::write(root.join("images/logo.png"), b"synthetic-image").unwrap();
        std::fs::write(root.join("main.js"), "import './helpers.js';").unwrap();
        std::fs::write(root.join("helpers.js"), "export const n=1").unwrap();
        let page = "<html><head><link rel=stylesheet href='./css/style.css'></head><body><h1>one</h1><script src='main.js'></script></body></html>";
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();
        let output = manifest(&sbx, &root.join("index.html"), page.as_bytes())
            .await
            .unwrap();
        assert!(output["missing"].as_array().unwrap().is_empty());
        // What stylesheets and scripts load in turn is found too, and every
        // asset is listed by path for a preview to read, never copied.
        let mut listed: Vec<_> = output["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| {
                assert!(asset.get("hash").is_none(), "{asset}");
                asset["path"].as_str().unwrap()
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                "css/base.css",
                "css/style.css",
                "helpers.js",
                "images/logo.png",
                "main.js"
            ]
        );
        assert!(
            output.get("hash").is_none(),
            "the text is in the write's own result"
        );
    }

    #[tokio::test]
    async fn a_reply_shows_the_files_it_names_that_its_run_made_and_never_what_it_only_read() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("chat");
        for folder in [
            "chat/campaign",
            "chat/stills",
            "chat/node_modules/pkg",
            "outside",
        ] {
            std::fs::create_dir_all(base.join(folder)).unwrap();
        }
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let old = |name: &str| {
            let path = base.join(name);
            std::fs::write(&path, b"bytes").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        };
        // Files that were there before the run: it may read them, but they are not its outputs.
        old("chat/stills/frame.png");
        old("chat/stills/earlier.mp4");
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        // What the run made: a video copied into a folder, a package's own image, a file outside.
        for name in [
            "chat/campaign/video_1080p.mp4",
            "chat/node_modules/pkg/logo.png",
            "outside/leak.png",
        ] {
            std::fs::write(base.join(name), b"bytes").unwrap();
        }
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();
        let path = |name: &str| root.join(name).to_string_lossy().into_owned();

        // Named by its file name alone, as a model often writes it, and next to a file it only read.
        let found = named(
            &sbx,
            "Ready: **`video_1080p.mp4`**. I looked at `stills/frame.png` too.",
            since,
        )
        .await;
        assert_eq!(
            found,
            vec![json!({"path": path("campaign/video_1080p.mp4"), "kind": "video"})]
        );
        // Not named at all: nothing is added, so a build's or a test run's files never pile up as cards.
        assert_eq!(
            named(&sbx, "The video is ready.", since).await,
            Vec::<Value>::new()
        );
        // Asked to see an earlier file again: a link shows it even though this run left it alone.
        let found = named(
            &sbx,
            "Here it is: [earlier](stills/earlier.mp4). Outside: [x](../outside/leak.png)",
            since,
        )
        .await;
        assert_eq!(
            found,
            vec![json!({"path": path("stills/earlier.mp4"), "kind": "video"})]
        );
    }

    #[tokio::test]
    async fn a_reply_brings_forward_only_permitted_outputs_its_run_changed() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("work");
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        for name in [
            "work/chart.png",
            "work/old.png",
            "work/notes.txt",
            "outside/leak.png",
        ] {
            std::fs::write(base.join(name), b"bytes").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(root.join("old.png"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        let leak = base.join("outside/leak.png");
        let text = format!(
            "![chart](chart.png) and again `chart.png`. Older: `old.png`. Text: `notes.txt`. \
             Gone: `missing.png`. Elsewhere: [a](../outside/leak.png) `{}` ![](https://example.com/a.png)",
            leak.display()
        );
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();
        let found = named(&sbx, &text, since).await;
        assert_eq!(
            found,
            vec![json!({"path": root.join("chart.png").to_string_lossy(), "kind": "image"})]
        );
    }

    #[tokio::test]
    async fn publication_does_not_read_assets_outside_the_document_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("output")).unwrap();
        std::fs::write(root.join("secret.json"), b"sentinel-never-an-asset").unwrap();
        let text = "<html><script src='../secret.json'></script><img src='/secret.json'><link href='missing.css'></html>";
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();
        let output = manifest(&sbx, &root.join("output/index.html"), text.as_bytes())
            .await
            .unwrap();
        assert!(output["assets"].as_array().unwrap().is_empty());
        assert_eq!(output["missing"].as_array().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn asset_fifos_and_replaced_symlinks_cannot_block_or_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sbx = WorkspaceSandbox::new_jailed(&root).unwrap();
        std::fs::write(other.path().join("secret.js"), "sentinel-private").unwrap();
        std::fs::write(root.join("safe.js"), "safe").unwrap();
        let approved = sbx.resolve_if_permitted("safe.js").await.unwrap();
        std::fs::remove_file(root.join("safe.js")).unwrap();
        symlink(other.path().join("secret.js"), root.join("safe.js")).unwrap();
        assert!(sbx.read_bytes_resolved(&approved, 1024).await.is_err());
        let fifo =
            std::ffi::CString::new(root.join("pipe.js").to_string_lossy().as_bytes()).unwrap();
        // SAFETY: fifo is a live NUL-terminated path; mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            manifest(
                &sbx,
                &root.join("index.html"),
                b"<html><script src='safe.js'></script><script src='pipe.js'></script></html>",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output["assets"].as_array().unwrap().is_empty());
        assert_eq!(output["missing"].as_array().unwrap().len(), 2);
    }
}
