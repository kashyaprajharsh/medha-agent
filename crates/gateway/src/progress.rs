//! The tool steps of one stretch of work, drawn as one message the chat rewrites as they run:
//! the same verb and target the desktop shows, never a tool's output.

use crate::telegram_format::code;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;

/// Lines kept on screen; older steps are counted instead.
const SHOWN: usize = 12;

#[derive(Default)]
pub(crate) struct Progress {
    described: transcript_view::Steps,
    lines: VecDeque<Line>,
    earlier: usize,
}

struct Line {
    id: Option<String>,
    text: String,
    mark: Mark,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Running,
    Done,
    Failed,
}

impl Progress {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// A path inside `folder` is named from the folder, as the person thinks of it.
    pub fn call(&mut self, id: Option<&str>, tool: &str, args: &Value, folder: &Path) {
        let call = self.described.call(id.unwrap_or_default(), tool, args);
        let mut text = call.verb;
        if let Some(target) = call.target {
            let shown = Path::new(&target)
                .strip_prefix(folder)
                .ok()
                .filter(|inside| !inside.as_os_str().is_empty())
                .map_or(target.clone(), |inside| inside.display().to_string());
            text.push(' ');
            text.push_str(&code(&shown));
        }
        if self.lines.len() == SHOWN {
            self.lines.pop_front();
            self.earlier += 1;
        }
        self.lines.push_back(Line {
            id: id.map(str::to_owned),
            text,
            mark: Mark::Running,
        });
    }

    /// A result for a step no longer on screen changes nothing.
    pub fn result(&mut self, id: Option<&str>, ok: bool) {
        let line = match id {
            Some(id) => self
                .lines
                .iter_mut()
                .find(|line| line.id.as_deref() == Some(id)),
            None => self
                .lines
                .iter_mut()
                .find(|line| line.mark == Mark::Running),
        };
        if let Some(line) = line {
            line.mark = if ok { Mark::Done } else { Mark::Failed };
        }
    }

    /// The message as Markdown, one step a line.
    pub fn text(&self) -> String {
        let mut text = String::new();
        if self.earlier > 0 {
            let steps = if self.earlier == 1 { "step" } else { "steps" };
            text.push_str(&format!("{} earlier {steps}\n", self.earlier));
        }
        for line in &self.lines {
            let mark = match line.mark {
                Mark::Running => "◦",
                Mark::Done => "✓",
                Mark::Failed => "✗",
            };
            text.push_str(&format!("{mark} {}\n", line.text));
        }
        text.trim_end().to_owned()
    }

    /// The next stretch starts a new message; what was described stays known for its results.
    pub fn close(&mut self) {
        self.lines.clear();
        self.earlier = 0;
    }
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
