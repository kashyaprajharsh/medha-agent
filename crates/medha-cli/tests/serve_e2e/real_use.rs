//! Regression tests for bugs found in real use, each recreating the scenario it was found in.
use super::*;
use kernel::EventLog;

/// The chat's events as the backend logged them.
async fn logged(world: &World, chat: &str) -> Vec<kernel::Event> {
    let state = std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("events.db").is_file())
        .unwrap();
    let log = store::SqliteLog::open(state.join("events.db")).unwrap();
    log.events(chat.parse().unwrap()).await
}

#[cfg(unix)]
fn git(folder: &Path, args: &[&str]) {
    let done = Command::new("git")
        .args(args)
        .current_dir(folder)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(done.status.success(), "git {args:?}: {done:?}");
}

/// A repository on `main` with `a.txt` and `c.txt`, and a branch `other` where `c.txt` differs.
#[cfg(unix)]
fn two_branch_repo(folder: &Path) {
    git(folder, &["init", "-q", "-b", "main"]);
    std::fs::write(folder.join("a.txt"), "a0\n").unwrap();
    std::fs::write(folder.join("b.txt"), "b0\n").unwrap();
    std::fs::write(folder.join("c.txt"), "main\n").unwrap();
    git(folder, &["add", "."]);
    git(folder, &["commit", "-qm", "start"]);
    git(folder, &["checkout", "-qb", "other"]);
    std::fs::write(folder.join("c.txt"), "other\n").unwrap();
    git(folder, &["commit", "-qam", "other"]);
    git(folder, &["checkout", "-q", "main"]);
}

/// Sends a plain first message, the point every rewind below returns to.
#[cfg(unix)]
async fn first_message(client: &mut Client, chat: &str) {
    client.send(chat, "Hi").await;
    client.until(|frame| kind(frame, "turn.done")).await;
}

#[cfg(unix)]
async fn rewind_to_first(client: &mut Client, chat: &str) -> (Value, Value) {
    let points = client
        .ask("session.rewind.points", Some(chat), json!({}))
        .await;
    let first = points["result"]["points"][0].clone();
    let rewound = client
        .ask(
            "session.rewind",
            Some(chat),
            json!({"at": first["id"], "scope": "both"}),
        )
        .await;
    (first, rewound)
}

fn read(folder: &Path, name: &str) -> String {
    std::fs::read_to_string(folder.join(name)).unwrap()
}

/// A rewind restores only the model's own project edits: never Git's changes, never files outside the project.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewind_restores_the_models_own_edits_not_gits_and_never_asks_for_outside_files() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let outside = world.folder("elsewhere");
    std::fs::write(outside.join("probe.py"), "old\n").unwrap();
    two_branch_repo(&folder);
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let checkout = json!({"command": "git checkout -q other", "network": false});
    let edit = json!({"path": "a.txt", "content": "a1\n"});
    let probe_path = outside.join("probe.py");
    let probe = json!({"path": probe_path, "content": "new\n"});
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("shell_exec {checkout}"),
            format!("read {}", json!({"path": "a.txt"})),
            format!("write {edit}"),
            format!("read {}", json!({"path": probe_path})),
            format!("write {probe}"),
        ],
    )
    .await;
    assert_eq!(read(&folder, "c.txt"), "other\n");
    assert_eq!(read(&folder, "a.txt"), "a1\n", "the model's edit landed");
    assert_eq!(
        read(&outside, "probe.py"),
        "new\n",
        "the outside write landed"
    );
    client.events.clear();
    let (first, rewound) = rewind_to_first(&mut client, &chat).await;
    assert_eq!(
        first["files"], 1,
        "only the model's own project edit is offered: {first}"
    );
    assert!(rewound.get("error").is_none(), "{rewound}");
    assert_eq!(read(&folder, "a.txt"), "a0\n", "the model's edit is undone");
    assert_eq!(
        read(&folder, "c.txt"),
        "other\n",
        "Git's change is left to Git"
    );
    assert_eq!(
        read(&outside, "probe.py"),
        "new\n",
        "outside the project is not touched"
    );
    let asked = client
        .events
        .iter()
        .find(|frame| frame["params"]["frame"]["method"] == "approval");
    assert!(asked.is_none(), "rewind asked for approval: {asked:?}");
}

