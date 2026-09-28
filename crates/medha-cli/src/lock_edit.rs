//! Targeted edits to a project's `medha.lock`: one value changes, and every
//! comment, ordering and unrelated setting in the file stays as written.

use anyhow::{Result, bail};

/// Sets `[tools] preset`, adding the table when the file has none. Refuses
/// shapes it cannot edit safely (an inline `tools = {…}` or dotted keys).
pub(crate) fn set_tools_preset(text: &str, preset: &str) -> Result<String> {
    if !["full", "minimal"].contains(&preset) {
        bail!("The tools preset is full or minimal");
    }
    let line = format!("preset = \"{preset}\"");
    let lines: Vec<&str> = text.lines().collect();
    let header = lines.iter().position(|row| row.trim() == "[tools]");
    let edited = match header {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|row| row.trim_start().starts_with('['))
                .map_or(lines.len(), |offset| start + 1 + offset);
            let mut out: Vec<String> = lines.iter().map(|row| (*row).to_owned()).collect();
            match (start + 1..end).find(|&index| key_of(lines[index]) == Some("preset")) {
                Some(index) => {
                    let indent =
                        &lines[index][..lines[index].len() - lines[index].trim_start().len()];
                    out[index] = format!("{indent}{line}");
                }
                None => out.insert(start + 1, line),
            }
            let mut joined = out.join("\n");
            if text.ends_with('\n') {
                joined.push('\n');
            }
            joined
        }
        None => {
            if lines.iter().any(|row| {
                let key = key_of(row);
                key == Some("tools") || key.is_some_and(|key| key.starts_with("tools."))
            }) {
                bail!(
                    "medha.lock sets tools in a form Medha cannot edit safely; change it by hand"
                );
            }
            let mut joined = text.trim_end().to_owned();
            if !joined.is_empty() {
                joined.push_str("\n\n");
            }
            joined.push_str(&format!("[tools]\n{line}\n"));
            joined
        }
    };
    let parsed: lockfile::MedhaLock = toml::from_str(&edited)
        .map_err(|error| anyhow::anyhow!("The edit would make medha.lock invalid: {error}"))?;
    if parsed.tools.preset != preset {
        bail!("medha.lock did not take the new preset; change it by hand");
    }
    Ok(edited)
}

/// The bare key a TOML line assigns, ignoring comments and table headers.
fn key_of(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (key, _) = line.split_once('=')?;
    Some(key.trim())
}

#[cfg(test)]
#[path = "lock_edit_tests.rs"]
mod tests;
