//! Shared caller capture and one-release compatibility for search credentials.

use std::sync::Once;

static DEPRECATION: Once = Once::new();

pub fn key(
    explicit: Option<&str>,
    captured: bool,
    canonical: &str,
    legacy: &str,
) -> Option<String> {
    resolve(explicit, captured, canonical, legacy, |name| {
        std::env::var(name).ok()
    })
}

fn resolve(
    explicit: Option<&str>,
    captured: bool,
    canonical: &str,
    legacy: &str,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let nonempty = |value: String| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    };
    let current = (!captured)
        .then(|| read(canonical))
        .flatten()
        .and_then(nonempty);
    let old = (!captured)
        .then(|| read(legacy))
        .flatten()
        .and_then(nonempty);
    for (name, value) in [
        (canonical, current.as_deref()),
        (legacy, old.as_deref()),
        (canonical, explicit),
    ] {
        if let Some(value) = value {
            kernel::redact::remember_named(name, value);
        }
    }
    if old.is_some() {
        DEPRECATION.call_once(|| eprintln!("Medha: TAVILY_API_KEY and BRAVE_API_KEY are deprecated for one release; use MEDHA_TAVILY_API_KEY and MEDHA_BRAVE_API_KEY"));
    }
    explicit
        .map(str::to_owned)
        .and_then(nonempty)
        .or(current)
        .or(old)
}

#[cfg(test)]
#[path = "search_environment_tests.rs"]
mod tests;
