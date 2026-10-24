use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_TEXT: u64 = 2 * 1024 * 1024;
const MAX_BINARY: u64 = 24 * 1024 * 1024;

pub fn resolve(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let path = dunce::canonicalize(root.join(requested)).map_err(|e| e.to_string())?;
    if !path.starts_with(root) {
        return Err("This file is outside the selected workspace".into());
    }
    Ok(path)
}
pub fn list(root: &Path, directory: &str) -> Result<Value, String> {
    // A chat that has not started has no folder yet, so nothing in it.
    if !root.exists() {
        return Ok(json!([]));
    }
    let directory = resolve(root, directory)?;
    let mut rows = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "target") {
            continue;
        }
        let Ok(path) = dunce::canonicalize(entry.path()) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }
        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        rows.push(json!({"name": name, "path": entry.path().strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy(), "directory": metadata.is_dir(), "size": metadata.len()}));
        if rows.len() > 4000 {
            return Err("This folder has more than 4,000 entries. Open a smaller folder.".into());
        }
    }
    rows.sort_by(|a, b| {
        b["directory"]
            .as_bool()
            .cmp(&a["directory"].as_bool())
            .then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .cmp(&b["name"].as_str().unwrap_or_default().to_lowercase())
            })
    });
    Ok(Value::Array(rows))
}
pub fn read(root: &Path, requested: &str) -> Result<Value, String> {
    let path = resolve(root, requested)?;
    let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Choose a file to preview".into());
    }
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let kind = match extension.as_str() {
        "md" | "markdown" | "mdx" => "markdown",
        "pdf" => "pdf",
        "docx" => "document",
        "xlsx" => "spreadsheet",
        "pptx" => "slides",
        "csv" | "tsv" => "table",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "image",
        _ => "text",
    };
    let binary = matches!(
        kind,
        "pdf" | "document" | "spreadsheet" | "slides" | "image"
    );
    let limit = if binary { MAX_BINARY } else { MAX_TEXT };
    if metadata.len() > limit {
        return Err(format!(
            "This file is too large to preview (limit {} MB). Open it in its default app.",
            limit / 1024 / 1024
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .map_err(|e| e.to_string())?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("The file grew beyond the preview limit".into());
    }
    preview_bytes(&path, bytes)
}

/// The same format adapter for current files and immutable backend revisions.
pub fn preview_bytes(path: &Path, bytes: Vec<u8>) -> Result<Value, String> {
    use base64::Engine;
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let kind = match extension.as_str() {
        "md" | "markdown" | "mdx" => "markdown",
        "pdf" => "pdf",
        "docx" => "document",
        "xlsx" => "spreadsheet",
        "pptx" => "slides",
        "csv" | "tsv" => "table",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "image",
        "mp4" | "m4v" | "mov" | "webm" | "mp3" | "wav" | "m4a" | "ogg" | "oga" | "flac" => "media",
        _ => "text",
    };
    let size = bytes.len();
    if matches!(
        kind,
        "pdf" | "document" | "spreadsheet" | "slides" | "image" | "media"
    ) {
        return Ok(
            json!({"kind": kind, "extension": extension, "size": size, "data": base64::engine::general_purpose::STANDARD.encode(bytes)}),
        );
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| "This binary format has no built-in preview. Open it in its default app.")?;
    if text.contains('\0') {
        return Err(
            "This binary format has no built-in preview. Open it in its default app.".into(),
        );
    }
    Ok(
        json!({"kind": kind, "extension": extension, "size": size, "html": (kind == "markdown").then(|| transcript_view::to_html(&text)), "text": text}),
    )
}
pub fn open_default(root: &Path, requested: &str) -> Result<Option<String>, String> {
    file_ops::opening::open_file(&resolve(root, requested)?)
}
pub fn open_path(path: &Path) -> Result<(), String> {
    file_ops::opening::open_path(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn previews_new_markdown_without_git_and_rejects_escape() {
        let root = std::env::temp_dir().join(format!("medha-preview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::write(root.join("outside.md"), "secret").unwrap();
        let workspace = dunce::canonicalize(root.join("workspace")).unwrap();
        fs::write(
            workspace.join("report.md"),
            "# Report\n\n<script>bad()</script>",
        )
        .unwrap();
        assert_eq!(list(&workspace, "").unwrap()[0]["name"], "report.md");
        let preview = read(&workspace, "report.md").unwrap();
        assert_eq!(preview["kind"], "markdown");
        assert!(preview["html"].as_str().unwrap().contains("&lt;script&gt;"));
        assert!(read(&workspace, "../outside.md").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside.md"), workspace.join("escape.md"))
                .unwrap();
            assert!(read(&workspace, "escape.md").is_err());
        }
        fs::write(workspace.join("binary"), [0, 3]).unwrap();
        assert!(read(&workspace, "binary").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
