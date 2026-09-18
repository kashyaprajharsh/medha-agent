//! Image paths written into the composer — typed, or dropped by the terminal.

use std::path::{Path, PathBuf};

const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "heic", "heif", "avif",
];

/// Resolve a path the user wrote: `~/` at home, relative under the workspace
/// root, absolute as written. Existence is the caller's business.
pub fn expand(root: &Path, raw: &str) -> PathBuf {
    let trimmed = unquote(raw.trim());
    match trimmed.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .unwrap_or_else(|| root.to_path_buf())
            .join(rest),
        None => root.join(trimmed),
    }
}

/// Paths in `text` that name an image file which exists, in the order written
/// and without duplicates. Code spans are skipped, so a path inside backticks
/// stays quoted text rather than becoming an attachment.
pub fn scan(text: &str, root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for token in tokens(&without_code_spans(text)) {
        let candidate = trim_delimiters(&token);
        if !has_image_extension(candidate) {
            continue;
        }
        let path = expand(root, candidate);
        if path.is_file() && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// True when `text` holds nothing but paths already recognised by [`scan`] —
/// what a terminal sends when files are dropped onto the composer.
pub fn only_paths(text: &str, paths: &[PathBuf], root: &Path) -> bool {
    let tokens = tokens(text);
    !tokens.is_empty()
        && tokens
            .iter()
            .all(|token| paths.contains(&expand(root, trim_delimiters(token))))
}

/// Prose wrapped around a path: `(shot.png)`, `see shot.png.`, `<shot.png>`.
fn trim_delimiters(token: &str) -> &str {
    token
        .trim_start_matches(['(', '[', '<'])
        .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>'])
}

/// Remove from `text` the paths of images that no tool could act on anyway:
/// anything outside the workspace, and anything already gone. A screenshot
/// dropped onto the composer is exactly that — the terminal writes a path under
/// `/var/folders/.../TemporaryItems` that is deleted moments later. Left in the
/// message it is not information, it is an invitation to chase a missing file.
/// A workspace path stays, because "resize assets/logo.png" needs its name.
pub fn strip_unreachable(text: &str, attached: &[PathBuf], root: &Path) -> String {
    let mut stripped = text.to_string();
    for path in attached {
        if path.starts_with(root) && path.is_file() {
            continue;
        }
        let written = path.to_string_lossy();
        for form in [
            format!("'{written}'"),
            format!("\"{written}\""),
            written.replace(' ', "\\ "),
            written.to_string(),
        ] {
            if let Some(at) = stripped.find(&form) {
                stripped.replace_range(at..at + form.len(), "");
                break;
            }
        }
    }
    collapse_spaces(&stripped)
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if !out.is_empty() {
            out.push('\n');
        }
        let mut spaced = false;
        for word in line.split_whitespace() {
            if spaced {
                out.push(' ');
            }
            out.push_str(word);
            spaced = true;
        }
    }
    out.trim().to_string()
}

pub fn has_image_extension(candidate: &str) -> bool {
    Path::new(candidate)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

fn unquote(token: &str) -> &str {
    let bytes = token.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'"'), Some(b'"')) | (Some(b'\''), Some(b'\'')) if token.len() >= 2 => {
            &token[1..token.len() - 1]
        }
        _ => token,
    }
}

/// Whitespace splits tokens, except where the terminal quoted the path it
/// dropped or escaped its spaces (`/tmp/screen\ shot.png`).
fn tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&' ') => {
                current.push(' ');
                characters.next();
            }
            '"' | '\'' if quote.is_none() && current.is_empty() => quote = Some(character),
            c if Some(c) == quote => {
                quote = None;
                tokens.push(std::mem::take(&mut current));
            }
            c if c.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn without_code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let fence = if after.starts_with("```") { "```" } else { "`" };
        rest = match after[fence.len()..].find(fence) {
            Some(end) => &after[fence.len() + end + fence.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[path = "refs_tests.rs"]
mod tests;
