//! What a shell line may do before it runs: a few real dangers are refused in every
//! mode, and without an OS jail anything the text cannot show goes to a person.

use crate::{argument_escapes_workspace, home_tail, is_system_path, normalize_rm_path};
use shell_reader::{Command, Flow, Reading, Value, Word, is_interpreter as interpreter};

/// Credential stores matched in path-like words only, so a search pattern naming one is not a read.
const CREDENTIALS: &[&str] = &[
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    ".aws/credentials",
    ".git-credentials",
    "/.netrc",
    ".docker/config.json",
    ".kube/config",
    "/etc/shadow",
];

/// Risk tier of a recursive delete target.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DeleteTier {
    /// A path the text does not show; the jail decides where it may land.
    Unseen,
    /// A user path outside the workspace, such as `~/Documents/x`.
    Outside,
    /// The filesystem root, a home root, or a system folder.
    System,
}

/// Refused in every mode: no jail or approval makes these safe to run.
pub(crate) fn danger(reading: &Reading, workspace: Option<&str>, jailed: bool) -> Option<String> {
    if reading.self_forking {
        return Some("a function that starts copies of itself without end (fork bomb)".into());
    }
    for command in &reading.commands {
        if let Some(why) = command_danger(command, jailed) {
            return Some(why.into());
        }
    }
    (delete_tier(reading, workspace) == Some(DeleteTier::System)).then(|| {
        "recursive delete of the filesystem root, a home folder, or a system folder".into()
    })
}

fn command_danger(command: &Command, jailed: bool) -> Option<&'static str> {
    if command
        .redirects
        .iter()
        .any(|redirect| redirect.writes() && raw_device(&redirect.target.text))
    {
        return Some("writes straight to a disk device");
    }
    let paths = command
        .words
        .iter()
        .chain(command.redirects.iter().map(|redirect| &redirect.target));
    if paths.filter(|word| path_like(&word.text)).any(|word| {
        let lower = word.text.to_lowercase();
        CREDENTIALS.iter().any(|store| lower.contains(store))
    }) {
        return Some("reads a credential store");
    }
    let program = command.program()?;
    let name = program.name.as_str();
    let args = program.args;
    if matches!(name, "sudo" | "doas") {
        return Some("privilege escalation");
    }
    if name.starts_with("mkfs") {
        return Some("formats a filesystem");
    }
    if name == "dd"
        && args
            .iter()
            .any(|arg| arg.text.strip_prefix("of=").is_some_and(raw_device))
    {
        return Some("writes straight to a disk device");
    }
    if matches!(name, "powershell" | "pwsh")
        && args.iter().any(|arg| {
            matches!(
                arg.text.to_lowercase().as_str(),
                "-enc" | "/enc" | "-encodedcommand" | "/encodedcommand"
            )
        })
    {
        return Some("runs an encoded PowerShell payload");
    }
    if matches!(name, "chmod" | "chown" | "chgrp")
        && recursive(args)
        && operands(args).any(|arg| path_tier(arg, None, jailed) == Some(DeleteTier::System))
    {
        return Some("changes ownership or permissions across a system folder");
    }
    // Inside the jail a piped interpreter is contained, and fetching what it runs needs network approval.
    if !jailed && command.piped_in && (interpreter(name) || name == "eval") {
        return Some("pipes data into a shell or interpreter");
    }
    None
}

/// The riskiest recursive `rm` target in the line.
pub(crate) fn delete_tier(reading: &Reading, workspace: Option<&str>) -> Option<DeleteTier> {
    reading
        .commands
        .iter()
        .filter_map(Command::program)
        .filter(|program| program.name == "rm" && recursive(program.args))
        .flat_map(|program| {
            operands(program.args).filter_map(|arg| path_tier(arg, workspace, false))
        })
        .max()
}

/// Tier of one delete target; `None` when it stays in the workspace or a temp folder.
fn path_tier(arg: &Word, workspace: Option<&str>, jailed: bool) -> Option<DeleteTier> {
    let lower = arg.text.to_lowercase();
    let raw = if lower == "/" {
        "/"
    } else {
        lower.trim_end_matches('/')
    };
    let owned;
    let expanded = match arg.value {
        Value::Literal | Value::Pattern => raw,
        _ => match home_tail(raw) {
            Some(tail) => {
                owned = format!("~{tail}");
                &owned
            }
            None if jailed => return None,
            None => return Some(DeleteTier::Unseen),
        },
    };
    let rooted = expanded.starts_with('/') || expanded.starts_with('~');
    // The kernel resolves `..` through links, so the text cannot show where it lands.
    if expanded.split('/').any(|part| part == "..") {
        return Some(if rooted {
            DeleteTier::System
        } else {
            DeleteTier::Unseen
        })
        .filter(|tier| *tier == DeleteTier::System || !jailed);
    }
    let path = normalize_rm_path(expanded);
    if !rooted {
        return None;
    }
    let temp = [
        "/tmp",
        "/private/tmp",
        "/var/folders",
        "/private/var/folders",
    ];
    if temp
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}/")))
    {
        return None;
    }
    if workspace.is_some_and(|root| path == root || path.starts_with(&format!("{root}/"))) {
        return None;
    }
    Some(if is_system_path(&path) {
        DeleteTier::System
    } else {
        DeleteTier::Outside
    })
}

