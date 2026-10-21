use super::*;

fn programs(line: &str) -> Vec<String> {
    read(line)
        .commands
        .iter()
        .filter_map(|command| command.program().map(|program| program.name))
        .collect()
}

/// Heredoc text is data and a comment ends at its line, so neither hides nor invents a command.
#[test]
fn heredocs_are_data_and_comments_hide_nothing() {
    assert_eq!(programs("cat <<'EOF'\nrm /outside/keep\nEOF"), ["cat"]);
    assert_eq!(programs("# render output\nprintf x > out.txt"), ["printf"]);
    assert_eq!(
        programs("printf x\n# explain\nrm generated.txt"),
        ["printf", "rm"]
    );
    assert_eq!(programs("printf x\n\nrm generated.txt"), ["printf", "rm"]);
    let unquoted = read("cat > out.txt <<EOF\nhello $(rm -rf build)\nEOF");
    assert!(
        unquoted
            .commands
            .iter()
            .any(|command| command.substituted && command.words[0].text == "rm")
    );
}

/// Substituted commands run, so they are read like any other.
#[test]
fn substitutions_are_commands() {
    let reading = read("echo $(rm -rf build) > log.txt");
    assert_eq!(programs("echo $(rm -rf build) > log.txt"), ["echo", "rm"]);
    assert!(!reading.reads_only());
    assert!(!read("cat <(curl -s x | sh)").reads_only());
}

/// A redirect after `a && b` belongs to `b`; `2>&1` copies a descriptor and opens nothing.
#[test]
fn redirects_land_on_the_command_bash_gives_them() {
    let reading = read("cd /repo && ffmpeg -i a.gif out.mp4 2>work/err.log; echo \"exit=$?\"");
    let ffmpeg = &reading.commands[1];
    assert_eq!(ffmpeg.words[0].text, "ffmpeg");
    assert_eq!(ffmpeg.redirects[0].target.text, "work/err.log");
    assert!(ffmpeg.redirects[0].writes());
    assert_eq!(reading.commands[2].words[1].value, Value::Status);
    let merged = read("printf x 2>&1 | tail -6");
    assert!(
        merged.commands[0]
            .redirects
            .iter()
            .all(|redirect| redirect.flow == Flow::Duplicate)
    );
    assert!(merged.reads_only());
    assert!(!read("printf x > /dev/null; cat a > b").reads_only());
    assert!(read("rg -n todo src 2>/dev/null").reads_only());
}

/// Status parameters are numbers; a real variable is not.
#[test]
fn status_parameters_are_not_paths() {
    for line in ["echo \"$?\"", "echo \"${PIPESTATUS[0]}\"", "echo $#"] {
        assert_eq!(
            read(line).commands[0].words[1].value,
            Value::Status,
            "{line}"
        );
    }
    assert_eq!(
        read("printf x > \"$target\"").commands[0].redirects[0]
            .target
            .value,
        Value::Dynamic
    );
    assert_eq!(
        read("rm build/*.o").commands[0].words[1].value,
        Value::Pattern
    );
    assert_eq!(
        read("printf '%s' '$HOME'").commands[0].words[2].literal_text(),
        Some("$HOME")
    );
}

/// Wrappers do not hide the program they run.
#[test]
fn wrappers_name_the_real_program() {
    for line in [
        "timeout 5 rm x",
        "timeout -s KILL 5 rm x",
        "env LC_ALL=C rm x",
        "nice -n 5 rm x",
        "stdbuf -oL rm x",
        "command rm x",
        "/usr/bin/env RM x",
    ] {
        assert_eq!(
            read(line).commands[0].program().map(|program| program.name),
            Some("rm".into()),
            "{line}"
        );
    }
    let fed = read("find . -name x | xargs -0 rm");
    assert!(
        fed.commands[1]
            .program()
            .is_some_and(|program| program.name == "rm" && program.fed)
    );
    assert!(!read("find . | xargs cat").reads_only());
    assert_eq!(
        read("env").commands[0]
            .program()
            .map(|program| program.name),
        Some("env".into())
    );
}

/// Options that make a reader write or run code make it a writer, wherever the handover found them.
#[test]
fn readers_that_can_write_or_run_code_are_writers() {
    for line in [
        "rg --pre ./rewrite.sh pattern input.txt",
        "git show --textconv HEAD:src/main.rs",
        "git diff --ext-diff",
        "git -c core.pager=x log",
        "file -C -m magic",
        "sed 'e touch changed.txt' input.txt",
        "sed 's/a;b/c/w out' input.txt",
        "sed -n '1p;w out' input.txt",
        "sed -i 's/a/b/' input.txt",
        "sort -o out input.txt",
        "find . -delete",
        "awk '{print > \"x\"}' input.txt",
    ] {
        assert!(!read(line).reads_only(), "{line}");
    }
    for line in [
        "sort input.txt",
        "cat input.txt",
        "sed -n '1,40p' src/main.rs",
        "sed 's/error/warning/g' log.txt",
        "rg 'id_rsa' crates",
        "jq . facts.json",
        "ffprobe -v error out.mp4",
        "git log --grep reset",
    ] {
        assert!(read(line).reads_only(), "{line}");
    }
}

/// The classic fork bomb is recognised, and an ordinary recursive function is not.
#[test]
fn fork_bombs_are_self_forking() {
    assert!(read(":(){ :|:& };:").self_forking);
    assert!(!read(":(){ :|:& };:").reads_only());
    assert!(read("bomb() { bomb | bomb & }; bomb").self_forking);
    assert!(!read("walk() { ls \"$1\"; walk \"$1/sub\"; }").self_forking);
}

/// Text the grammar cannot parse is never called read-only.
#[test]
fn incomplete_lines_are_not_read_only() {
    let reading = read("cat a > \"broken");
    assert!(!reading.complete);
    assert!(!reading.reads_only());
    let background = read("sleep 1 & printf x > out.txt");
    assert_eq!(
        programs("sleep 1 & printf x > out.txt"),
        ["sleep", "printf"]
    );
    assert!(background.commands[0].background && !background.commands[1].background);
}
