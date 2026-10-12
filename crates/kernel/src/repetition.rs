//! Whether a reply is mostly one piece of text repeated: a model caught in a loop.

use std::collections::HashMap;

/// Shorter text is never judged: it repeats words for honest reasons.
const MIN_LEN: usize = 400;
/// Only a repeat this long counts; ordinary writing does not reuse this much.
const WINDOW: usize = 60;
/// How often a piece must recur before it counts as a loop.
const REPEATS: usize = 5;
/// Where the search for a long repeated unit starts from, and how far each looks.
const ANCHORS: usize = 32;
const MATCHES: usize = 8;

/// True when repeats of one piece cover at least half of `text`, and its
/// lines are mostly copies. Rows of a batch share a long start but are each
/// different, and are never called a loop: that would throw away honest work.
pub(crate) fn looping(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < MIN_LEN {
        return false;
    }
    if !(one_line(text, chars.len()) || one_window(&chars) || one_long_unit(&chars)) {
        return false;
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let distinct: std::collections::HashSet<&str> = lines.iter().copied().collect();
    lines.len() < REPEATS || distinct.len() * 2 <= lines.len()
}

/// One line, ignoring its indentation, repeated enough to be half of the text.
fn one_line(text: &str, len: usize) -> bool {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        *counts.entry(line).or_default() += 1;
    }
    counts
        .iter()
        .any(|(line, count)| *count >= REPEATS && count * line.chars().count() * 2 >= len)
}

/// One window of text recurring often enough to cover half of it; catches
/// loops whose repeats differ by a counter or a stray word.
fn one_window(chars: &[char]) -> bool {
    let needed = REPEATS.max(chars.len().div_ceil(2 * WINDOW));
    let mut counts: HashMap<&[char], usize> = HashMap::new();
    chars.windows(WINDOW).any(|window| {
        let count = counts.entry(window).or_default();
        *count += 1;
        *count >= needed
    })
}

/// A long unit repeated exactly: each of its windows recurs too rarely for
/// [`one_window`], so the run is measured by its true period.
fn one_long_unit(chars: &[char]) -> bool {
    let len = chars.len();
    let last = len - WINDOW;
    let step = ((last + ANCHORS - 2) / (ANCHORS - 1)).max(1);
    let starts = (0..=last).step_by(step).chain([last]);
    // Runs already measured and found too short, as (left, right, period).
    let mut measured: Vec<(usize, usize, usize)> = Vec::new();
    for start in starts {
        let anchor = &chars[start..start + WINDOW];
        let mut from = start + 1;
        for _ in 0..MATCHES {
            let Some(found) = chars[from..].windows(WINDOW).position(|at| at == anchor) else {
                break;
            };
            let period = from + found - start;
            from += found + 1;
            let seen = |&(left, right, unit): &(usize, usize, usize)| {
                left <= start && start < right && period % unit == 0
            };
            if measured.iter().any(seen) {
                continue;
            }
            let (mut left, mut right) = (start, start + WINDOW);
            while left > 0 && chars[left - 1] == chars[left - 1 + period] {
                left -= 1;
            }
            while right + period < len && chars[right] == chars[right + period] {
                right += 1;
            }
            let run = right + period - left;
            if run >= REPEATS * period && run * 2 >= len {
                return true;
            }
            measured.push((left, right + period, period));
        }
    }
    false
}

#[cfg(test)]
#[path = "repetition_tests.rs"]
mod tests;