/// Without a jail nothing contains the command, so anything the text cannot show is reviewed.
pub(crate) fn review(
    reading: &Reading,
    workspace: Option<&str>,
    scratch: Option<&str>,
) -> Option<&'static str> {
    if !reading.complete {
        return Some("contains shell syntax Medha could not read");
    }
    reading
        .commands
        .iter()
        .find_map(|command| review_command(command, workspace, scratch))
}

fn review_command(
    command: &Command,
    workspace: Option<&str>,
    scratch: Option<&str>,
) -> Option<&'static str> {
    if command.background {
        return Some("runs a command in the background");
    }
    for redirect in &command.redirects {
        let target = redirect.target.text.to_lowercase();
        if target.starts_with("/dev/tcp/") || target.starts_with("/dev/udp/") {
            return Some("opens a raw network socket");
        }
        if matches!(
            redirect.flow,
            Flow::Read | Flow::Write | Flow::Append | Flow::ReadWrite
        ) && (redirect.writes() || redirect.flow == Flow::Read)
        {
            return Some("reads or writes a file through shell redirection");
        }
    }
    if !command.assigns.is_empty() {
        return Some("sets a shell environment variable");
    }
    let Some(program) = command.program() else {
        return (!command.words.is_empty()).then_some("has no statically known program");
    };
    if command
        .words
        .iter()
        .any(|word| matches!(word.value, Value::Dynamic | Value::Pattern))
    {
        return Some("uses shell expansion or globbing");
    }
    if program.at > 0 || program.fed {
        return Some("runs its program through a wrapper");
    }
    let name = program.name.as_str();
    if interpreter(name) {
        return Some("invokes a shell or interpreter");
    }
    if command.words[0].text.contains(['/', '\\']) || command.words[0].text.starts_with('~') {
        return Some("runs a file by path");
    }
    if network(name) {
        return Some("uses network egress or file transfer");
    }
    if dumps_environment(name, program.args) {
        return Some("prints the environment");
    }
    if matches!(
        name,
        "eval" | "source" | "." | "parallel" | "chmod" | "chown" | "install"
    ) {
        return Some("uses a dynamic or consequential command");
    }
    let outside = |arg: &Word| {
        let text = arg.text.to_lowercase();
        argument_escapes_workspace(&text, workspace)
            && scratch.is_none_or(|root| argument_escapes_workspace(&text, Some(root)))
    };
    if name != "rm" && program.args.iter().any(outside) {
        return Some("references a path outside the authorized workspace");
    }
    if !command.reads_only()
        && !builds(name, program.args)
        && !(name == "rm" && recursive(program.args))
    {
        return Some("invokes a command outside the statically approved shell subset");
    }
    None
}

/// Build and test entry points: they run workspace code, which the review accepts as the work itself.
fn builds(name: &str, args: &[Word]) -> bool {
    match name {
        "cargo" => args.first().is_some_and(|sub| {
            matches!(
                sub.text.as_str(),
                "build"
                    | "check"
                    | "test"
                    | "bench"
                    | "clippy"
                    | "fmt"
                    | "doc"
                    | "metadata"
                    | "tree"
                    | "--version"
                    | "-vV"
            )
        }),
        "rustc" | "rustfmt" | "go" | "make" | "cmake" | "ninja" | "pytest" => true,
        _ => false,
    }
}

fn dumps_environment(name: &str, args: &[Word]) -> bool {
    let first = args.first().map(|arg| arg.text.as_str());
    match name {
        "printenv" => true,
        "env" | "set" => args.is_empty(),
        "export" => first == Some("-p") || args.is_empty(),
        "declare" | "typeset" => matches!(first, Some("-x" | "-p")),
        _ => false,
    }
}

fn network(name: &str) -> bool {
    matches!(
        name,
        "curl"
            | "wget"
            | "http"
            | "https"
            | "scp"
            | "sftp"
            | "rsync"
            | "nc"
            | "ncat"
            | "netcat"
            | "telnet"
            | "ftp"
            | "ssh"
            | "invoke-webrequest"
            | "invoke-restmethod"
    )
}

/// A recursive flag before `--`; GNU tools accept it after operands too.
fn recursive(args: &[Word]) -> bool {
    args.iter().take_while(|arg| arg.text != "--").any(|arg| {
        arg.text == "--recursive"
            || arg
                .text
                .strip_prefix('-')
                .is_some_and(|flags| !flags.starts_with('-') && flags.contains(['r', 'R']))
    })
}

/// Operands: every word after `--`, and words that are not options before it.
fn operands(args: &[Word]) -> impl Iterator<Item = &Word> {
    let mut after = false;
    args.iter().filter(move |arg| {
        if !after && arg.text == "--" {
            after = true;
            return false;
        }
        after || !arg.text.starts_with('-')
    })
}

/// A disk or partition device; terminals, null and descriptor files are harmless.
fn raw_device(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.starts_with("/dev/")
        && !matches!(
            lower.as_str(),
            "/dev/null" | "/dev/zero" | "/dev/stdout" | "/dev/stderr" | "/dev/tty"
        )
        && !lower.starts_with("/dev/fd/")
        && !lower.starts_with("/dev/tcp/")
        && !lower.starts_with("/dev/udp/")
}

fn path_like(text: &str) -> bool {
    text.contains(['/', '\\']) || text.starts_with('~') || text.starts_with('.')
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
