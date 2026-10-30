use super::*;

/// Messages captured from git, sudo, ssh and shells run without a terminal (macOS and Linux forms).
#[test]
fn a_prompt_with_no_terminal_is_recognised_and_ordinary_failures_are_not() {
    for asked in [
        "fatal: could not read Username for 'https://github.com': Device not configured",
        "fatal: could not read Password for 'https://x@github.com': No such device or address",
        "sudo: a terminal is required to read the password; either use the -S option",
        "Host key verification failed.",
        "sh: /dev/tty: Device not configured",
        "/bin/sh: 1: cannot open /dev/tty: No such device or address",
    ] {
        assert!(asked_on_terminal(asked), "{asked}");
    }
    for failed in [
        "git@github.com: Permission denied (publickey).",
        "fatal: Authentication failed for 'https://github.com/x/y.git/'",
        "error: could not lock config file",
    ] {
        assert!(!asked_on_terminal(failed), "{failed}");
    }
}

/// Leaving the sandbox is offered only for a genuine sandbox refusal.
#[test]
fn outside_the_sandbox_is_offered_only_when_it_could_help() {
    let outside = "rerun it unchanged once with outside_sandbox: true";
    let offered = |command: &str, output: &str| {
        sandbox_blocked(command, output, &[]).is_some_and(|next| next.contains(outside))
    };

    assert!(offered("make", "cc: /opt/x: Operation not permitted"));
    for refusal in [
        "dial unix /var/run/docker.sock: connect: operation not permitted",
        "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock",
    ] {
        assert!(offered("docker ps", refusal), "{refusal}");
    }
    if let Some(cookies) = sandbox::protected_paths().first() {
        let denial = format!(
            "cat: {}/Cookies: Operation not permitted",
            cookies.display()
        );
        let next = sandbox_blocked("cat Cookies", &denial, &[]).unwrap();
        assert!(!next.contains(outside) && next.contains("never opens it"));
    }
    for temp in ["/tmp", "/private/tmp"] {
        let next = sandbox_blocked(
            "touch /tmp/x",
            "touch: /tmp/x: Operation not permitted",
            &[PathBuf::from(temp)],
        )
        .unwrap();
        assert!(next.contains("scratch folder") && !next.contains("write_paths"));
    }
    let project = [PathBuf::from("/tmp/project")];
    let next = sandbox_blocked("make", "cc: /tmp/project/x: Permission denied", &project).unwrap();
    assert!(
        next.contains("write_paths"),
        "a folder inside temp can be granted"
    );
    let chrome = "\"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\" --headless";
    let next = sandbox_blocked(chrome, "Operation not permitted", &[]).unwrap();
    assert!(next.contains("render: true") && !next.contains(outside));
    assert_eq!(
        sandbox_blocked("cat notes", "cat: notes: Permission denied", &[]).is_none(),
        cfg!(target_os = "macos"),
        "on macOS a plain permission error is the file's own mode"
    );
}
