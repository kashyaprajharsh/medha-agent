//! A PowerShell line read to its plain commands, or not read at all.
//!
//! Only a small literal subset is understood: plain commands and arguments, quotes whose value is
//! fixed, pipes, `;`, `&&`, `||` and comments. Anything else (variables, script blocks, redirects
//! to files, subexpressions, version-dependent escapes) leaves the line unread, so it still asks.

use crate::{Command, Reading, Shell, Value, Word};
use tree_sitter::{Node, Parser};

pub(crate) fn read(line: &str) -> Reading {
    match lower(&without_discards(line)) {
        Ok(commands) => Reading {
            commands: commands
                .into_iter()
                .map(|words| Command {
                    words: words
                        .iter()
                        .map(|word| Word::new(word, Value::Literal))
                        .collect(),
                    shell: Shell::PowerShell,
                    ..Command::default()
                })
                .collect(),
            complete: true,
            self_forking: false,
        },
        Err(_) => Reading::default(),
    }
}

/// The line with output thrown away (`> $null`, `2>$null`, `*>$null`) and stream merges (`2>&1`)
/// blanked out; they write no file. Only whole tokens outside quotes are touched.
fn without_discards(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = chars.clone();
    let (mut single, mut double) = (false, false);
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if single {
            single = ch != '\'';
            index += 1;
            continue;
        }
        if double {
            if ch == '`' {
                index += 2;
                continue;
            }
            double = ch != '"';
            index += 1;
            continue;
        }
        match ch {
            '\'' => single = true,
            '"' => double = true,
            '`' => {
                index += 2;
                continue;
            }
            _ => {}
        }
        let at_boundary = index == 0 || chars[index - 1].is_whitespace() || chars[index - 1] == ';';
        if at_boundary && let Some(end) = discard_end(&chars, index) {
            for slot in &mut out[index..end] {
                *slot = ' ';
            }
            index = end;
            continue;
        }
        index += 1;
    }
    out.into_iter().collect()
}

/// Where a discarding redirect that starts at `start` ends, if one does.
fn discard_end(chars: &[char], start: usize) -> Option<usize> {
    let mut index = start;
    if chars
        .get(index)
        .is_some_and(|ch| ch.is_ascii_digit() || *ch == '*')
    {
        index += 1;
    }
    if chars.get(index) != Some(&'>') {
        return None;
    }
    index += 1;
    // A merge into another stream, such as 2>&1, opens no file.
    if chars.get(index) == Some(&'&') && chars.get(index + 1).is_some_and(char::is_ascii_digit) {
        return ends_token(chars, index + 2);
    }
    while chars.get(index).is_some_and(|ch| *ch == ' ' || *ch == '\t') {
        index += 1;
    }
    let word: String = chars.get(index..index + 5)?.iter().collect();
    if !word.eq_ignore_ascii_case("$null") {
        return None;
    }
    ends_token(chars, index + 5)
}

fn ends_token(chars: &[char], end: usize) -> Option<usize> {
    match chars.get(end) {
        None => Some(end),
        Some(ch) if ch.is_whitespace() || matches!(ch, ';' | '|' | ')') => Some(end),
        Some(_) => None,
    }
}

fn lower(script: &str) -> Result<Vec<Vec<String>>, String> {
    // PowerShell reads these Unicode characters as quotes and dashes; such a line stays unread.
    if script
        .chars()
        .any(|ch| matches!(ch, '‘' | '’' | '“' | '”' | '–' | '—' | '―'))
    {
        return Err("Unicode syntax alias".into());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_powershell::LANGUAGE.into())
        .map_err(|error| format!("load grammar: {error}"))?;
    // The grammar rejects native `--flag=value`; only that `=` is blanked, keeping every offset.
    let mut source = script.as_bytes().to_vec();
    for equals in inline_double_dash_equals(script) {
        source[equals] = b' ';
    }
    let source = String::from_utf8(source).map_err(|_| "source is not UTF-8".to_string())?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| "no tree".to_string())?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("tree has errors".into());
    }
    if has_requires_directive(root, script) {
        return Err("#requires runs before the script".into());
    }
    if let Some(kind) = first_unknown_kind(root) {
        return Err(format!("unknown node: {kind}"));
    }
    let mut nodes = Vec::new();
    collect_commands(root, &mut nodes);
    if nodes.is_empty() {
        return Err("no commands".into());
    }
    let mut commands = Vec::with_capacity(nodes.len());
    let mut ranges = Vec::with_capacity(nodes.len());
    for node in nodes {
        let text = node
            .utf8_text(script.as_bytes())
            .map_err(|_| "command is not UTF-8".to_string())?;
        ranges.push(node.start_byte()..node.end_byte());
        commands.push(lower_command(text)?);
    }
    if !covered_by_commands(script, &ranges) {
        return Err("text outside the commands".into());
    }
    if commands.iter().any(|command| {
        command
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("using"))
    }) {
        return Err("using declarations load code".into());
    }
    if commands
        .iter()
        .any(|command| command.is_empty() || command.iter().any(String::is_empty))
    {
        return Err("empty command or word".into());
    }
    Ok(commands)
}

