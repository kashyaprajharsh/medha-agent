//! Tool-name patterns shared by every tool filter: an exact name, or a prefix ending in one `*`.

use serde::{Deserialize, Serialize};

/// `allow`, when not empty, keeps only matching tools; `deny` then removes matches, so deny always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolFilter {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl ToolFilter {
    pub fn admits(&self, tool: &str) -> bool {
        !self.leaves_out(tool) && !self.denies(tool)
    }

    /// An allow list is set and names nothing matching `tool`.
    pub fn leaves_out(&self, tool: &str) -> bool {
        !self.allow.is_empty() && !self.allow.iter().any(|p| matches(p, tool))
    }

    pub fn denies(&self, tool: &str) -> bool {
        self.deny.iter().any(|p| matches(p, tool))
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Refuses a pattern that cannot mean what it says, naming the list it is in.
    pub fn validate(&self) -> Result<(), String> {
        for (list, patterns) in [("allow", &self.allow), ("deny", &self.deny)] {
            if let Some((pattern, why)) = patterns.iter().find_map(|p| flaw(p).map(|why| (p, why)))
            {
                return Err(format!("tools.{list} entry {pattern:?} {why}"));
            }
        }
        Ok(())
    }

    /// Entries without `*`: each should name a tool that exists.
    pub fn exact_names(&self) -> impl Iterator<Item = &str> {
        self.allow
            .iter()
            .chain(&self.deny)
            .map(String::as_str)
            .filter(|name| !name.contains('*'))
    }
}

fn flaw(pattern: &str) -> Option<&'static str> {
    if pattern.is_empty() {
        return Some("is empty");
    }
    if pattern.chars().any(char::is_whitespace) {
        return Some("contains a space");
    }
    match pattern.find('*') {
        Some(at) if at + 1 < pattern.len() => {
            Some("has a * before its end; only a trailing * is supported")
        }
        _ => None,
    }
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[cfg(test)]
#[path = "tool_pattern_tests.rs"]
mod tests;
