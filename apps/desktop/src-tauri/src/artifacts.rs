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
    let mut assets = Vec::new();
    let mut missing = manifest.missing;
    if manifest.assets.len() > 64 {
        return Err("The artifact has too many assets.".into());
    }
    let folder = std::path::Path::new(&manifest.path).parent();
    for asset in manifest.assets {
        let data = match &asset.hash {
            Some(hash) => {
                if asset.size > MAX_ASSET || total.saturating_add(asset.size) > MAX_BUNDLE {
                    return Err("The artifact's asset bundle is too large.".into());
                }
                bytes(
                    runtime,
                    session,
                    revision,
                    Some(&asset.path),
                    asset.size,
                    hash,
                )?
            }
            // Not kept: read as it is now, from the document's own folder.
            None => match folder.and_then(|folder| current(&runtime.path, folder, &asset.path)) {
                Some(data) if data.len() <= MAX_ASSET && total + data.len() <= MAX_BUNDLE => data,
                _ => {
                    missing.push(asset.path);
                    continue;
                }
            },
        };
        total += data.len();
        assets.push(view::Asset {
            path: asset.path,
            mime: asset.mime,
            data,
        });
    }
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
