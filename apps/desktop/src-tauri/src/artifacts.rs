//! Desktop adapter for immutable backend artifacts. Only a conversation's
//! verified publication manifest can supply bytes; no file path is opened here.
use crate::{files, view, workspaces::Runtime};
use base64::Engine;
use protocol::{ArtifactBytes, ArtifactSnapshot};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_FILE: usize = 24 * 1024 * 1024;
const MAX_BUNDLE: usize = 32 * 1024 * 1024;
const MAX_ASSET: usize = 4 * 1024 * 1024;

pub fn manifest(
    runtime: &Runtime,
    session: &str,
    revision: &str,
) -> Result<ArtifactSnapshot, String> {
    serde_json::from_value(runtime.request_params(
        "application.artifacts.file",
        json!({"session_id": session, "revision_id": revision}),
    )?)
    .map_err(|e| e.to_string())
}

pub fn bytes(
    runtime: &Runtime,
    session: &str,
    revision: &str,
    asset: Option<&str>,
    size: usize,
    hash: &str,
) -> Result<Vec<u8>, String> {
    if size > MAX_FILE {
        return Err("This artifact exceeds the 24 MiB preview limit.".into());
    }
    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        let read: ArtifactBytes = serde_json::from_value(runtime.request_params("application.artifacts.read", json!({
            "session_id": session, "revision_id": revision, "asset": asset, "offset": bytes.len()
        }))?).map_err(|e| e.to_string())?;
        let part = base64::engine::general_purpose::STANDARD
            .decode(read.data)
            .map_err(|e| e.to_string())?;
        if read.size != size || part.is_empty() || part.len() > size - bytes.len() {
            return Err("The artifact's size changed or it is truncated.".into());
        }
        bytes.extend(part);
    }
    if format!("{:x}", Sha256::digest(&bytes)) != hash {
        return Err("The stored artifact failed its content check.".into());
    }
    Ok(bytes)
}

/// A page's asset as it is now, if it is still inside both the workspace and
/// the page's own folder. A name that leads anywhere else reads nothing.
fn current(root: &std::path::Path, folder: &std::path::Path, asset: &str) -> Option<Vec<u8>> {
    let folder = files::resolve(root, &folder.to_string_lossy()).ok()?;
    let path = files::resolve(root, &folder.join(asset).to_string_lossy()).ok()?;
    if !path.starts_with(&folder) || !std::fs::metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(std::fs::File::open(&path).ok()?, MAX_ASSET as u64 + 1),
        &mut data,
    )
    .ok()?;
    Some(data)
}

/// Where a page's files may be read from: the chat's scratch folder for a page inside it, else the chat's folder.
pub(crate) fn asset_root<'a>(
    chat: &'a std::path::Path,
    page: &std::path::Path,
    scratch: Option<&'a str>,
) -> &'a std::path::Path {
    // Compared as shown, so a Windows path written with `\\?\` still matches its folder.
    scratch
        .map(std::path::Path::new)
        .filter(|scratch| file_ops::shown(page).starts_with(file_ops::shown(scratch)))
        .unwrap_or(chat)
}

/// What a page loads from its own folder as the folder is now, after the copies it kept.
pub(crate) fn assets_now(
    root: &std::path::Path,
    page: &std::path::Path,
    html: &str,
    kept: Vec<view::Asset>,
) -> (Vec<view::Asset>, Vec<String>) {
    use file_ops::page_assets::{mime, references, relative};
    use std::path::PathBuf;
    let Some(folder) = page.parent() else {
        return (kept, Vec::new());
    };
    let mut total: usize = kept.iter().map(|asset| asset.data.len()).sum();
    let mut seen: std::collections::HashSet<String> =
        kept.iter().map(|asset| asset.path.clone()).collect();
    let mut assets = kept;
    let mut missing = Vec::new();
    let mut pending: std::collections::VecDeque<(PathBuf, String)> = references(html, true)
        .into_iter()
        .map(|url| (PathBuf::new(), url))
        .collect();
    while let Some((from, url)) = pending.pop_front() {
        let Some(path) = relative(&from, &url) else {
            // Web addresses are the preview's own business; a local name that leaves the folder is reported.
            if url.starts_with("../") || url.starts_with('/') && !url.starts_with("//") {
                missing.push(url.chars().take(300).collect());
            }
            continue;
        };
        let name = path.to_string_lossy().replace('\\', "/");
        if !seen.insert(name.clone()) {
            continue;
        }
        if assets.len() >= 64 {
            missing.push("More than 64 local assets; remaining assets were left out.".into());
            break;
        }
        let (Some(mime), Some(data)) = (mime(&path), current(root, folder, &name)) else {
            missing.push(name);
            continue;
        };
        if data.len() > MAX_ASSET || total + data.len() > MAX_BUNDLE {
            missing.push(name);
            continue;
        }
        total += data.len();
        // Only stylesheets and scripts are read on, for what they load in turn.
        if matches!(mime, "text/css" | "text/javascript")
            && let Ok(text) = std::str::from_utf8(&data)
        {
            let parent = path
                .parent()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf();
            pending.extend(
                references(text, false)
                    .into_iter()
                    .map(|url| (parent.clone(), url)),
            );
        }
        assets.push(view::Asset {
            path: name,
            mime: mime.into(),
            data,
        });
    }
    (assets, missing)
}

pub fn preview(
    runtime: &Runtime,
    views: &view::Views,
    session: &str,
    revision: &str,
) -> Result<Value, String> {
    let manifest = manifest(runtime, session, revision)?;
    let data = bytes(
        runtime,
        session,
        revision,
        None,
        manifest.size,
        &manifest.hash,
    )?;
    let mut preview = files::preview_bytes(std::path::Path::new(&manifest.path), data)?;
    let mut total = manifest.size;
    let mut kept = Vec::new();
    if manifest.assets.len() > 64 {
        return Err("The artifact has too many assets.".into());
    }
    // Copies the revision kept come first; everything else it loads is read from its folder as it is now.
    for asset in manifest.assets {
        let Some(hash) = &asset.hash else {
            continue;
        };
        if asset.size > MAX_ASSET || total.saturating_add(asset.size) > MAX_BUNDLE {
            return Err("The artifact's asset bundle is too large.".into());
        }
        let data = bytes(
            runtime,
            session,
            revision,
            Some(&asset.path),
            asset.size,
            hash,
        )?;
        total += data.len();
        kept.push(view::Asset {
            path: asset.path,
            mime: asset.mime,
            data,
        });
    }
    let (assets, missing) = match preview["extension"].as_str() {
        Some("html" | "htm") => {
            let page = preview["text"].as_str().unwrap_or_default().to_owned();
            let path = std::path::Path::new(&manifest.path);
            let root = asset_root(&runtime.path, path, manifest.scratch.as_deref());
            assets_now(root, path, &page, kept)
        }
        _ => (kept, manifest.missing),
    };
    if !assets.is_empty() {
        preview["bundle"] = json!(views.bundle(assets)?);
    }
    preview["missing"] = json!(missing);
    preview["snapshot"] = json!(true);
    Ok(preview)
}

#[cfg(test)]
#[path = "artifacts_tests.rs"]
mod tests;
