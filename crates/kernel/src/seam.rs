//! Where a reply goes on after it stopped early, a model often begins by
//! writing the end of what it had already written. That repeat is held back
//! as it arrives and never shown or saved twice.

use crate::types::{ContentPart, ModelMessage};

/// The shortest repeat worth cutting. A shorter one is ordinary writing.
const MIN: usize = 32;
/// A repeat has this many different characters. A rule of dashes or a run
/// of indentation that simply goes on does not.
const VARIED: usize = 8;
/// How far back from the end a repeat may begin. It bounds the work a reply
/// of one endless character could cost; a longer repeat is shown as written.
const REACH: usize = 8_000;

/// What a reply being continued has written so far, and how many characters
/// of it the newest piece wrote again.
#[derive(Debug, Default)]
pub(crate) struct Join {
    pub written: String,
    pub cut: usize,
}

/// Reads the start of a continuation against the text it continues.
pub(crate) struct Seam {
    before: Vec<char>,
    /// The start of the continuation, not yet shown.
    held: Vec<char>,
    /// Places in `before` from which every held character has matched so far.
    starts: Vec<usize>,
    /// How much of `held` is known to repeat the very end of `before`.
    repeat: usize,
    /// Characters cut as a repeat, once the join is settled.
    pub cut: usize,
    settled: bool,
}

impl Seam {
    pub(crate) fn after(before: &str) -> Self {
        let mut before: Vec<char> = before.chars().rev().take(REACH).collect();
        before.reverse();
        Self {
            // Too little was written for a repeat of it to be told from writing.
            settled: before.len() < MIN,
            starts: (0..before.len()).collect(),
            before,
            held: Vec::new(),
            repeat: 0,
            cut: 0,
        }
    }

    /// Take the next piece of the continuation; what comes back may be shown.
    pub(crate) fn feed(&mut self, piece: &str) -> String {
        if self.settled {
            return piece.to_owned();
        }
        for next in piece.chars() {
            if self.settled {
                self.held.push(next);
                continue;
            }
            let at = self.held.len();
            let before = &self.before;
            self.starts
                .retain(|start| before.get(start + at) == Some(&next));
            self.held.push(next);
            // One of them has now run to the very end: this much is a repeat.
            if self
                .starts
                .iter()
                .any(|start| start + at + 1 == before.len())
            {
                self.repeat = at + 1;
            }
            // Nothing written before matches any further: the join is known.
            if self
                .starts
                .iter()
                .all(|start| start + at + 1 >= before.len())
            {
                self.settle();
            }
        }
        match self.settled {
            true => std::mem::take(&mut self.held).into_iter().collect(),
            false => String::new(),
        }
    }

    /// The continuation ended: what is still held, less any repeat in it.
    pub(crate) fn finish(&mut self) -> String {
        if !self.settled {
            self.settle();
        }
        std::mem::take(&mut self.held).into_iter().collect()
    }

    /// Characters taken so far that were not shown: cut, or still held.
    /// While held, they match text already written, so dropping them loses nothing.
    pub(crate) fn hidden(&self) -> usize {
        self.cut + self.held.len()
    }

    fn settle(&mut self) {
        self.settled = true;
        let repeated = &self.held[..self.repeat];
        let mut different: Vec<char> = repeated.to_vec();
        different.sort_unstable();
        different.dedup();
        if self.repeat >= MIN && different.len() >= VARIED {
            self.cut = self.repeat;
            self.held.drain(..self.repeat);
        }
    }
}

/// Drop the first `count` characters of `text`.
pub(crate) fn without_start(text: &mut String, count: usize) {
    let end = text
        .char_indices()
        .nth(count)
        .map_or(text.len(), |(at, _)| at);
    text.drain(..end);
}

/// Drop the first `count` characters of the text a message carries.
pub(crate) fn without_start_of(message: &mut ModelMessage, mut count: usize) {
    for part in &mut message.parts {
        let ContentPart::Text(part) = part else {
            continue;
        };
        if count == 0 {
            return;
        }
        let had = part.text.chars().count();
        without_start(&mut part.text, count);
        count = count.saturating_sub(had);
    }
}

#[cfg(test)]
#[path = "seam_tests.rs"]
mod tests;
