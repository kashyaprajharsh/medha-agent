//! Commands that delete or throw away something a person may want back: files, git work, database data,
//! containers and cloud resources, disks and backups. Each is the person's alone to allow, in every mode.

use regex::Regex;
use shell_reader::{Command, Reading, Value, Word};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What the person is told on the card, by kind of loss.
pub(crate) const FILES: &str =
    "This deletes files or folders. Allow it only if you asked Medha to delete them.";
pub(crate) const GIT: &str = "This throws away work in git: uncommitted changes, branches or shared history. \
     Allow it only if you asked for it.";
pub(crate) const DATABASE: &str =
    "This deletes data in a database. Allow it only if you asked for it.";
pub(crate) const RESOURCES: &str = "This deletes containers, volumes or cloud resources, and the data in them. \
     Allow it only if you asked for it.";
pub(crate) const SYSTEM: &str =
    "This erases a disk, backups or system settings. Allow it only if you asked for it.";

/// The loss a command line may cause, if any, from its readings and its text.
pub(crate) fn loss(readings: &[&Reading], line: &str) -> Option<&'static str> {
    readings
        .iter()
        .flat_map(|reading| reading.commands.iter())
        .filter_map(|command| command.program())
        .find_map(|program| {
            let (name, args) = unwrap_elevation(&program.name, program.args);
            by_program(&name, args)
        })
        .or_else(|| by_text(line))
}

/// The workspace and this chat's scratch folder, as the session named them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Folders {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) scratch: Option<PathBuf>,
}

impl Folders {
    /// The line, as its shell reads it, only deletes things already in this chat's scratch folder, links followed.
    pub(crate) fn only_clears_scratch(&self, reading: &Reading, workdir: Option<&str>) -> bool {
        let Some(scratch) = self
            .scratch
            .as_deref()
            .and_then(|dir| dir.canonicalize().ok())
        else {
            return false;
        };
        // The shell tool runs in the workspace, or in `workdir` joined to it.
        let cwd = match (&self.workspace, workdir) {
            (Some(root), Some(dir)) => root.join(dir),
            (None, Some(dir)) => PathBuf::from(dir),
            (Some(root), None) => root.clone(),
            (None, None) => return false,
        };
        reading.complete
            && !reading.commands.is_empty()
            && reading
                .commands
                .iter()
                .all(|command| clears(command, &scratch, &cwd))
    }
}

/// A plain file delete whose every target is inside `scratch`.
fn clears(command: &Command, scratch: &Path, cwd: &Path) -> bool {
    let quiet = |target: &str| {
        matches!(
            target.to_ascii_lowercase().as_str(),
            "/dev/null" | "$null" | "nul"
        )
    };
    if command.background
        || !command.assigns.is_empty()
        || command
            .redirects
            .iter()
            .any(|redirect| !quiet(&redirect.target.text))
    {
        return false;
    }
    let Some(program) = command.program() else {
        return false;
    };
    if program.at > 0 || program.fed {
        return false;
    }
    let switch: fn(&str) -> bool = match program.name.as_str() {
        "rm" | "unlink" | "rmdir" | "remove-item" | "ri" => |word| word.starts_with('-'),
        "del" | "erase" | "rd" => cmd_switch,
        _ => return false,
    };
    let mut targets = Vec::new();
    let (mut after, mut skip) = (false, false);
    for word in program.args {
        let lower = word.text.to_ascii_lowercase();
        if skip {
            skip = false;
        } else if !after && word.text == "--" {
            after = true;
        } else if !after && switch(&word.text) {
            // These PowerShell parameters take a value that is not a path.
            skip = matches!(
                lower.as_str(),
                "-erroraction" | "-ea" | "-warningaction" | "-wa" | "-informationaction" | "-infa"
            );
        } else {
            targets.push(word);
        }
    }
    !targets.is_empty() && targets.iter().all(|word| inside(word, scratch, cwd))
}

/// A cmd switch such as `/s`, `/q` or `/a:h`.
fn cmd_switch(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('/')
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && matches!(chars.next(), None | Some(':'))
}

