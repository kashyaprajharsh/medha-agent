//! Which local files a page loads, read the same way wherever it is written or shown.

use regex::Regex;
use scraper::{Html, Selector};
use std::{
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

/// The type a page may load a local file as; anything else is never served to it.
pub fn mime(path: &Path) -> Option<&'static str> {
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
    // A script names a file only in quotes: `import.meta` or `fetch(apiUrl)` is no address.
    Regex::new(
        r#"(?i)(?:url\(\s*["']?|@import\s+["']?|\b(?:from|import)\s*["']|\bimport\(\s*["']|\bfetch\(\s*["'])([^\s"'<>);]+)"#,
    )
        .expect("asset references")
});

/// Every address a page, stylesheet or script names, at most 257 of them.
pub fn references(text: &str, html: bool) -> Vec<String> {
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

/// Where a named file sits inside the page's folder; `/x` is `x` there, as a built site means it.
pub fn relative(from: &Path, requested: &str) -> Option<PathBuf> {
    let requested = requested.split(['?', '#']).next()?.trim();
    let (from, requested) = match requested.strip_prefix('/') {
        Some(rest) if !rest.starts_with('/') => (Path::new(""), rest),
        Some(_) => return None,
        None => (from, requested),
    };
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
