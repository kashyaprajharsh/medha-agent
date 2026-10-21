//! Walks the bash grammar tree into the commands a line runs.

use crate::word::{named, text, word};
use crate::{Command, Flow, Reading, Redirect, Value, Word};
use tree_sitter::{Node, Parser};

#[derive(Clone, Copy, Default)]
struct Context {
    piped_in: bool,
    background: bool,
    substituted: bool,
}

struct Walk<'a> {
    src: &'a str,
    commands: Vec<Command>,
    complete: bool,
    self_forking: bool,
}

pub(crate) fn read(line: &str) -> Reading {
    let mut parser = Parser::new();
    let tree = parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()
        .and_then(|()| parser.parse(line, None));
    let Some(tree) = tree else {
        return Reading::default();
    };
    let root = tree.root_node();
    let mut walk = Walk {
        src: line,
        commands: Vec::new(),
        complete: !root.has_error(),
        self_forking: false,
    };
    walk.block(root, Context::default());
    Reading {
        commands: walk.commands,
        complete: walk.complete,
        self_forking: walk.self_forking,
    }
}

/// Nodes that are values, not statements; only substitutions inside them run.
fn is_value(kind: &str) -> bool {
    matches!(
        kind,
        "word"
            | "string"
            | "raw_string"
            | "ansi_c_string"
            | "translated_string"
            | "concatenation"
            | "number"
            | "simple_expansion"
            | "expansion"
            | "arithmetic_expansion"
            | "brace_expression"
            | "extglob_pattern"
            | "array"
            | "regex"
            | "variable_name"
            | "test_operator"
            | "command_substitution"
            | "process_substitution"
    )
}

