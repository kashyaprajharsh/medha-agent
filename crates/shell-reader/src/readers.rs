//! Programs that only read. Missing one is harmless: an unlisted program is treated as one that may write.

use crate::Word;

pub(crate) fn reads_only(program: &str, args: &[Word]) -> bool {
    let has = |test: &dyn Fn(&str) -> bool| args.iter().any(|arg| test(&arg.text));
    // Long options may be abbreviated to any unique prefix.
    let long =
        |prefixes: &[&str]| has(&|arg| prefixes.iter().any(|prefix| arg.starts_with(prefix)));
    // A cluster such as `-uo` carries the same flag as `-o`.
    let short = |flags: &str| {
        has(&|arg| {
            arg.strip_prefix('-').is_some_and(|rest| {
                !rest.starts_with('-') && rest.contains(|flag| flags.contains(flag))
            })
        })
    };
    match program {
        "ls" | "pwd" | "echo" | "printf" | "cat" | "head" | "tail" | "less" | "more" | "wc"
        | "cut" | "tr" | "grep" | "egrep" | "fgrep" | "diff" | "cmp" | "comm" | "stat" | "du"
        | "df" | "which" | "whereis" | "type" | "true" | "false" | "sleep" | "readlink"
        | "realpath" | "basename" | "dirname" | "test" | "[" | "[[" | ":" | "nl" | "od" | "xxd"
        | "hexdump" | "strings" | "jq" | "cd" | "pushd" | "popd" | "exit" | "return" | "wait"
        | "read" => true,
        "sort" => !short("o") && !long(&["--o", "--co"]),
        "uniq" => args.iter().filter(|arg| !arg.text.starts_with('-')).count() < 2,
        // `-R` with `-H` writes an HTML page into every folder.
        "tree" => !short("oR"),
        "file" => !short("C") && !long(&["--c"]),
        "rg" => !short("z") && !long(&["--pre", "--hostname-bin", "--search-zip"]),
        "find" => !has(&|arg| {
            matches!(arg, "-delete" | "-fls")
                || ["-exec", "-ok", "-fprint"]
                    .iter()
                    .any(|run| arg.starts_with(run))
        }),
        "date" => !short("s") && !long(&["--set"]),
        "ffprobe" => !has(&|arg| arg == "-report"),
        "sed" => sed_reads(args),
        "git" => git_reads(args),
        _ => false,
    }
}

/// `git <reader>` with nothing before the subcommand and no option that writes or runs a program.
fn git_reads(args: &[Word]) -> bool {
    let reader = args.first().is_some_and(|sub| {
        matches!(
            sub.text.as_str(),
            "status"
                | "diff"
                | "log"
                | "show"
                | "blame"
                | "rev-parse"
                | "ls-files"
                | "ls-tree"
                | "cat-file"
                | "describe"
                | "shortlog"
                | "merge-base"
                | "rev-list"
                | "--version"
        )
    });
    reader
        && !args.iter().any(|arg| {
            ["--out", "--ext", "--textc", "--show-sig", "--open-files"]
                .iter()
                .any(|flag| arg.text.starts_with(flag))
        })
}

/// `sed` without in-place editing, a script file, or a script command that writes or runs.
fn sed_reads(args: &[Word]) -> bool {
    let mut scripts = Vec::new();
    let mut explicit = false;
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        let text = arg.text.as_str();
        if text.starts_with("--in-place") || text.starts_with("--file") || text == "-f" {
            return false;
        }
        if let Some(flags) = text
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-') && !flags.is_empty())
        {
            if flags.contains('i') || flags.contains('f') {
                return false;
            }
            if flags.ends_with('e') {
                explicit = true;
                scripts.extend(words.next());
            }
        } else if text == "--expression" {
            explicit = true;
            scripts.extend(words.next());
        } else if !explicit && scripts.is_empty() {
            scripts.push(arg);
        }
    }
    scripts
        .iter()
        .all(|script| script.literal_text().is_some_and(script_reads))
}

/// No `e`, `w` or `W` command and no `s` command with an `e` or `w` flag.
fn script_reads(script: &str) -> bool {
    let mut rest = script;
    loop {
        rest = strip_address(
            rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ';' | '{' | '}')),
        );
        let Some(command) = rest.chars().next() else {
            return true;
        };
        rest = &rest[command.len_utf8()..];
        let tail = match command {
            'e' | 'w' | 'W' => return false,
            's' | 'y' => {
                let Some(flags) = substitute_flags(rest) else {
                    return false;
                };
                let end = flags.find(['\n', ';', '}']).unwrap_or(flags.len());
                if command == 's' && flags[..end].contains(['e', 'w']) {
                    return false;
                }
                &flags[end..]
            }
            // Text and file arguments run to the end of the line; labels end at `;` too.
            'a' | 'i' | 'c' | 'r' | 'R' => rest.find('\n').map_or("", |at| &rest[at..]),
            'b' | 't' | 'T' | ':' => rest.find(['\n', ';']).map_or("", |at| &rest[at..]),
            _ => rest,
        };
        rest = tail;
    }
}

/// The command after an address such as `1,40`, `$`, `/re/` or `/a/,/b/!`.
fn strip_address(command: &str) -> &str {
    let mut rest = command;
    loop {
        rest = rest.trim_start_matches(|c: char| {
            c.is_ascii_digit() || matches!(c, '$' | ',' | '!' | ' ' | '~' | '+')
        });
        match rest.strip_prefix('/') {
            Some(regex) => rest = skip_delimited(regex, '/').unwrap_or(""),
            None => return rest,
        }
    }
}

/// The flags of `s<d>pattern<d>replacement<d>flags`; `None` when it is malformed.
fn substitute_flags(rest: &str) -> Option<&str> {
    let delimiter = rest.chars().next()?;
    let rest = skip_delimited(&rest[delimiter.len_utf8()..], delimiter)?;
    skip_delimited(rest, delimiter)
}

/// The text after the next unescaped `delimiter`.
fn skip_delimited(text: &str, delimiter: char) -> Option<&str> {
    let mut escaped = false;
    for (at, ch) in text.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            _ if ch == delimiter => return Some(&text[at + ch.len_utf8()..]),
            _ => {}
        }
    }
    None
}
