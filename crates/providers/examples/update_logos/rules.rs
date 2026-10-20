//! What Medha accepts as a provider logo: a small vector mark that loads nothing and runs nothing.

pub const LIMIT: usize = 32 * 1024;

/// Why a logo is refused, or `None` when it is kept.
pub fn refusal(data: &[u8]) -> Option<&'static str> {
    if data.len() > LIMIT {
        return Some("larger than 32 KB");
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return Some("not text");
    };
    let text = text.to_ascii_lowercase();
    if !after_prologue(&text).starts_with("<svg") {
        return Some("not an SVG");
    }
    for (part, why) in [
        ("<script", "a script"),
        ("javascript:", "a script"),
        ("<foreignobject", "an embedded page"),
        ("<iframe", "an embedded page"),
        ("<image", "an embedded picture"),
        ("<!entity", "an entity"),
        ("<!doctype", "a doctype"),
    ] {
        if text.contains(part) {
            return Some(why);
        }
    }
    if has_handler(&text) {
        return Some("an event handler");
    }
    if links_outside(&text) {
        return Some("a link outside the file");
    }
    None
}

/// The text after an XML declaration and any leading comments.
fn after_prologue(text: &str) -> &str {
    let mut rest = text.trim_start();
    if rest.starts_with("<?xml") {
        rest = rest
            .split_once("?>")
            .map_or("", |(_, after)| after)
            .trim_start();
    }
    while rest.starts_with("<!--") {
        rest = rest
            .split_once("-->")
            .map_or("", |(_, after)| after)
            .trim_start();
    }
    rest
}

/// An attribute such as ` onload=`: whitespace, `on`, letters, then `=`.
fn has_handler(text: &str) -> bool {
    text.match_indices("on").any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let rest = text[at + 2..].trim_start_matches(|c: char| c.is_ascii_lowercase());
        before.is_some_and(char::is_whitespace)
            && rest.len() < text.len() - at - 2
            && rest.trim_start().starts_with('=')
    })
}

/// Any `href` whose value is not a reference inside the same file.
fn links_outside(text: &str) -> bool {
    text.match_indices("href").any(|(at, _)| {
        let rest = text[at + 4..].trim_start();
        let Some(value) = rest.strip_prefix('=') else {
            return false;
        };
        !value
            .trim_start()
            .trim_start_matches(['"', '\''])
            .starts_with('#')
    })
}