impl Walk<'_> {
    /// Statements in order; a statement followed by `&` runs in the background.
    fn block(&mut self, node: Node, ctx: Context) -> Option<usize> {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        let mut last = None;
        for (i, child) in children.iter().enumerate() {
            if !child.is_named() {
                continue;
            }
            if is_value(child.kind()) {
                self.substitutions(*child);
                continue;
            }
            let background = children.get(i + 1).is_some_and(|next| next.kind() == "&");
            last = self.statement(
                *child,
                Context {
                    background: ctx.background || background,
                    ..ctx
                },
            );
        }
        last
    }

    /// Walks one statement; returns the index of the command a trailing redirect belongs to.
    fn statement(&mut self, node: Node, ctx: Context) -> Option<usize> {
        match node.kind() {
            "command" | "declaration_command" | "unset_command" | "test_command" => {
                Some(self.command(node, ctx))
            }
            "variable_assignment" | "variable_assignments" => {
                self.substitutions(node);
                None
            }
            "pipeline" => self.pipeline(node, ctx),
            "redirected_statement" => self.redirected(node, ctx),
            "function_definition" => self.function(node, ctx),
            "comment" | "heredoc_body" => None,
            "ERROR" => {
                self.complete = false;
                self.block(node, ctx)
            }
            "program"
            | "list"
            | "subshell"
            | "compound_statement"
            | "negated_command"
            | "if_statement"
            | "elif_clause"
            | "else_clause"
            | "while_statement"
            | "for_statement"
            | "c_style_for_statement"
            | "case_statement"
            | "case_item"
            | "do_group" => self.block(node, ctx),
            _ => {
                self.complete = false;
                self.substitutions(node);
                None
            }
        }
    }

    fn command(&mut self, node: Node, ctx: Context) -> usize {
        let mut command = Command {
            piped_in: ctx.piped_in,
            background: ctx.background,
            substituted: ctx.substituted,
            ..Command::default()
        };
        let at = self.commands.len();
        self.commands.push(Command::default());
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "variable_assignment" => {
                    command.assigns.extend(
                        child
                            .child_by_field_name("name")
                            .map(|name| text(name, self.src).to_owned()),
                    );
                }
                "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                    command.redirects.extend(self.redirect(child, ctx));
                    continue;
                }
                "command_name" => command
                    .words
                    .extend(named(child).first().map(|name| word(*name, self.src))),
                kind if is_value(kind) => command.words.push(word(child, self.src)),
                // `export`, `unset`, `[[` and similar keywords name the command themselves.
                kind if !child.is_named()
                    && command.words.is_empty()
                    && kind.chars().all(|c| c.is_ascii_alphabetic() || c == '[') =>
                {
                    command.words.push(Word::new(kind, Value::Literal));
                }
                _ => {}
            }
            self.substitutions(child);
        }
        self.commands[at] = command;
        at
    }

    fn pipeline(&mut self, node: Node, ctx: Context) -> Option<usize> {
        let mut last = None;
        for (i, element) in named(node).into_iter().enumerate() {
            last = self.statement(
                element,
                Context {
                    piped_in: ctx.piped_in || i > 0,
                    ..ctx
                },
            );
        }
        last
    }

    /// Bash binds a redirect after `a && b` to `b`; after a group or loop it covers the whole statement.
    fn redirected(&mut self, node: Node, ctx: Context) -> Option<usize> {
        let mut redirects = Vec::new();
        let mut last = None;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                    redirects.extend(self.redirect(child, ctx));
                }
                _ if child.is_named() => last = self.statement(child, ctx),
                _ => {}
            }
        }
        let body = node.child_by_field_name("body").map(|body| body.kind());
        match last {
            Some(at) if matches!(body, Some("command" | "list" | "pipeline")) => {
                self.commands[at].redirects.extend(redirects);
                Some(at)
            }
            _ => {
                self.commands.push(Command {
                    redirects,
                    ..Command::default()
                });
                None
            }
        }
    }

    fn redirect(&mut self, node: Node, ctx: Context) -> Vec<Redirect> {
        let fd = node
            .child_by_field_name("descriptor")
            .and_then(|fd| text(fd, self.src).parse().ok());
        match node.kind() {
            "file_redirect" => {
                let mut cursor = node.walk();
                let operator = node
                    .children(&mut cursor)
                    .find(|child| !child.is_named())
                    .map_or("", |child| child.kind());
                let target = node.child_by_field_name("destination").map_or_else(
                    || Word::new("", Value::Literal),
                    |target| word(target, self.src),
                );
                self.substitutions(node);
                vec![Redirect {
                    fd,
                    flow: flow(operator, &target),
                    target,
                }]
            }
            "heredoc_redirect" => {
                // A pipeline or more redirects can follow the heredoc start on the same line.
                let mut redirects = vec![Redirect {
                    fd,
                    flow: Flow::Here,
                    target: Word::new("", Value::Literal),
                }];
                for child in named(node) {
                    match child.kind() {
                        "file_redirect" => redirects.extend(self.redirect(child, ctx)),
                        "heredoc_body" | "heredoc_start" | "heredoc_end" => {
                            self.substitutions(child)
                        }
                        _ => {
                            self.statement(
                                child,
                                Context {
                                    piped_in: true,
                                    ..ctx
                                },
                            );
                        }
                    }
                }
                redirects
            }
            _ => {
                self.substitutions(node);
                vec![Redirect {
                    fd,
                    flow: Flow::Here,
                    target: Word::new("", Value::Literal),
                }]
            }
        }
    }

    /// A function that pipes or backgrounds a call to itself multiplies without end.
    fn function(&mut self, node: Node, ctx: Context) -> Option<usize> {
        let name = node
            .child_by_field_name("name")
            .map(|name| text(name, self.src).to_owned());
        let start = self.commands.len();
        if let Some(body) = node.child_by_field_name("body") {
            self.statement(body, ctx);
        }
        self.self_forking |= self.commands[start..].iter().any(|command| {
            (command.piped_in || command.background)
                && command.words.first().map(|word| word.text.as_str()) == name.as_deref()
        });
        None
    }

    /// Commands inside `$(...)`, backquotes and `<(...)` run too.
    fn substitutions(&mut self, node: Node) {
        if matches!(node.kind(), "command_substitution" | "process_substitution") {
            self.block(
                node,
                Context {
                    substituted: true,
                    ..Context::default()
                },
            );
            return;
        }
        for child in named(node) {
            self.substitutions(child);
        }
    }
}

fn flow(operator: &str, target: &Word) -> Flow {
    let descriptor = target.text == "-" || target.text.chars().all(|c| c.is_ascii_digit());
    match operator {
        "<" => Flow::Read,
        ">>" | "&>>" => Flow::Append,
        "<>" => Flow::ReadWrite,
        "<&" | "<&-" | ">&-" => Flow::Duplicate,
        ">&" if descriptor => Flow::Duplicate,
        _ => Flow::Write,
    }
}