/// A rewind that cannot restore every file changes none and makes no branch.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewind_that_cannot_finish_changes_no_file_and_makes_no_branch() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    two_branch_repo(&folder);
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let both = json!({"command": "echo a1 > a.txt && echo b1 > b.txt", "network": false});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {both}")]).await;
    // The person edits b.txt after the command, so it must not be overwritten, and so a.txt must not be either.
    std::fs::write(folder.join("b.txt"), "mine\n").unwrap();
    let chats_before = logged_chats(&world).await;
    let (_, rewound) = rewind_to_first(&mut client, &chat).await;
    assert!(rewound.get("error").is_some(), "{rewound}");
    assert_eq!(
        read(&folder, "a.txt"),
        "a1\n",
        "nothing is restored when one file cannot be"
    );
    assert_eq!(read(&folder, "b.txt"), "mine\n");
    assert_eq!(
        logged_chats(&world).await,
        chats_before,
        "a failed rewind leaves no branch"
    );
}

#[cfg(unix)]
async fn logged_chats(world: &World) -> usize {
    let state = std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("events.db").is_file())
        .unwrap();
    store::SqliteLog::open(state.join("events.db"))
        .unwrap()
        .list_sessions()
        .unwrap()
        .len()
}

/// An expired command backup does not stop the model's own edits from being rewound.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_command_backup_still_lets_the_models_own_edit_be_rewound() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    two_branch_repo(&folder);
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let command = json!({"command": "echo b1 > b.txt", "network": false});
    let edit = json!({"path": "a.txt", "content": "a1\n"});
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("shell_exec {command}"),
            format!("read {}", json!({"path": "a.txt"})),
            format!("write {edit}"),
        ],
    )
    .await;
    assert_eq!(read(&folder, "a.txt"), "a1\n", "the model's edit landed");
    for snapshots in std::fs::read_dir(world.home().join("projects")).unwrap() {
        let copies = snapshots.unwrap().path().join("command-snapshots");
        for entry in std::fs::read_dir(&copies).into_iter().flatten().flatten() {
            if entry.path().is_file() && entry.file_name().len() == 26 {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    let (_, rewound) = rewind_to_first(&mut client, &chat).await;
    assert!(rewound.get("error").is_none(), "{rewound}");
    assert_eq!(
        read(&folder, "a.txt"),
        "a0\n",
        "the model's own edit is undone"
    );
    assert_eq!(
        read(&folder, "b.txt"),
        "b1\n",
        "the expired one is left and named"
    );
    assert!(rewound.to_string().contains("b.txt"), "{rewound}");
}

/// `Personal`, rows 11582 and 168: `memory` called without `op`, once with `write` sent as a field; the retry that
/// followed one of them failed again on a field the error had not named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_memory_call_without_op_is_told_each_ops_fields() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let chat = client.open(&folder).await;
    let fields = json!({
        "name": "clean-workspace-after-task",
        "claim": "User cares about disk space and wants temporary build artifacts, venvs, dependencies and helper scripts deleted after a task.",
        "description": "Clean up temp artifacts after tasks; avoid heavy installs",
        "kind": "preference",
        "scope": "user",
    });
    let mut as_field = fields.clone();
    as_field["write"] = Value::Null;
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("memory {fields}"), format!("memory {as_field}")],
    )
    .await;
    let missing = result_of(&world, &chat, 0).await.to_string();
    assert!(
        missing.contains("write (name, claim, description, kind)"),
        "{missing}"
    );
    let named = result_of(&world, &chat, 1).await.to_string();
    assert!(named.contains("belongs in op"), "{named}");
}

