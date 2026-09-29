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
        return Some(format!(
            "{} holds browser sessions, keychains or passwords. Medha never opens it: do not \
             retry, request it, or run the command outside the sandbox. Tell the user if the \
             task needs it.",
            root.display()
        ));
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

/// Seatbelt refuses with EPERM, Landlock with EACCES; macOS EACCES is the file's own mode.
fn sandbox_refused(output: &str) -> bool {
    output.contains("Operation not permitted")
        || output.contains("blocked by sandbox")
        || (!cfg!(target_os = "macos") && output.contains("Permission denied"))
}

#[cfg(test)]
#[path = "shell_hints_tests.rs"]
mod tests;