fn collect_commands<'tree>(root: Node<'tree>, commands: &mut Vec<Node<'tree>>) {
    // Nesting is the model's to choose, so the walk keeps off the call stack.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            commands.push(node);
            continue;
        }
        for child in (0..node.named_child_count()).rev() {
            if let Some(child) = node.named_child(child) {
                stack.push(child);
            }
        }
    }
}

/// `#requires` looks like a comment but runs before the script and can load modules.
fn has_requires_directive(root: Node<'_>, script: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "comment"
            && node
                .utf8_text(script.as_bytes())
                .ok()
                .is_some_and(|comment| {
                    comment
                        .trim_start()
                        .to_ascii_lowercase()
                        .starts_with("#requires")
                })
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// Only these grammar forms are understood; a new one leaves the line unread until it is reviewed.
fn first_unknown_kind(root: Node<'_>) -> Option<String> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_named() && !known_kind(node.kind()) {
            return Some(node.kind().to_string());
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    None
}

fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "program"
            | "statement_list"
            | "pipeline"
            | "pipeline_chain"
            | "pipeline_chain_tail"
            | "command"
            | "command_name"
            | "command_elements"
            | "command_argument_sep"
            | "command_parameter"
            | "generic_token"
            | "array_literal_expression"
            | "unary_expression"
            | "string_literal"
            | "verbatim_string_characters"
            | "expandable_string_literal"
            | "integer_literal"
            | "decimal_integer_literal"
            | "comment"
            | "empty_statement"
            // A negative number such as `git log -1`.
            | "expression_with_unary_operator"
    )
}

/// Where the `=` of each bare `--flag=value` token is.
fn inline_double_dash_equals(script: &str) -> Vec<usize> {
    let mut equals = Vec::new();
    let mut start = 0;
    for (index, ch) in script.char_indices() {
        if ch.is_whitespace() || "|&;#><(){}".contains(ch) {
            push_equals(&mut equals, script, start, index);
            start = index + ch.len_utf8();
        }
    }
    push_equals(&mut equals, script, start, script.len());
    equals
}

fn push_equals(equals: &mut Vec<usize>, script: &str, start: usize, end: usize) {
    let token = &script[start..end];
    if token.starts_with("--")
        && token
            .chars()
            .all(|ch| !(ch.is_whitespace() || rejected_bare(ch) || matches!(ch, '`' | '#')))
        && let Some(at) = token.find('=')
        && at > 2
        && at + 1 < token.len()
    {
        equals.push(start + at);
    }
}

/// Every character outside the commands must be a separator, a comment or a grouping the walk understood.
fn covered_by_commands(script: &str, ranges: &[std::ops::Range<usize>]) -> bool {
    let mut index = 0;
    let mut range = 0;
    let mut can_chain = false;
    let mut needs_command = false;
    let mut parens = 0;
    while index < script.len() {
        if let Some(next) = ranges.get(range)
            && index == next.start
        {
            index = next.end;
            range += 1;
            can_chain = true;
            needs_command = false;
            continue;
        }
        let Some(ch) = script[index..].chars().next() else {
            return false;
        };
        let next = index + ch.len_utf8();
        if ch == '\r' || ch == '\n' {
            can_chain = false;
            index = next;
            continue;
        }
        if ch.is_whitespace() {
            index = next;
            continue;
        }
        if ch == ';' {
            if needs_command {
                return false;
            }
            can_chain = false;
            index = next;
            continue;
        }
        if ch == '(' && !can_chain {
            parens += 1;
            index = next;
            continue;
        }
        if ch == ')' && parens > 0 && !needs_command {
            parens -= 1;
            index = next;
            continue;
        }
        if ch == '|' && can_chain {
            can_chain = false;
            needs_command = true;
            index = if script[next..].starts_with('|') {
                next + 1
            } else {
                next
            };
            continue;
        }
        if ch == '&' && can_chain && script[next..].starts_with('&') {
            can_chain = false;
            needs_command = true;
            index = next + 1;
            continue;
        }
        if ch == '#' && !needs_command {
            // A `#` inside a word is no comment; reading it as one would drop the rest of the line.
            if index > 0
                && !script[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|previous| previous.is_whitespace() || previous == ';')
            {
                return false;
            }
            index = script[next..]
                .find(['\r', '\n'])
                .map_or(script.len(), |offset| next + offset);
            continue;
        }
        if script[index..].starts_with("<#")
            && !needs_command
            && (index == 0
                || script[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|previous| previous.is_whitespace() || previous == ';'))
        {
            let Some(end) = script[next..].find("#>") else {
                return false;
            };
            index = next + end + 2;
            continue;
        }
        return false;
    }
    range == ranges.len() && !needs_command && parens == 0
}