/// The logged result of a scripted chat's call number `step`, counted from 0.
async fn result_of(world: &World, chat: &str, step: usize) -> Value {
    let id = format!("script_{step}");
    logged(world, chat)
        .await
        .into_iter()
        .find(|event| {
            event.kind == kernel::EventKind::ToolObs && event.payload["intent_id"] == id.as_str()
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
        .payload
}

/// `Personal`, row 10692: Qwen3.8-27B sent an edit with a carriage return between every token, in a file with
/// none, and the edit missed three times.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_damaged_with_carriage_returns_lands_in_a_file_that_has_none() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("pitch");
    let file = "{\n  \"components\": [\n    { \"id\": \"user\", \"type\": \"person\", \"label\": \"You\", \"pos\": [40, 200], \"size\": [120, 62] },\n    { \"id\": \"cloud\", \"type\": \"external\", \"label\": \"Cloud\", \"sublabel\": \"not used — nothing to call\", \"pos\": [1120, 470], \"size\": [130, 62] }\n  ]\n}\n";
    std::fs::write(folder.join("architecture.json"), file).unwrap();
    let chat = client.open(&folder).await;
    let old = "\r \r \r \r \r{\r \r\"id\r\"\r:\r \r\"cloud\r\"\r,\r \r\"type\r\"\r:\r \r\"external\r\"\r,\r \r\"label\r\"\r:\r \r\"Cloud\r\"\r,\r \r\"sublabel\r\"\r:\r \r\"not\r used\r \r—\r nothing\r to\r call\r\"\r,\r \r\"pos\r\"\r:\r \r[1120\r,\r 470\r]\r,\r \r\"size\r\"\r:\r \r[130\r,\r 62\r]\r \r}";
    let new = old.replace("[1120\r", "[40\r").replace("[130\r", "[126\r");
    let edit =
        json!({"path": "architecture.json", "edits": [{"old_string": old, "new_string": new}]});
    scripted(&mut client, &world, &chat, &[format!("edit {edit}")]).await;
    let result = result_of(&world, &chat, 0).await;
    let now = read(&folder, "architecture.json");
    assert!(
        now.contains("\"pos\": [40, 470], \"size\": [126, 62] }"),
        "{result}"
    );
    assert!(!now.contains('\r'), "no carriage return reaches the file");
    assert!(result.to_string().contains("carriage returns"), "{result}");
}

/// `Personal`, row 3485: the model sent `old_string` with every backslash doubled, and the miss said only "not
/// found", so it could not tell what was wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_with_doubled_escapes_is_told_so_and_shown_the_line() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("antarman-pitch");
    let line = r#"  s.addText('\"That sounds like what you described three weeks ago — the quiet when everyone\'s gone. You mentioned that Tuesday too.\"", {"#;
    let file = format!(
        "  }});\n{line}\n    x: 2.0, y: 2.7, w: 7, h: 0.8,\n    fontSize: 14, color: C.text, fontFace: \"Inter\",\n"
    );
    std::fs::write(folder.join("generate.js"), &file).unwrap();
    let chat = client.open(&folder).await;
    let old = r#"  s.addText('\\\"That sounds like what you described three weeks ago — the quiet when everyone\\'s gone. You mentioned that Tuesday too.\\\"\", {"#;
    let new = r#"  s.addText("That sounds like what you described three weeks ago — the quiet when everyone's gone. You mentioned that Tuesday too.", {"#;
    let edit = json!({"path": "generate.js", "old_string": old, "new_string": new});
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("read {}", json!({"path": "generate.js", "limit": 5})),
            format!("edit {edit}"),
        ],
    )
    .await;
    let result = result_of(&world, &chat, 1).await;
    let error = result["payload"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("backslash-escaped quotes"), "{result}");
    assert!(
        error.contains(&format!("2|{line}")),
        "the file's line is shown: {error}"
    );
    assert_eq!(
        read(&folder, "generate.js"),
        file,
        "nothing is guessed into the file"
    );
}

/// `Personal`, row 18074: the model wrote `strategy.md` with a shell command, never read it, and edited a line from
/// memory; the miss gave it nothing to correct from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missed_edit_of_an_unread_file_says_so_and_shows_the_nearest_lines() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("upsc-prep");
    let line = "**Indian Language (Paper A):** Choose **one** of the languages listed in the 8th Schedule (Hindi, Marathi, Bengali, Telugu, Tamil, Urdu, Gujarati, Kannada, Malayalam, Punjabi, Oriya, Assamese + others — see the CSE-2027 notification for the exact list), at matric standard.";
    let heredoc = format!(
        "cat > strategy.md << 'MDEOF'\n# UPSC CSE 2027\n\n## Mains\n\n{line}\n\n**English (Paper B):** matric standard.\nMDEOF"
    );
    let write = json!({"command": heredoc, "network": false});
    let edit = json!({
        "path": "strategy.md",
        "old_string": line.replace("exact list", "final list"),
        "new_string": line.replace("languages listed in", "languages in"),
    });
    let chat = client.open(&folder).await;
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("shell_exec {write}"), format!("edit {edit}")],
    )
    .await;
    let result = result_of(&world, &chat, 1).await;
    let error = result["payload"]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("No read in this chat shows this file's current text."),
        "{error}"
    );
    assert!(
        error.contains(&format!("5|{line}")),
        "the nearest line, numbered: {error}"
    );
    assert!(read(&folder, "strategy.md").contains("exact list"));
}

