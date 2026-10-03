//! The one next step a shell result names after the sandbox stopped it.

use std::path::PathBuf;

/// Headless browsers outlive their screenshot, so a shell command waits out its deadline.
pub(crate) const BROWSER: &str = "A browser started from the shell never exits on its own and \
     cannot run inside Medha's sandbox. To see a page, call read with render: true and the \
     page's path or URL instead. Do not rerun this command.";

pub(crate) fn launches_browser(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    command.contains("--headless")
        || [
            "google chrome",
            "google-chrome",
            "chromium",
            "chrome.exe",
            "msedge",
            "microsoft edge",
            "brave browser",
            "headless_shell",
        ]
        .iter()
        .any(|name| command.contains(name))
}

/// None unless the output shows the sandbox itself refusing.
pub(crate) fn sandbox_blocked(
    command: &str,
    output: &str,
    denied_paths: &[PathBuf],
) -> Option<String> {
    if launches_browser(command) {
        return Some(BROWSER.into());
    }
    if let Some(root) = protected_in(output) {
        let credential = sandbox::CREDENTIAL_RELATIVE
            .iter()
            .any(|relative| root.ends_with(relative));
        return Some(if credential {
            format!(
                "{} holds the user's keys or tokens, which the sandbox never opens and no grant \
                 adds. If the task needs a command that signs in with them, such as ssh or git \
                 push, rerun it unchanged once with outside_sandbox: true. The user reviews that run.",
                root.display()
            )
        } else {
            format!(
                "{} holds browser sessions, keychains or passwords. Medha never opens it: do not \
                 retry, request it, or run the command outside the sandbox. Tell the user if the \
                 task needs it.",
                root.display()
            )
        });
    }
    if local_socket_refused(output) {
        return Some(
            "Medha's sandbox closes local sockets such as a container daemon's, and no grant \
             opens them. If the task needs this command, rerun it unchanged once with \
             outside_sandbox: true. The user reviews that run."
                .into(),
        );
    }
    // The shared temp root itself: a grant would open every app's files. Folders inside it are ordinary.
    let shared_temp = ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"];
    if denied_paths.iter().any(|path| {
        shared_temp
            .iter()
            .any(|temp| path == std::path::Path::new(temp))
    }) {
        return Some(
            "The shared temp directory is closed in Medha's sandbox; do not request it. Put \
             throwaway files in the scratch folder named in your Environment, or use mktemp \
             or $TMPDIR inside the command, then rerun."
                .into(),
        );
    }
    if !denied_paths.is_empty() {
        return Some(
            "Medha's sandbox blocked the directories in denied_paths. Do not try variations. \
             Rerun this command unchanged once with those directories in write_paths \
             (read_paths if it only reads). The user reviews that run."
                .into(),
        );
    }
    sandbox_refused(output).then(|| {
        "Medha's sandbox blocked this command and no narrower grant applies. Do not try \
         variations or workarounds; they hit the same sandbox. If the task needs this \
         command, rerun it unchanged once with outside_sandbox: true. The user reviews that run."
            .into()
    })
}

/// Matched on whole roots, since protected paths contain spaces.
fn protected_in(output: &str) -> Option<PathBuf> {
    let output = output.to_lowercase();
    sandbox::protected_paths()
        .into_iter()
        .find(|root| output.contains(&root.to_string_lossy().to_lowercase()))
}

/// A daemon client names its Unix socket and fails the same way however the network is set.
pub(crate) fn local_socket_refused(output: &str) -> bool {
    output.lines().map(str::to_ascii_lowercase).any(|line| {
        line.contains("unix")
            && (line.contains("permission denied") || line.contains("operation not permitted"))
    })
}

/// Seatbelt refuses with EPERM, Landlock with EACCES; macOS EACCES is the file's own mode.
fn sandbox_refused(output: &str) -> bool {
    output.contains("Operation not permitted")
        || output.contains("blocked by sandbox")
        || (!cfg!(target_os = "macos") && output.contains("Permission denied"))
}

#[cfg(test)]
#[path = "shell_hints_tests.rs"]
mod tests;
