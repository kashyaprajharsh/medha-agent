//! The value a shell word takes, as far as the text decides it.

use crate::{Value, Word};
use tree_sitter::Node;

/// One argument node as a word.
pub(crate) fn word(node: Node, src: &str) -> Word {
    let raw = text(node, src);
    match node.kind() {
        "concatenation" => named(node)
            .into_iter()
            .fold(Word::new("", Value::Literal), |word, part| {
                word.join(&self::word(part, src))
            }),
        "string" | "translated_string" => quoted(node, src),
        "word" | "number" if named(node).is_empty() => bare(raw),
        "raw_string" => Word::new(
            raw.trim_start_matches('\'').trim_end_matches('\''),
            Value::Literal,
        ),
        "ansi_c_string" => Word::new(&ansi_c(raw), Value::Literal),
        "simple_expansion" | "expansion" => expansion(node, src),
        "arithmetic_expansion" => Word::new(raw, Value::Status),
        "brace_expression" | "extglob_pattern" => Word::new(raw, Value::Pattern),
        _ => Word::new(raw, Value::Dynamic),
    }
}

/// An unquoted word: escapes resolve, while globs and braces stay patterns.
fn bare(raw: &str) -> Word {
    let mut value = String::new();
    let mut pattern = false;
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => value.extend(chars.next().filter(|next| *next != '\n')),
            '*' | '?' | '[' => {
                pattern = true;
                value.push(ch);
            }
            '{' if raw.contains(',') || raw.contains("..") => {
                pattern = true;
                value.push(ch);
            }
            _ => value.push(ch),
        }
    }
    Word::new(
        &value,
        if pattern {
            Value::Pattern
        } else {
            Value::Literal
        },
    )
}

/// A double-quoted string: only expansions inside it are left to the shell.
fn quoted(node: Node, src: &str) -> Word {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() != "\"")
        .map(|child| match child.kind() {
            "string_content" => Word::new(&unescape_quoted(text(child, src)), Value::Literal),
            _ if !child.is_named() => Word::new(text(child, src), Value::Literal),
            _ => word(child, src),
        })
        .fold(Word::new("", Value::Literal), |word, part| word.join(&part))
}

/// Status parameters such as `$?` and `${PIPESTATUS[0]}` are numbers; any other expansion is dynamic.
fn expansion(node: Node, src: &str) -> Word {
    let raw = text(node, src);
    let inner = named(node);
    let status = match inner.as_slice() {
        [only] if only.kind() == "special_variable_name" => {
            matches!(text(*only, src), "?" | "#" | "$" | "!" | "-")
        }
        [only] if only.kind() == "subscript" => raw.starts_with("${PIPESTATUS["),
        _ => raw.starts_with("${#"),
    };
    Word::new(
        raw,
        if status {
            Value::Status
        } else {
            Value::Dynamic
        },
    )
}

fn unescape_quoted(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match (ch, chars.peek()) {
            ('\\', Some('$' | '`' | '"' | '\\')) => out.extend(chars.next()),
            ('\\', Some('\n')) => {
                chars.next();
            }
            _ => out.push(ch),
        }
    }
    out
}

fn ansi_c(raw: &str) -> String {
    let body = raw.trim_start_matches("$'").trim_end_matches('\'');
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

pub(crate) fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub(crate) fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or_default()
}
