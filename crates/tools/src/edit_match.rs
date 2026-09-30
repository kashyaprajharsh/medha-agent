//! Finding `old_string` when the model's copy differs from the file only in
//! form: line endings, trailing spaces, a uniform indent shift, typography. A
//! copy that differs in content must fail, never overwrite what it misremembers.

use std::ops::Range;

pub(crate) const LOOSE_NOTE: &str =
    "Matched ignoring whitespace, indentation and quote or dash style.";

pub(crate) struct Replaced {
    pub text: String,
    pub count: usize,
    pub loose: bool,
}

pub(crate) enum Miss {
    NotFound,
    Ambiguous(usize),
}

pub(crate) fn replace(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Replaced, Miss> {
    let crlf = content.contains("\r\n");
    let (old, new) = match crlf {
        true if content.contains(old) => (old.to_string(), to_crlf(new)),
        true => (to_crlf(old), to_crlf(new)),
        false => (old.to_string(), new.to_string()),
    };
    let exact = content.matches(&old).count();
    if exact > 0 {
        if exact > 1 && !replace_all {
            return Err(Miss::Ambiguous(exact));
        }
        let text = if replace_all {
            content.replace(&old, &new)
        } else {
            content.replacen(&old, &new, 1)
        };
        let count = if replace_all { exact } else { 1 };
        return Ok(Replaced {
            text,
            count,
            loose: false,
        });
    }
    if old.trim().is_empty() {
        return Err(Miss::NotFound);
    }
    // A block matching one way here and another way there is two candidates.
    let mut found = folded(content, &old, &new);
    for candidate in by_lines(content, &old, &new, if crlf { "\r\n" } else { "\n" }) {
        let overlaps = |(seen, _): &(Range<usize>, String)| {
            seen.start < candidate.0.end && candidate.0.start < seen.end
        };
        if !found.iter().any(overlaps) {
            found.push(candidate);
        }
    }
    found.sort_by_key(|(range, _)| range.start);
    match found.len() {
        0 => Err(Miss::NotFound),
        n if n > 1 && !replace_all => Err(Miss::Ambiguous(n)),
        count => {
            let mut text = content.to_string();
            for (range, replacement) in found.into_iter().rev() {
                text.replace_range(range, &replacement);
            }
            Ok(Replaced {
                text,
                count,
                loose: true,
            })
        }
    }
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

fn fold_str(text: &str) -> String {
    text.chars().map(fold).collect()
}

/// What the model kept from `old` is taken from the file, so its typography survives.
fn folded(content: &str, old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    let drifted = |text: &str| text.chars().any(|c| fold(c) != c);
    if !drifted(content) && !drifted(old) {
        return Vec::new();
    }
    let mut origin = Vec::with_capacity(content.len() + 1);
    let mut haystack = String::with_capacity(content.len());
    for (at, c) in content.char_indices() {
        let folded = fold(c);
        origin.extend(std::iter::repeat_n(at, folded.len_utf8()));
        haystack.push(folded);
    }
    origin.push(content.len());
    let needle = fold_str(old);
    let kept: Vec<char> = needle.chars().collect();
    let written: Vec<char> = new.chars().collect();
    let prefix = kept
        .iter()
        .zip(&written)
        .take_while(|(a, b)| **a == fold(**b))
        .count();
    let room = kept.len().min(written.len()) - prefix;
    let suffix = kept
        .iter()
        .rev()
        .zip(written.iter().rev())
        .take(room)
        .take_while(|(a, b)| **a == fold(**b))
        .count();
    let middle: String = written[prefix..written.len() - suffix].iter().collect();
    haystack
        .match_indices(&needle)
        .map(|(at, _)| {
            let range = origin[at]..origin[at + needle.len()];
            let region: Vec<char> = content[range.clone()].chars().collect();
            let head: String = region[..prefix].iter().collect();
            let tail: String = region[region.len() - suffix..].iter().collect();
            (range, format!("{head}{middle}{tail}"))
        })
        .collect()
}

struct Line<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    next: usize,
}

fn lines(content: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    for piece in content.split_inclusive('\n') {
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let body = body.strip_suffix('\r').unwrap_or(body);
        out.push(Line {
            text: body,
            start,
            end: start + body.len(),
            next: start + piece.len(),
        });
        start += piece.len();
    }
    out
}

fn body(text: &str) -> (Vec<&str>, bool) {
    let (text, broken) = match text.strip_suffix('\n') {
        Some(rest) => (rest.strip_suffix('\r').unwrap_or(rest), true),
        None => (text, false),
    };
    if text.is_empty() && !broken {
        return (Vec::new(), false);
    }
    let split = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    (split, broken)
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn key(line: &str) -> String {
    fold_str(line.trim_start_matches([' ', '\t']).trim_end())
}

#[derive(Clone, Copy)]
enum Shift {
    None,
    By(char, isize),
}

impl Shift {
    fn between(copy: &str, file: &str) -> Option<Shift> {
        if copy == file {
            return Some(Shift::None);
        }
        let unit = copy.chars().chain(file.chars()).next()?;
        let uniform = |run: &str| run.chars().all(|c| c == unit);
        (uniform(copy) && uniform(file))
            .then(|| Shift::By(unit, file.len() as isize - copy.len() as isize))
    }

    fn exact(self, run: &str) -> Option<String> {
        match self {
            Shift::None => Some(run.to_string()),
            Shift::By(unit, by) if by >= 0 => {
                Some(format!("{}{run}", unit.to_string().repeat(by as usize)))
            }
            Shift::By(unit, by) => {
                let cut = by.unsigned_abs();
                run.chars()
                    .take(cut)
                    .all(|c| c == unit)
                    .then(|| run.get(cut..).map(str::to_string))
                    .flatten()
            }
        }
    }

    /// A new line shallower than the copy's base lands at the file's base.
    fn apply(self, line: &str) -> String {
        if line.trim().is_empty() {
            return line.to_string();
        }
        let run = indent(line);
        let rest = &line[run.len()..];
        let moved = self.exact(run).unwrap_or_else(|| match self {
            Shift::By(unit, _) => run.trim_start_matches(unit).to_string(),
            Shift::None => run.to_string(),
        });
        format!("{moved}{rest}")
    }
}

fn by_lines(content: &str, old: &str, new: &str, eol: &str) -> Vec<(Range<usize>, String)> {
    let file = lines(content);
    let (copy, copy_broken) = body(old);
    let (written, written_broken) = body(new);
    let n = copy.len();
    if n == 0 || n > file.len() {
        return Vec::new();
    }
    let keys: Vec<String> = copy.iter().map(|line| key(line)).collect();
    let Some(anchor) = keys.iter().position(|k| !k.is_empty()) else {
        return Vec::new();
    };
    let file_keys: Vec<String> = file.iter().map(|line| key(line.text)).collect();
    let shift_at = |at: usize| -> Option<Shift> {
        let window = &file[at..at + n];
        if file_keys[at + anchor] != keys[anchor] || file_keys[at..at + n] != keys[..] {
            return None;
        }
        let shift = Shift::between(indent(copy[anchor]), indent(window[anchor].text))?;
        (0..n)
            .filter(|&j| !keys[j].is_empty())
            .all(|j| shift.exact(indent(copy[j])).as_deref() == Some(indent(window[j].text)))
            .then_some(shift)
    };
    let prefix = copy
        .iter()
        .zip(&written)
        .take_while(|(a, b)| a == b)
        .count();
    let room = n.min(written.len()) - prefix;
    let suffix = copy
        .iter()
        .rev()
        .zip(written.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let mut found = Vec::new();
    let mut at = 0;
    while at + n <= file.len() {
        let Some(shift) = shift_at(at) else {
            at += 1;
            continue;
        };
        let window = &file[at..at + n];
        let last = &window[n - 1];
        let broken = last.next > last.end;
        let end = if copy_broken && broken {
            last.next
        } else {
            last.end
        };
        let mut out: Vec<String> = window[..prefix].iter().map(|l| l.text.into()).collect();
        out.extend(
            written[prefix..written.len() - suffix]
                .iter()
                .map(|line| shift.apply(line)),
        );
        out.extend(window[n - suffix..].iter().map(|l| l.text.to_string()));
        let mut replacement = out.join(eol);
        // A copy ending in a line break the file lacks at EOF adds none.
        if written_broken && (broken || !copy_broken) {
            replacement.push_str(eol);
        }
        found.push((window[0].start..end, replacement));
        at += n;
    }
    found
}

#[cfg(test)]
#[path = "edit_match_tests.rs"]
mod tests;
