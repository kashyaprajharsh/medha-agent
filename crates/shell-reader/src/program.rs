//! The program a command really runs once wrappers such as `env` or `timeout` are set aside.

use crate::{Command, Word};

pub struct Program<'a> {
    /// Index of the program word in `Command::words`.
    pub at: usize,
    /// Lowercased file name, without a directory or `.exe`.
    pub name: String,
    pub args: &'a [Word],
    /// `xargs` adds operands read from standard input, so `args` is not all of them.
    pub fed: bool,
}

impl Command {
    /// `None` when the program word is decided only when the shell runs.
    pub fn program(&self) -> Option<Program<'_>> {
        let mut at = 0;
        let mut fed = false;
        loop {
            let name = basename(self.words.get(at)?.literal_text()?);
            let rest = &self.words[at + 1..];
            let wrapped = match name.as_str() {
                "env" => after_options(rest, "uCSP", &["unset", "chdir", "split-string"], true),
                "command"
                    if !rest
                        .iter()
                        .any(|word| matches!(word.text.as_str(), "-v" | "-V")) =>
                {
                    after_options(rest, "", &[], false)
                }
                "builtin" | "nohup" => after_options(rest, "", &[], false),
                "exec" => after_options(rest, "a", &[], false),
                "nice" => after_options(rest, "n", &["adjustment"], false),
                "time" => after_options(rest, "fo", &["format", "output"], false),
                "stdbuf" => after_options(rest, "ioe", &["input", "output", "error"], false),
                // The duration comes first, then the program.
                "timeout" => {
                    after_options(rest, "sk", &["signal", "kill-after"], false).map(|at| at + 1)
                }
                "xargs" => {
                    fed = true;
                    after_options(
                        rest,
                        "IELnPsdae",
                        &[
                            "max-args",
                            "max-procs",
                            "delimiter",
                            "arg-file",
                            "eof",
                            "replace",
                            "max-lines",
                            "max-chars",
                        ],
                        false,
                    )
                }
                _ => None,
            };
            match wrapped.filter(|offset| *offset < rest.len()) {
                Some(offset) => at += 1 + offset,
                None => {
                    return Some(Program {
                        at,
                        name,
                        args: rest,
                        fed,
                    });
                }
            }
        }
    }
}

/// Offset of the first operand, skipping options and, for `env`, `NAME=value` pairs.
fn after_options(
    args: &[Word],
    short_values: &str,
    long_values: &[&str],
    assignments: bool,
) -> Option<usize> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.text.as_str();
        if text == "--" {
            return Some(i + 1);
        }
        if let Some(long) = text.strip_prefix("--") {
            i += if !long.contains('=') && long_values.contains(&long) {
                2
            } else {
                1
            };
        } else if text.len() > 1 && text.starts_with('-') {
            let flags = &text[1..];
            let value = flags.find(|flag| short_values.contains(flag));
            // A value-taking flag at the end of its cluster takes the next word.
            i += if value.is_some_and(|at| at + 1 == flags.len()) {
                2
            } else {
                1
            };
        } else if assignments && text.contains('=') && !text.starts_with('=') {
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

pub(crate) fn basename(word: &str) -> String {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    name.strip_suffix(".exe").map(str::to_owned).unwrap_or(name)
}