/// `Personal`, row 18074 again: the model copies the shown line back with its number; it is told to drop the number.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_nearest_lines_pasted_back_with_their_numbers_are_refused_plainly() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("upsc-prep");
    std::fs::write(
        folder.join("strategy.md"),
        "# UPSC\n\n## Mains\n\nOne of the 8th Schedule languages.\n\nEnglish at matric standard.\n",
    )
    .unwrap();
    let chat = client.open(&folder).await;
    let miss = json!({"path": "strategy.md", "old_string": "One of the 8th Schedule tongues.", "new_string": "Any 8th Schedule language."});
    let pasted = json!({"path": "strategy.md", "old_string": "4|\n5|One of the 8th Schedule languages.", "new_string": "\nAny 8th Schedule language."});
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("edit {miss}"), format!("edit {pasted}")],
    )
    .await;
    let error = result_of(&world, &chat, 1).await.to_string();
    assert!(error.contains("line numbers"), "{error}");
    assert!(read(&folder, "strategy.md").contains("languages."));
}

/// `draft`, row 437: the model edited text it had read, but the file had changed twice since; the miss did not say.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missed_edit_of_a_file_changed_since_its_read_says_so() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("draft");
    let page = "<script>\n    if (!reduced) requestAnimationFrame(frame);\n  }\n  let pauseT = 0, decidedThisPass = false;\n  proposeEdit();\n</script>\n";
    std::fs::write(folder.join("agents.html"), page).unwrap();
    let chat = client.open(&folder).await;
    let change = json!({"command": "sed -i.bak 's/pauseT = 0/pauseT = 0.9/' agents.html && rm agents.html.bak", "network": false});
    let edit = json!({
        "path": "agents.html",
        "old_string": "  let pauseT = 0, decidedThisPass = false;\n  proposeEdit();",
        "new_string": "  let pauseT = 0, decidedThisPass = false;\n  proposeEdit();\n  requestAnimationFrame(frame);",
    });
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("read {}", json!({"path": "agents.html"})),
            format!("shell_exec {change}"),
            format!("edit {edit}"),
        ],
    )
    .await;
    let error = result_of(&world, &chat, 2).await["payload"]["error"].to_string();
    assert!(
        error.contains("The file has changed since this chat last read it."),
        "{error}"
    );
    assert!(
        error.contains("4|  let pauseT = 0.9, decidedThisPass = false;"),
        "{error}"
    );
}

/// A resent edit that already landed reports done without writing again; a second write would add a rewind step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_sent_again_after_it_landed_reports_done_and_writes_nothing() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("notes");
    std::fs::write(folder.join("plan.md"), "Start 28 Sep 2026\n").unwrap();
    let chat = client.open(&folder).await;
    let edit = json!({"path": "plan.md", "old_string": "Start 28 Sep 2026", "new_string": "Start 4 Oct 2026"});
    scripted(
        &mut client,
        &world,
        &chat,
        &[format!("edit {edit}"), format!("edit {edit}")],
    )
    .await;
    let again = result_of(&world, &chat, 1).await;
    assert_eq!(again["status"], "ok", "{again}");
    assert!(
        again["payload"]["note"]
            .to_string()
            .contains("already made"),
        "{again}"
    );
    assert!(
        again["payload"].get("snapshot").is_none(),
        "nothing written: {again}"
    );
    assert_eq!(read(&folder, "plan.md"), "Start 4 Oct 2026\n");
}

/// A sub-agent transcript of local reads is not labelled as web content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_only_sub_agent_transcript_does_not_mark_the_chat_as_web() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("inventory");
    std::fs::write(folder.join("notes.txt"), "local only\n").unwrap();
    let chat = client.open(&folder).await;
    let objective = format!("SCRIPT\nread {}", json!({"path": "notes.txt"}));
    let spawn = json!({"name": "inv", "objective": objective, "wait": true});
    let transcript = json!({"action": "transcript", "agent": "inv"});
    scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("agent_spawn {spawn}"),
            format!("agent {transcript}"),
        ],
    )
    .await;
    let events = logged(&world, &chat).await;
    let read_back = events
        .iter()
        .find(|event| {
            event.kind == kernel::EventKind::ToolObs
                && event.payload["payload"]["agent"] == "inv"
                && event.payload["payload"]["steps"].is_array()
        })
        .unwrap_or_else(|| panic!("no transcript result: {events:?}"));
    assert_ne!(
        read_back.trust,
        kernel::TrustLabel::Web,
        "a transcript of local reads is not web content: {:?}",
        read_back.payload
    );
}
