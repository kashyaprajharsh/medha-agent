//! One reading of a shell command line, shared by admission, backups and lanes.

mod parse;
mod program;
mod readers;
mod word;

pub use program::Program;

/// Reads `line` with a bash grammar.
pub fn read(line: &str) -> Reading {
    parse::read(line)
}

/// What "always allow" for this line covers: each program and its first plain word, such as
/// `cargo test`. `None` when a variable, an interpreter, or a path outside the folder is involved,
/// so a broad approval never reaches what the person did not see.
pub fn approval_shape(line: &str) -> Option<String> {
    let reading = read(line);
    if !reading.complete || reading.self_forking {
        return None;
    }
    let mut shapes: Vec<String> = Vec::new();
    for command in &reading.commands {
        let words = command
            .words
            .iter()
            .chain(command.redirects.iter().map(|redirect| &redirect.target));
        if words
            .into_iter()
            .any(|word| word.value == Value::Dynamic || leaves(&word.text))
        {
            return None;
        }
        let Some(program) = command.program() else {
            continue;
        };
        if is_interpreter(&program.name) || program.at > 0 {
            return None;
        }
        let plain = program.args.first().filter(|arg| {
            arg.text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
                && !arg.text.starts_with('-')
        });
        let shape = match plain {
            Some(arg) => format!("{} {}", program.name, arg.text),
            None => program.name.clone(),
        };
        if !shapes.contains(&shape) {
            shapes.push(shape);
        }
    }
    (!shapes.is_empty()).then(|| shapes.join("; "))
}

/// A path that leaves the folder the line runs in: absolute, home-based, or through `..`.
fn leaves(text: &str) -> bool {
    text.starts_with(['/', '\\', '~'])
        || text.split(['/', '\\']).any(|part| part == "..")
        || text.as_bytes().get(1) == Some(&b':')
}

/// Shells and language runtimes: what they run is in their script, not in the line.
pub fn is_interpreter(name: &str) -> bool {
    matches!(
        name,
        "sh" | "dash"
            | "bash"
            | "zsh"
            | "fish"
            | "ksh"
            | "python"
            | "python2"
            | "python3"
            | "perl"
            | "ruby"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "php"
            | "lua"
            | "tclsh"
            | "powershell"
            | "pwsh"
            | "cmd"
            | "wscript"
            | "cscript"
            | "mshta"
            | "osascript"
    )
}

/// Whether `program` started directly with `args`, with no shell between, only reads.
pub fn program_reads_only(program: &str, args: &[String]) -> bool {
    let args: Vec<Word> = args
        .iter()
        .map(|arg| Word::new(arg, Value::Literal))
        .collect();
    readers::reads_only(&program::basename(program), &args)
}

/// Every command a line runs, including those inside substitutions.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub commands: Vec<Command>,
    /// The grammar parsed with no error, so no command is missing from `commands`.
    pub complete: bool,
    /// A function pipes or backgrounds a call to itself: a fork bomb.
    pub self_forking: bool,
}

impl Reading {
    /// True when nothing in the line writes a file, runs a program that might, or outlives the line.
    pub fn reads_only(&self) -> bool {
        self.complete
            && !self.self_forking
            && !self.commands.is_empty()
            && self
                .commands
                .iter()
                .all(|command| !command.background && command.reads_only())
    }
}

/// One simple command as the shell will run it.
#[derive(Clone, Debug, Default)]
pub struct Command {
    /// The program and its arguments; prefix assignments are in `assigns`.
    pub words: Vec<Word>,
    /// Names of `NAME=value` prefixes set only for this command.
    pub assigns: Vec<String>,
    pub redirects: Vec<Redirect>,
    pub piped_in: bool,
    pub background: bool,
    /// Runs inside `$(...)`, backquotes or `<(...)`.
    pub substituted: bool,
}

impl Command {
    /// True when the command neither writes a file nor runs a program that might.
    pub fn reads_only(&self) -> bool {
        // An environment variable can turn a reader into a launcher, as `LESSOPEN` or `GIT_EXTERNAL_DIFF` do.
        if self.redirects.iter().any(Redirect::writes) || !self.assigns.is_empty() {
            return false;
        }
        let Some(program) = self.program() else {
            return self.words.is_empty();
        };
        // A path runs whatever file is there, whatever its name.
        let by_name = !self.words[program.at].text.contains(['/', '\\']);
        let set_env = self.words[..program.at]
            .iter()
            .any(|word| word.text.contains('='));
        by_name && !set_env && !program.fed && readers::reads_only(&program.name, program.args)
    }
}

/// How much of a word the text decides before the shell runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// Fully written out.
    Literal,
    /// Written out, but a glob or brace picks among existing names.
    Pattern,
    /// Holds a status number such as `$?`, never a name the line chose.
    Status,
    /// A variable, substitution or argument list decides it.
    Dynamic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// The shell's value for written parts; expansions keep their source text.
    pub text: String,
    pub value: Value,
}

impl Word {
    pub(crate) fn new(text: &str, value: Value) -> Self {
        Self {
            text: text.to_owned(),
            value,
        }
    }

    pub(crate) fn join(&self, other: &Word) -> Word {
        Word {
            text: format!("{}{}", self.text, other.text),
            value: self.value.max(other.value),
        }
    }

    /// The text when nothing is left for the shell to decide.
    pub fn literal_text(&self) -> Option<&str> {
        (self.value == Value::Literal).then_some(self.text.as_str())
    }
}

/// Which way a redirect moves data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Read,
    Write,
    Append,
    ReadWrite,
    /// Copies or closes a descriptor, such as `2>&1`; no file is opened.
    Duplicate,
    /// A heredoc or herestring: the target is data, not a file.
    Here,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub fd: Option<u32>,
    pub flow: Flow,
    pub target: Word,
}

impl Redirect {
    /// Opens a file for writing, other than a terminal or null device.
    pub fn writes(&self) -> bool {
        matches!(self.flow, Flow::Write | Flow::Append | Flow::ReadWrite)
            && !is_device(&self.target.text)
    }
}

/// Devices a write cannot damage.
fn is_device(path: &str) -> bool {
    matches!(
        path,
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty"
    ) || path.starts_with("/dev/fd/")
        || path.eq_ignore_ascii_case("nul")
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