/// A command's words with their quotes and escapes decoded, only where the value is fixed in the text.
fn lower_command(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let chars: Vec<char> = text.trim().chars().collect();
    let mut index = 0;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index == chars.len() || chars[index] == '#' {
            break;
        }
        let (word, next, bare) = match chars[index] {
            '\'' => {
                let (word, next) = single_quoted(&chars, index)?;
                (word, next, false)
            }
            '"' => {
                let (word, next) = double_quoted(&chars, index)?;
                (word, next, false)
            }
            _ => {
                let (word, next) = bare_word(&chars, index)?;
                (word, next, true)
            }
        };
        index = next;
        if index < chars.len() && !chars[index].is_whitespace() && chars[index] != '#' {
            return Err("joined words".into());
        }
        if word.is_empty() {
            return Err("empty word".into());
        }
        if bare {
            plain_bare_word(&word)?;
        }
        words.push(word);
    }
    if words.is_empty() {
        return Err("no words".into());
    }
    Ok(words)
}

fn single_quoted(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut value = String::new();
    let mut index = start + 1;
    while index < chars.len() {
        if chars[index] == '\'' {
            if chars.get(index + 1) == Some(&'\'') {
                value.push('\'');
                index += 2;
                continue;
            }
            return Ok((value, index + 1));
        }
        value.push(chars[index]);
        index += 1;
    }
    Err("unclosed single quote".into())
}

fn double_quoted(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut value = String::new();
    let mut index = start + 1;
    while index < chars.len() {
        match chars[index] {
            '"' => return Ok((value, index + 1)),
            '$' => return Err("a variable in a string".into()),
            '`' => {
                let escaped = *chars
                    .get(index + 1)
                    .ok_or_else(|| "escape at the end".to_string())?;
                // `e is ESC only from PowerShell 6, so its value depends on the version.
                if escaped == 'e' {
                    return Err("version-dependent escape".into());
                }
                if escaped == 'u' && chars.get(index + 2) == Some(&'{') {
                    return Err("Unicode escape".into());
                }
                value.push(unescape(escaped));
                index += 2;
            }
            ch => {
                value.push(ch);
                index += 1;
            }
        }
    }
    Err("unclosed double quote".into())
}

fn unescape(ch: char) -> char {
    match ch {
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        other => other,
    }
}

fn bare_word(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut value = String::new();
    let mut index = start;
    while index < chars.len() && !chars[index].is_whitespace() {
        let ch = chars[index];
        if ch == '`' {
            let escaped = *chars
                .get(index + 1)
                .ok_or_else(|| "escape at the end".to_string())?;
            if escaped == 'e' {
                return Err("version-dependent escape".into());
            }
            value.push(unescape(escaped));
            index += 2;
            continue;
        }
        if rejected_bare(ch) {
            return Err(format!("dynamic or structural character {ch:?}"));
        }
        value.push(ch);
        index += 1;
    }
    if value.is_empty() {
        return Err("empty word".into());
    }
    Ok((value, index))
}

fn rejected_bare(ch: char) -> bool {
    matches!(
        ch,
        '$' | '@'
            | '\''
            | '"'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | ';'
            | '|'
            | '&'
            | '>'
            | '<'
            | ','
    )
}

/// Forms PowerShell converts on its own (`-Name:value`, odd numbers) leave the line unread.
fn plain_bare_word(word: &str) -> Result<(), String> {
    if word.starts_with('-') && !word.starts_with("--") && word.contains(':') {
        return Err("attached parameter value".into());
    }
    if word.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && !(word == "0" || (!word.starts_with('0') && word.chars().all(|ch| ch.is_ascii_digit())))
    {
        return Err("number PowerShell would convert".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "powershell_tests.rs"]
mod tests;
