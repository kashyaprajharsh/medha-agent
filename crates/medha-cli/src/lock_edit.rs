//! Targeted edits to a project's `medha.lock`: one value changes, and every
//! comment, ordering and unrelated setting in the file stays as written.

use anyhow::{Result, bail};

/// Sets `[tools] preset`, adding the table when the file has none. Refuses
/// shapes it cannot edit safely (an inline `tools = {…}` or dotted keys).
pub(crate) fn set_tools_preset(text: &str, preset: &str) -> Result<String> {
    if !["full", "minimal"].contains(&preset) {
        bail!("The tools preset is full or minimal");
    }
    let edited = set_table_key(
        text,
        "tools",
        "preset",
        Some(format!("preset = \"{preset}\"")),
    )?;
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
    let edited = set_table_key(text, "tools", "deny", line)?;
    if parse(&edited)?.tools.filter.denies(tool) != off {
        bail!("medha.lock turns {tool} off with a pattern; change it there by hand");
    }
    Ok(edited)
}

/// Sets one `[budget]` limit, or removes it for `None` so the automatic value applies again.
pub(crate) fn set_budget_limit(
    text: &str,
    key: &str,
    value: Option<&serde_json::Value>,
) -> Result<String> {
    const WHOLE: [&str; 7] = [
        "max_turns",
        "max_tokens",
        "max_wall_s",
        "max_parallel_tools",
        "tool_result_bytes",
        "tool_round_bytes",
        "background_wakes",
    ];
    let line = match value {
        None => None,
        Some(value) if key == "max_cost_usd" => match value.as_f64() {
            Some(cost) if cost.is_finite() && cost > 0.0 => Some(format!("{key} = {cost:?}")),
            _ => bail!("The cost limit must be a number above zero"),
        },
        Some(value) if WHOLE.contains(&key) => match value.as_u64() {
            Some(number) if number > 0 => Some(format!("{key} = {number}")),
            _ => bail!("This limit must be a whole number above zero"),
        },
        Some(_) => bail!("{key} is not a limit Medha knows"),
    };
    if value.is_none() && key != "max_cost_usd" && !WHOLE.contains(&key) {
        bail!("{key} is not a limit Medha knows");
    }
    let edited = set_table_key(text, "budget", key, line)?;
    parse(&edited)?;
    Ok(edited)
}

fn parse(text: &str) -> Result<lockfile::MedhaLock> {
    toml::from_str(text)
        .map_err(|error| anyhow::anyhow!("The edit would make medha.lock invalid: {error}"))
}

/// Writes `line` as `key` in `[table]`, or removes the key for `None`, adding the table when needed.
fn set_table_key(text: &str, table: &str, key: &str, line: Option<String>) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let heading = format!("[{table}]");
    let header = lines.iter().position(|row| row.trim() == heading);
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
                key == Some(table) || key.is_some_and(|key| key.starts_with(&format!("{table}.")))
            }) {
                bail!(
                    "medha.lock sets {table} in a form Medha cannot edit safely; change it by hand"
                );
            }
            let Some(line) = line else {
                return Ok(text.to_owned());
            };
            let mut joined = text.trim_end().to_owned();
            if !joined.is_empty() {
                joined.push_str("\n\n");
            }
            joined.push_str(&format!("{heading}\n{line}\n"));
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
