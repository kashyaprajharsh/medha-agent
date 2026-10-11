//! The pages `read` has shown the model, so text pasted back from them with
//! their `N|` line numbers is recognised for what it is. A file whose own
//! lines happen to start with numbers and a bar is never mistaken for one:
//! only lines matching a page that was actually shown count.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// Recent pages kept; the model pastes from what it has just read.
const PAGES: usize = 32;

#[derive(Default, Clone)]
pub(crate) struct ShownPages(Arc<Mutex<VecDeque<Page>>>);

struct Page {
    start: u64,
    lines: Vec<String>,
}

impl ShownPages {
    /// A page as the model saw it: its first line's number, then each line.
    pub(crate) fn remember(&self, start: u64, lines: Vec<String>) {
        let Ok(mut pages) = self.0.lock() else {
            return;
        };
        pages.push_back(Page { start, lines });
        while pages.len() > PAGES {
            pages.pop_front();
        }
    }

    /// `text` with read line numbers removed, when it holds at least two
    /// consecutive lines copied from a shown page; `None` otherwise.
    pub(crate) fn pasted(&self, text: &str) -> Option<String> {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let numbered: Vec<Option<(u64, &str)>> = lines.iter().map(|line| gutter(line)).collect();
        let pages = self.0.lock().ok()?;
        let shown = |number: u64, rest: &str| {
            pages.iter().any(|page| {
                number
                    .checked_sub(page.start)
                    .and_then(|index| page.lines.get(index as usize))
                    .is_some_and(|line| line == rest)
            })
        };
        let copied: Vec<bool> = numbered
            .iter()
            .map(|entry| entry.is_some_and(|(number, rest)| shown(number, rest)))
            .collect();
        // A copy is two neighbours whose numbers follow on, both from a page.
        let pasted = numbered
            .windows(2)
            .zip(copied.windows(2))
            .any(|(pair, seen)| {
                matches!(pair, [Some((a, _)), Some((b, _))] if *b == a + 1) && seen[0] && seen[1]
            });
        if !pasted {
            return None;
        }
        Some(
            lines
                .iter()
                .zip(&numbered)
                .zip(&copied)
                .map(|((line, entry), copied)| match entry {
                    Some((_, rest)) if *copied => {
                        let ending = &line[line.trim_end_matches(['\n', '\r']).len()..];
                        format!("{rest}{ending}")
                    }
                    _ => (*line).to_owned(),
                })
                .collect(),
        )
    }
}

impl ShownPages {
    /// Why `text` (the tool's `field`) cannot be written as it is, when it
    /// carries line numbers copied from a read: with how it starts without them.
    pub(crate) fn refusal(&self, field: &str, text: &str) -> Option<String> {
        let clean = self.pasted(text)?;
        let start: Vec<&str> = clean.lines().take(3).collect();
        Some(format!(
            "`{field}` holds lines copied from a read with their `N|` line numbers. Those \
             numbers are not part of the file. Send it again without them; it would begin:\n{}",
            start.join("\n")
        ))
    }
}

/// `N|rest` split into its number and the rest of the line, without its end.
fn gutter(line: &str) -> Option<(u64, &str)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let (number, rest) = line.split_once('|')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, rest))
}

#[cfg(test)]
#[path = "shown_tests.rs"]
mod tests;