/// Whether a delete target is something that exists inside `scratch`, after links; never the folder itself.
fn inside(word: &Word, scratch: &Path, cwd: &Path) -> bool {
    let text = word.text.as_str();
    if text.is_empty() || text.starts_with('~') || text.split(['/', '\\']).any(|part| part == "..")
    {
        return false;
    }
    let path = cwd.join(text);
    let real = match word.value {
        Value::Literal => path.canonicalize().ok().filter(|real| real != scratch),
        // A glob only in the last name, which `rm` removes without following a link.
        Value::Pattern => {
            let parent = path.parent().filter(|parent| {
                !parent.to_string_lossy().contains(['*', '?', '['])
                    && !text.contains('{')
                    && !text.ends_with(['/', '\\'])
            });
            parent.and_then(|parent| parent.canonicalize().ok())
        }
        Value::Status | Value::Dynamic => None,
    };
    real.is_some_and(|real| real.starts_with(scratch))
}

/// `sudo rm …`, `doas rm …` and `npx prisma …` lose what the program they run does.
fn unwrap_elevation<'a>(name: &str, args: &'a [Word]) -> (String, &'a [Word]) {
    let runs = match name {
        "sudo" | "doas" | "npx" | "bunx" | "pnpx" => true,
        "pnpm" | "yarn" => matches!(
            args.first().map(|arg| arg.text.as_str()),
            Some("dlx" | "exec")
        ),
        _ => false,
    };
    if !runs {
        return (name.to_owned(), args);
    }
    let args = match name {
        "pnpm" | "yarn" => &args[1..],
        _ => args,
    };
    match args.iter().position(|arg| !arg.text.starts_with('-')) {
        Some(at) => (
            args[at]
                .text
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase(),
            &args[at + 1..],
        ),
        None => (name.to_owned(), args),
    }
}

fn by_program(name: &str, args: &[Word]) -> Option<&'static str> {
    let words: Vec<String> = args
        .iter()
        .map(|arg| arg.text.to_ascii_lowercase())
        .collect();
    let has = |test: &dyn Fn(&str) -> bool| words.iter().any(|word| test(word));
    let first = words
        .iter()
        .find(|word| !word.starts_with('-'))
        .map(String::as_str);
    match name {
        // Files and folders, in every shell.
        "rm" | "unlink" | "shred" | "srm" | "trash" | "trash-put" | "rmtrash" | "rmdir"
        | "remove-item" | "ri" | "del" | "erase" | "rd" | "clear-content" | "clc"
        | "clear-recyclebin" => Some(FILES),
        "gio" if matches!(first, Some("trash" | "remove")) => Some(FILES),
        "find" if has(&|word| word == "-delete") || execs_a_delete(&words) => Some(FILES),
        "git" => git_loss(&words).then_some(GIT),
        // Databases.
        "dropdb" | "dropuser" => Some(DATABASE),
        "mysqladmin" if has(&|word| word == "drop") => Some(DATABASE),
        "redis-cli" if has(&|word| matches!(word, "flushall" | "flushdb")) => Some(DATABASE),
        "rails" | "rake" | "bin/rails"
            if has(&|word| {
                matches!(word, "db:drop" | "db:reset" | "db:schema:load" | "db:purge")
            }) =>
        {
            Some(DATABASE)
        }
        "python" | "python3"
            if has(&|word| word == "flush") && has(&|w| w.ends_with("manage.py")) =>
        {
            Some(DATABASE)
        }
        "prisma" if has(&|word| word == "reset" || word == "--force-reset") => Some(DATABASE),
        "supabase" if has(&|word| word == "reset") => Some(DATABASE),
        // Containers and cloud.
        "docker" | "podman" if container_loss(&words) => Some(RESOURCES),
        "docker-compose" if compose_down_volumes(&words) => Some(RESOURCES),
        "kubectl" | "oc" if first == Some("delete") => Some(RESOURCES),
        "helm" if matches!(first, Some("uninstall" | "delete")) => Some(RESOURCES),
        "terraform" | "tofu"
            if first == Some("destroy")
                || (first == Some("apply") && has(&|w| w == "-destroy")) =>
        {
            Some(RESOURCES)
        }
        "aws" if has(&|word| matches!(word, "rm" | "rb") || word.starts_with("delete")) => {
            Some(RESOURCES)
        }
        "gsutil" if matches!(first, Some("rm" | "rb")) => Some(RESOURCES),
        "az" | "gcloud" if has(&|word| word == "delete") => Some(RESOURCES),
        // Disks, backups and system settings.
        "dd" if has(&|word| word.starts_with("of=/dev/")) => Some(SYSTEM),
        "diskutil" if has(&|word| word.starts_with("erase") || word == "partitiondisk") => {
            Some(SYSTEM)
        }
        "format" | "diskpart" | "format-volume" | "clear-disk" | "remove-itemproperty" | "rp" => {
            Some(SYSTEM)
        }
        "cipher" if has(&|word| word.starts_with("/w")) => Some(SYSTEM),
        "vssadmin" | "wbadmin" if has(&|word| word == "delete") => Some(SYSTEM),
        "reg" if first == Some("delete") => Some(SYSTEM),
        "tmutil" if has(&|word| word == "delete") => Some(SYSTEM),
        _ if name.starts_with("mkfs") => Some(SYSTEM),
        _ => None,
    }
}

