//! The shared narrow scope for instruction and delayed-execution mutations.

use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Instructions,
    Scheduled,
    PackageScripts,
    Operator,
}

pub fn classify(path: &Path, after_web: bool) -> Option<Kind> {
    crate::egress::mutation_context()
        .and_then(|(guard, _)| guard.classify(path, after_web))
        .or_else(|| builtin(path, after_web))
}

pub(crate) fn builtin(path: &Path, after_web: bool) -> Option<Kind> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_ascii_lowercase()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let name = parts.last()?.as_str();
    if ["medha.md", "agents.md", "claude.md"].contains(&name)
        || parts
            .windows(2)
            .any(|pair| pair[0] == ".medha" && pair[1] == "prompts")
    {
        return Some(Kind::Instructions);
    }
    if !after_web {
        return None;
    }
    if name == "package.json" {
        return Some(Kind::PackageScripts);
    }
    if [".envrc", ".gitlab-ci.yml"].contains(&name)
        || parts
            .windows(2)
            .any(|pair| pair[0] == ".github" && pair[1] == "workflows")
        || (parts.len() >= 2
            && parts[parts.len() - 2] == ".vscode"
            && ["tasks.json", "launch.json"].contains(&name))
    {
        return Some(Kind::Scheduled);
    }
    None
}

pub fn matters(kind: Kind, before: Option<&[u8]>, after: Option<&[u8]>) -> bool {
    if before == after {
        return false;
    }
    if kind != Kind::PackageScripts {
        return true;
    }
    let scripts = |bytes: Option<&[u8]>| -> Result<Option<serde_json::Value>, serde_json::Error> {
        bytes
            .map(serde_json::from_slice::<serde_json::Value>)
            .transpose()
            .map(|value| value.and_then(|value| value.get("scripts").cloned()))
    };
    match (scripts(before), scripts(after)) {
        (Ok(before), Ok(after)) => before != after,
        _ => true,
    }
}

#[cfg(test)]
#[path = "protected_paths_tests.rs"]
mod tests;
