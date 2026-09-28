use crate::tool::{OUTPUT_LIMIT, bounded, clip, counted, failure_detail, fields, grouped, text};
use serde_json::Value;

/// What came back from a tool: a short result beside the step, and the body a
/// person reads when they open it — file text, command output, matches, a
/// diff, reports. Never the raw JSON the model was given.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub summary: Option<String>,
    pub output: Option<String>,
    pub detail: Option<String>,
}

pub(crate) fn outcome(name: &str, ok: bool, out: &Value) -> Outcome {
    let detail = (!ok).then(|| failure_detail(out)).flatten();
    let only_reason = out.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| key == "error" || key == "reason" || key.starts_with('_'))
    });
    Outcome {
        summary: summary(name, out),
        output: (!only_reason)
            .then(|| body(name, out))
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .map(|text| bounded(&text, OUTPUT_LIMIT)),
        detail,
    }
}

fn number(out: &Value, key: &str) -> Option<u64> {
    out.get(key).and_then(Value::as_u64)
}

fn list<'a>(out: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    out.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn summary(name: &str, out: &Value) -> Option<String> {
    let more = if out.get("truncated") == Some(&Value::Bool(true)) {
        "+"
    } else {
        ""
    };
    match name {
        "shell.exec" | "git" => out
            .get("exit_code")
            .and_then(Value::as_i64)
            .filter(|code| *code != 0)
            .map(|code| format!("exit {code}")),
        "read" => match (
            number(out, "start_line"),
            number(out, "end_line"),
            number(out, "total_lines"),
        ) {
            (Some(start), Some(end), Some(total)) => Some(format!(
                "lines {}–{} of {}",
                grouped(start),
                grouped(end),
                grouped(total)
            )),
            _ => number(out, "lines")
                .map(|lines| counted(lines, "line", "lines"))
                .or_else(|| {
                    out.get("hash").is_none().then_some(())?;
                    let lines = text(out, "content")?.lines().count() as u64;
                    Some(counted(lines, "line", "lines"))
                }),
        },
        "grep" => number(out, "count").map(|n| counted(n, "match", "matches") + more),
        "glob" => number(out, "count").map(|n| counted(n, "file", "files") + more),
        "ls" => number(out, "entries")
            .or_else(|| Some(out.get("entries")?.as_array()?.len() as u64))
            .map(|n| counted(n, "entry", "entries") + more),
        "web.search" => number(out, "count").map(|n| counted(n, "result", "results")),
        "web.fetch" => text(out, "title").map(|title| clip(title, 60)),
        "edit" | "write" => text(out, "diff").map(changed_lines),
        "agent.spawn" => match number(out, "count") {
            Some(count) => Some(format!("{count} started")),
            None => (text(out, "status") == Some("running")).then(|| "started".into()),
        },
        "agent.wait" if out.get("timed_out") == Some(&Value::Bool(true)) => {
            Some("still running".into())
        }
        "agent.wait" => {
            let names: Vec<&str> = list(out, "reports")
                .filter_map(|report| text(report, "agent"))
                .map(|agent| agent.trim_start_matches('/'))
                .collect();
            match names.as_slice() {
                [] => None,
                [one] => Some(format!("{one} reported")),
                many => Some(format!("{} reported", many.len())),
            }
        }
        "agent.list" => Some(format!("{} running", list(out, "agents").count())),
        "agent.transcript" => match (number(out, "showing"), number(out, "total")) {
            (Some(showing), Some(total)) if showing < total => {
                Some(format!("last {showing} of {total} steps"))
            }
            (_, Some(total)) => Some(counted(total, "step", "steps")),
            _ => None,
        },
        "update_plan" => match (number(out, "done"), number(out, "total")) {
            (Some(done), Some(total)) => Some(format!("{done} of {total} done")),
            _ => None,
        },
        _ => None,
    }
}

fn changed_lines(diff: &str) -> String {
    let lines = diff
        .lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"));
    let (added, removed) = lines.fold((0, 0), |(added, removed), line| {
        match line.as_bytes().first() {
            Some(b'+') => (added + 1, removed),
            Some(b'-') => (added, removed + 1),
            _ => (added, removed),
        }
    });
    format!("+{added} −{removed}")
}

fn body(name: &str, out: &Value) -> Option<String> {
    match name {
        "shell.exec" | "git" => Some(terminal(out)),
        "read" | "web.fetch" => text(out, "content").map(unwrap_stored),
        "edit" | "write" => text(out, "diff").map(str::to_owned),
        "grep" => Some(
            list(out, "matches")
                .map(|found| match found.as_str() {
                    Some(line) => line.to_owned(),
                    None => format!(
                        "{}:{}: {}",
                        text(found, "path").unwrap_or_default(),
                        found["line"],
                        text(found, "text").unwrap_or_default()
                    ),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "glob" => Some(lines(list(out, "matches"))),
        "ls" => text(out, "tree")
            .map(str::to_owned)
            .or_else(|| Some(lines(list(out, "entries")))),
        "web.search" => Some(
            list(out, "results")
                .map(|result| {
                    [text(result, "title"), text(result, "url")]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        "agent.wait" => Some(
            list(out, "reports")
                .map(|report| {
                    format!(
                        "{}\n{}",
                        text(report, "agent")
                            .unwrap_or("agent")
                            .trim_start_matches('/'),
                        text(report, "report").unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        "agent.transcript" => Some(lines(list(out, "steps"))),
        "agent.list" => Some(
            list(out, "agents")
                .map(|agent| {
                    let name = ["name", "path", "agent"]
                        .iter()
                        .find_map(|key| text(agent, key))
                        .unwrap_or("agent");
                    let doing = text(agent, "doing").unwrap_or_default();
                    let objective = clip(text(agent, "objective").unwrap_or_default(), 160);
                    format!("{} · {doing}\n{objective}", name.trim_start_matches('/'))
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        "agent.spawn" | "agent.steer" | "agent.message" | "agent.followup" | "agent.cancel"
        | "update_plan" => None,
        _ => fields(out, OUTPUT_LIMIT),
    }
}

fn lines<'a>(items: impl Iterator<Item = &'a Value>) -> String {
    items
        .map(|item| match item.as_str() {
            Some(text) => text.to_owned(),
            None => item.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal(out: &Value) -> String {
    let mut text = out["stdout"]
        .as_str()
        .unwrap_or_default()
        .trim_end()
        .to_owned();
    let stderr = out["stderr"].as_str().unwrap_or_default().trim_end();
    if !stderr.is_empty() {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(stderr);
    }
    if out.get("stdout_truncated") == Some(&Value::Bool(true))
        || out.get("stderr_truncated") == Some(&Value::Bool(true))
    {
        text.push_str("\n… output cut short");
    }
    text
}

/// A stored output read back by hash is a page of a tool result's JSON. A whole
/// result is unwrapped to its `content`; a page cut from the middle of that
/// JSON is decoded back to the text it escapes, up to where the string ends.
fn unwrap_stored(content: &str) -> String {
    if let Some(inner) = serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| text(&value, "content").map(str::to_owned))
    {
        return inner;
    }
    if content.contains('\n') || !content.contains("\\n") {
        return content.to_owned();
    }
    let body = content
        .find("\"content\":\"")
        .map_or(content, |at| &content[at + "\"content\":\"".len()..]);
    unescape(body)
}

fn unescape(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => {}
                Some('u') => {
                    let code: String = chars.by_ref().take(4).collect();
                    if let Some(decoded) =
                        u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                    {
                        out.push(decoded);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}