/// `find … -exec rm …` deletes what it finds.
fn execs_a_delete(words: &[String]) -> bool {
    words.windows(2).any(|pair| {
        matches!(pair[0].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
            && matches!(
                pair[1].rsplit('/').next(),
                Some("rm" | "unlink" | "shred" | "rmdir")
            )
    })
}

/// Git commands that throw work away: uncommitted changes, untracked files, branches, stashes or history.
fn git_loss(words: &[String]) -> bool {
    let mut rest = words.iter().map(String::as_str);
    let mut sub = None;
    while let Some(word) = rest.next() {
        // Lower-cased already, so `-c` here is both `-c` and `-C`; each takes a value.
        if matches!(word, "-c" | "--git-dir" | "--work-tree") {
            rest.next();
        } else if !word.starts_with('-') {
            sub = Some(word);
            break;
        }
    }
    let rest: Vec<&str> = rest.collect();
    let short = |flag: char| {
        rest.iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flag))
    };
    match sub {
        Some("clean") => !rest.iter().any(|arg| matches!(*arg, "-n" | "--dry-run")),
        Some("reset") => rest.contains(&"--hard"),
        Some("checkout") => rest.iter().any(|arg| matches!(*arg, "--" | ".")),
        Some("restore") => !rest.contains(&"--staged") || rest.contains(&"--worktree"),
        Some("push") => {
            short('f')
                || rest
                    .iter()
                    .any(|arg| arg.starts_with("--force") || arg.starts_with('+'))
                || rest.contains(&"--delete")
        }
        // Words are lower-cased, so `-D` reads as `-d`: any branch delete asks.
        Some("branch") => short('d') || rest.contains(&"--delete"),
        Some("stash") => matches!(rest.first(), Some(&"drop" | &"clear")),
        Some("filter-branch" | "filter-repo") => true,
        Some("reflog") => rest.contains(&"expire") || rest.contains(&"delete"),
        _ => false,
    }
}

/// `docker rm`, `rmi`, `volume rm`, any `prune`, and `compose down -v`.
fn container_loss(words: &[String]) -> bool {
    let positional: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|word| !word.starts_with('-'))
        .collect();
    match positional.as_slice() {
        ["rm" | "rmi", ..] | [_, "rm" | "prune", ..] => true,
        ["compose", ..] => compose_down_volumes(words),
        _ => false,
    }
}

fn compose_down_volumes(words: &[String]) -> bool {
    words.iter().any(|word| word == "down")
        && words
            .iter()
            .any(|word| matches!(word.as_str(), "-v" | "--volumes"))
}

/// Database statements and calls written inside a line's quotes, which no command reading can see.
static DATABASE_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:drop\s+(?:table|database|schema|index|view|collection)|truncate\s+(?:table\s+)?\w|delete\s+from\b|alter\s+table\s+\S+\s+drop\b|dropdatabase\s*\(|\.drop\s*\(|deletemany\s*\(|\.remove\s*\(\s*\{|flushall\b|flushdb\b)",
    )
    .expect("database statements")
});

fn by_text(line: &str) -> Option<&'static str> {
    DATABASE_TEXT.is_match(line).then_some(DATABASE)
}

#[cfg(test)]
#[path = "destructive_tests.rs"]
mod tests;
