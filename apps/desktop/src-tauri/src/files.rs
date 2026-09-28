use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_TEXT: u64 = 2 * 1024 * 1024;
const MAX_BINARY: u64 = 24 * 1024 * 1024;

pub fn resolve(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let path = root
        .join(requested)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !path.starts_with(root) {
        return Err("This file is outside the selected workspace".into());
    }
    Ok(path)
}
pub fn list(root: &Path, directory: &str) -> Result<Value, String> {
    let directory = resolve(root, directory)?;
    let mut rows = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "target") {
            continue;
        }
        let Ok(path) = entry.path().canonicalize() else {
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
        "csv" | "tsv" => "table",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "image",
        _ => "text",
    };
    let binary = matches!(kind, "pdf" | "document" | "spreadsheet" | "image");
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
    if binary {
        return Ok(
            json!({"kind": kind, "extension": extension, "size": metadata.len(), "bytes": bytes}),
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
        json!({"kind": kind, "extension": extension, "size": metadata.len(), "html": (kind == "markdown").then(|| transcript_view::to_html(&text)), "text": text}),
    )
}
pub fn open_default(root: &Path, requested: &str) -> Result<(), String> {
    open_path(&resolve(root, requested)?)
}
pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
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
        let workspace = root.join("workspace").canonicalize().unwrap();
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
