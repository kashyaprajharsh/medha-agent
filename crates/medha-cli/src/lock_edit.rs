//! Targeted edits to a project's `medha.lock`: one value changes, and every
//! comment, ordering and unrelated setting in the file stays as written.

use anyhow::{Result, bail};

/// Sets `[tools] preset`, adding the table when the file has none. Refuses
/// shapes it cannot edit safely (an inline `tools = {…}` or dotted keys).
pub(crate) fn set_tools_preset(text: &str, preset: &str) -> Result<String> {
    if !["full", "minimal"].contains(&preset) {
        bail!("The tools preset is full or minimal");
    }
    let edited = set_tools_key(text, "preset", Some(format!("preset = \"{preset}\"")))?;
    if parse(&edited)?.tools.preset != preset {
        bail!("medha.lock did not take the new preset; change it by hand");
    }
    Ok(edited)
}

/// Turns one tool off, or back on, in `[tools] deny`. A pattern that still covers it is left for the person to change.
pub(crate) fn set_tool_denied(text: &str, tool: &str, off: bool) -> Result<String> {
    let mut deny = parse(text)?.tools.filter.deny;
    deny.retain(|entry| entry != tool);
    if off {
        deny.push(tool.to_owned());
    }
    let line = (!deny.is_empty()).then(|| format!("deny = {}", toml::Value::from(deny)));
    let edited = set_tools_key(text, "deny", line)?;
    if parse(&edited)?.tools.filter.denies(tool) != off {
        bail!("medha.lock turns {tool} off with a pattern; change it there by hand");
    }
    Ok(edited)
}

fn parse(text: &str) -> Result<lockfile::MedhaLock> {
    toml::from_str(text)
        .map_err(|error| anyhow::anyhow!("The edit would make medha.lock invalid: {error}"))
}

/// Writes `line` as `key` in `[tools]`, or removes the key for `None`, adding the table when needed.
fn set_tools_key(text: &str, key: &str, line: Option<String>) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let header = lines.iter().position(|row| row.trim() == "[tools]");
    let edited = match header {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|row| row.trim_start().starts_with('['))
                .map_or(lines.len(), |offset| start + 1 + offset);
            let mut out: Vec<String> = lines.iter().map(|row| (*row).to_owned()).collect();
            let found = (start + 1..end).find(|&index| key_of(lines[index]) == Some(key));
            if let Some(index) = found
                && lines[index].contains('[')
                && !lines[index].contains(']')
            {
                bail!("medha.lock spreads {key} over several lines; change it by hand");
            }
            match (found, line) {
                (Some(index), Some(line)) => {
                    let indent =
                        &lines[index][..lines[index].len() - lines[index].trim_start().len()];
                    out[index] = format!("{indent}{line}");
                }
                (Some(index), None) => {
                    out.remove(index);
                }
                (None, Some(line)) => out.insert(start + 1, line),
                (None, None) => {}
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
            let Some(line) = line else {
                return Ok(text.to_owned());
            };
            let mut joined = text.trim_end().to_owned();
            if !joined.is_empty() {
                joined.push_str("\n\n");
            }
            joined.push_str(&format!("[tools]\n{line}\n"));
            joined
        }
    };
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
