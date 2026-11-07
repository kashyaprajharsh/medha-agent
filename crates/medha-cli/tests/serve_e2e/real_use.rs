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
    assert_eq!(
        rewound["error"]["message"],
        "Nothing was rewound: b.txt changed after Medha's change, and putting it back would overwrite that. \
         No file was changed and no branch was made.",
        "{rewound}"
    );
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

/// A memory call without `op` is told each op's fields, and an op name sent as a field is pointed out.
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

/// An edit sent with carriage returns the file does not have still lands, and says what was adjusted.
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

/// An edit whose `old_string` has every escape doubled is told so and shown the line it meant.
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

/// A missed edit of a file never read says so and shows the nearest lines to copy from.
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

/// The nearest lines pasted back with their line numbers are refused with a plain reason.
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

/// A missed edit of a file changed since it was read says the file changed.
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

/// A call to a misnamed shell tool is told which offered tool to use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_misnamed_shell_call_is_pointed_at_the_shell_tool() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("misnamed")).await;
    let call = json!({"command": "echo hi", "network": false});
    let raw = scripted(
        &mut client,
        &world,
        &chat,
        &[format!("shell_command {call}")],
    )
    .await;
    assert!(raw.contains("Use `shell_exec`"), "{raw}");
}

/// A field the tool does not have is named in the result with the fields it does have.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_field_a_tool_does_not_have_is_named_with_the_one_it_meant() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("unknown-field")).await;
    let raw = scripted(
        &mut client,
        &world,
        &chat,
        &[format!("task_output {}", json!({"timeout_s": 5}))],
    )
    .await;
    assert!(
        raw.contains("`timeout_s` is not a field of task.output") && raw.contains("`wait_s`"),
        "{raw}"
    );
}

/// Code assigning an expression to a variable named `key` is read and edited as written, not redacted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_with_a_variable_named_key_is_read_and_edited_as_written() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("code-key");
    let code = "API_KEY = 'Zq81-vK2mPx9wLr7Tn'\n\n\ndef resolve(name):\n    key = name.upper()\n    return key\n\n\ndef pair_of(name):\n    pair = name.upper()\n    return pair\n";
    std::fs::write(folder.join("client.py"), code).unwrap();
    let chat = client.open(&folder).await;
    let edit = json!({"path": "client.py", "edits": [
        {"old_string": "    pair = name.upper()\n", "new_string": "    pair = name.upper().strip()\n"}]});
    let raw = scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("read {}", json!({"path": "client.py"})),
            format!("edit {edit}"),
        ],
    )
    .await;
    assert!(
        raw.contains("key = name.upper()") && raw.contains("pair = name.upper()"),
        "{raw}"
    );
    assert!(
        !raw.contains("Zq81-vK2mPx9wLr7Tn"),
        "a real key is still removed: {raw}"
    );
    assert!(read(&folder, "client.py").contains("pair = name.upper().strip()"));
}

/// An agent is continued only when the call says `continue`; a start that also names an agent still starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_is_continued_by_asking_for_it_and_never_by_a_stray_field() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("agents")).await;
    let first = json!({"name": "inv", "objective": "first job", "wait": true});
    let more = json!({"action": "continue", "agent": "inv", "text": "second job"});
    let other = json!({"name": "other", "objective": "third job", "agent": "inv", "wait": true});
    let raw = scripted(
        &mut client,
        &world,
        &chat,
        &[
            format!("agent_spawn {first}"),
            format!("agent_spawn {more}"),
            format!("agent_spawn {other}"),
        ],
    )
    .await;
    assert!(
        raw.contains("Picked up where it left off"),
        "continue reaches the agent: {raw}"
    );
    assert!(
        raw.contains(r#"\"agent\":\"/other\""#),
        "a start naming an agent still starts: {raw}"
    );
}

/// A `medha.lock` broken mid-chat keeps the settings already in force and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_broken_mid_chat_keeps_its_settings_and_says_so() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("broken-lock");
    std::fs::write(folder.join("notes.txt"), "kept\n").unwrap();
    std::fs::write(
        folder.join("medha.lock"),
        "[policy]\napprove = [\"read\"]\n",
    )
    .unwrap();
    let chat = client.open(&folder).await;
    let (asked, _) = read_watching(&mut client, &chat).await;
    assert!(asked, "the lock makes a read ask first");
    std::fs::write(folder.join("medha.lock"), "[policy\napprove = [\"read\"]\n").unwrap();
    let (asked, told) = read_watching(&mut client, &chat).await;
    assert!(asked, "a broken lock does not drop the rule in force");
    assert!(told, "the person is told the lock is broken");
}

/// Runs one scripted read, approving what it asks; whether it asked, and whether a lock notice was shown.
async fn read_watching(client: &mut Client, chat: &str) -> (bool, bool) {
    let read = json!({"path": "notes.txt"});
    client.send(chat, &format!("SCRIPT\nread {read}")).await;
    let (mut asked, mut told) = (false, false);
    loop {
        let (_, frame) = client.event().await;
        told |= frame.to_string().contains("medha.lock could not be read");
        if frame["method"] == "approval" {
            asked = true;
            let gate = json!({"gate_id": frame["params"]["gate_id"], "approve": true});
            client.ask("approval.respond", Some(chat), gate).await;
        }
        if kind(&frame, "turn.done") {
            return (asked, told);
        }
    }
}

/// A rewound branch, and reopening it later, keep what every other chat in the project saved to memory.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewound_branch_keeps_the_projects_shared_memory() {
    let world = World::new();
    let folder = world.folder("shared-memory");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let saver = client.open(&folder).await;
    let fact = json!({"op": "write", "name": "deploy-target", "claim": "Deploys go to alpha-cluster-7.",
        "description": "Deploys go to alpha-cluster-7", "kind": "project"});
    scripted(&mut client, &world, &saver, &[format!("memory {fact}")]).await;
    let written = result_of(&world, &saver, 0).await;
    assert_eq!(written["status"], "ok", "the memory is saved: {written}");
    let seen = |world: &World| {
        std::iter::from_fn(|| world.provider.seen.try_recv().ok())
            .last()
            .unwrap()["messages"]
            .to_string()
    };
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    assert!(
        seen(&world).contains("alpha-cluster-7"),
        "another chat's memory reaches this one"
    );
    let points = client
        .ask("session.rewind.points", Some(&chat), json!({}))
        .await;
    let at = points["result"]["points"][0]["id"].clone();
    let rewound = client
        .ask(
            "session.rewind",
            Some(&chat),
            json!({"at": at, "scope": "conversation"}),
        )
        .await;
    let branch = rewound["result"]["session"].as_str().unwrap().to_string();
    // The live chat keeps its key; the rewind switched it onto the branch.
    first_message(&mut client, &chat).await;
    assert!(
        seen(&world).contains("alpha-cluster-7"),
        "the rewind kept the project's memory"
    );
    drop(client);
    drop(backend);
    let backend = world.backend();
    let mut client = reopen(&backend, &folder, &branch).await;
    first_message(&mut client, &branch).await;
    assert!(
        seen(&world).contains("alpha-cluster-7"),
        "reopening the branch kept the project's memory"
    );
}

/// A fact about who the user is saves and reaches the next chat; a wrong kind's refusal names the right ones.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_memory_about_the_user_is_saved_and_a_wrong_kind_names_the_right_ones() {
    let world = World::new();
    let folder = world.folder("about-user");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let saver = client.open(&folder).await;
    let fact = json!({"op": "write", "name": "user-role", "claim": "The user is a platform engineer.",
        "description": "The user is a platform engineer", "kind": "user", "scope": "user"});
    let wrong = json!({"op": "write", "name": "user-team", "claim": "The user works on payments.",
        "description": "The user's team", "kind": "person"});
    scripted(
        &mut client,
        &world,
        &saver,
        &[format!("memory {fact}"), format!("memory {wrong}")],
    )
    .await;
    let written = result_of(&world, &saver, 0).await;
    assert_eq!(
        written["status"], "ok",
        "a fact about the user is saved: {written}"
    );
    let refused = result_of(&world, &saver, 1).await.to_string();
    assert!(
        [
            "preference",
            "project",
            "feedback",
            "reference",
            "decision",
            "user"
        ]
        .iter()
        .all(|kind| refused.contains(kind)),
        "the refusal names the kinds to choose from: {refused}"
    );
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let sent = std::iter::from_fn(|| world.provider.seen.try_recv().ok())
        .last()
        .unwrap()["messages"]
        .to_string();
    assert!(
        sent.contains("platform engineer"),
        "the next chat recalls who the user is"
    );
}

/// A reply streamed in thousands of tiny pieces, as a fast model sends it, never ends the chat it belongs to.
#[tokio::test]
async fn a_reply_streamed_in_thousands_of_pieces_keeps_the_chat_alive() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("burst")).await;
    client.send(&chat, "BURST 6000").await;
    let (_, end) = client
        .until(|frame| kind(frame, "turn.done") || frame["method"] == "session.ended")
        .await;
    assert_ne!(
        end["method"], "session.ended",
        "the chat ended mid-reply: {end}"
    );
    client.send(&chat, "Hi").await;
    client.until(|frame| kind(frame, "turn.done")).await;
}

/// Waiting longer than a minute on a background command returns its end, never a tool timeout.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_longer_than_a_minute_on_a_background_command_returns_its_end() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&world.folder("long-wait")).await;
    let start =
        json!({"command": "sleep 65; echo slow-work-done", "network": false, "background": true});
    let wait = json!({"task_id": "t1", "wait_s": 90});
    client
        .send(
            &chat,
            &format!("SCRIPT\nshell_exec {start}\ntask_output {wait}"),
        )
        .await;
    let (_, approval) = client.until(|frame| frame["method"] == "approval").await;
    let gate = json!({"gate_id": approval["params"]["gate_id"], "approve": true});
    client.ask("approval.respond", Some(&chat), gate).await;
    // The wait itself is quiet for over a minute, longer than the client's usual patience.
    loop {
        let frame = match client.events.pop_front() {
            Some(frame) => frame,
            None => tokio::time::timeout(
                Duration::from_secs(150),
                wire::read_frame(&mut client.reader),
            )
            .await
            .expect("the backend went quiet during the wait")
            .expect("the backend closed the connection"),
        };
        if frame["method"] == "session.event" && kind(&frame["params"]["frame"], "turn.done") {
            break;
        }
    }
    let waited = result_of(&world, &chat, 1).await.to_string();
    assert!(
        !waited.contains("timed out after"),
        "the wait failed as a timeout: {waited}"
    );
    assert!(
        waited.contains("slow-work-done"),
        "the wait did not return the command's end: {waited}"
    );
}

/// A write refused to a relative path names the folder it was refused in, never the shell that reported it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_relative_write_names_its_folder_not_the_shell() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let elsewhere = world.folder("elsewhere");
    let chat = client.open(&folder).await;
    let write = json!({"command": "echo x > made.txt", "network": false,
        "workdir": elsewhere, "read_paths": [elsewhere]});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {write}")]).await;
    let refused = result_of(&world, &chat, 0).await;
    assert!(
        !elsewhere.join("made.txt").exists(),
        "the write was not refused, so nothing was tested: {refused}"
    );
    assert_eq!(
        refused["payload"]["denied_paths"],
        json!([elsewhere]),
        "the refusal named the wrong folder: {refused}"
    );
}

/// A refusal whose message names no full path, as `mkdir` prints it, still names the folder the command ran in.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_naming_no_full_path_names_the_folder_the_command_ran_in() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let elsewhere = world.folder("elsewhere");
    let chat = client.open(&folder).await;
    let make = json!({"command": "mkdir newdir", "network": false,
        "workdir": elsewhere, "read_paths": [elsewhere]});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {make}")]).await;
    let refused = result_of(&world, &chat, 0).await;
    assert!(
        !elsewhere.join("newdir").exists(),
        "the folder was made, so nothing was tested: {refused}"
    );
    assert_eq!(
        refused["payload"]["denied_paths"],
        json!([elsewhere]),
        "the refusal named the wrong folder: {refused}"
    );
}

/// A command that changes folder itself names no refused folder, and is told to use workdir so the next refusal can.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_after_cd_names_no_folder_and_asks_for_workdir() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("project");
    let elsewhere = world.folder("elsewhere");
    let chat = client.open(&folder).await;
    let command = format!("cd {} && echo x > made.txt", elsewhere.display());
    let write = json!({"command": command, "network": false, "read_paths": [elsewhere]});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {write}")]).await;
    let refused = result_of(&world, &chat, 0).await;
    assert!(
        !elsewhere.join("made.txt").exists(),
        "the write was not refused, so nothing was tested: {refused}"
    );
    assert!(
        refused["payload"].get("denied_paths").is_none(),
        "a folder was named without proof: {refused}"
    );
    assert!(
        refused["payload"]["sandbox_blocked"]
            .as_str()
            .is_some_and(|note| note.contains("workdir")),
        "the note did not point to workdir: {refused}"
    );
}

/// The scratch folder the model was last shown, read from what it was sent.
#[cfg(unix)]
fn scratch_shown(world: &World) -> String {
    let sent: Vec<Value> = std::iter::from_fn(|| world.provider.seen.try_recv().ok()).collect();
    let system = sent.last().expect("the model was sent nothing")["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let (_, rest) = system
        .split_once("Scratch folder: ")
        .unwrap_or_else(|| panic!("no scratch folder in the brief: {system}"));
    rest.split_once(" (").unwrap().0.to_owned()
}

/// A reopened chat keeps its scratch folder, so the files its history names are still there and allowed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_chat_keeps_its_scratch_folder_and_its_files() {
    let world = World::new();
    let folder = world.folder("project");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let scratch = scratch_shown(&world);
    let write =
        json!({"command": "echo kept-in-scratch > note.txt", "network": false, "workdir": scratch});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {write}")]).await;
    drop(client);
    drop(backend);

    let backend = world.backend();
    let mut client = reopen(&backend, &folder, &chat).await;
    first_message(&mut client, &chat).await;
    assert_eq!(
        scratch_shown(&world),
        scratch,
        "the reopened chat was shown another scratch folder"
    );
    let read = json!({"command": "cat note.txt", "network": false, "workdir": scratch});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {read}")]).await;
    let logged = logged(&world, &chat).await;
    let last = logged
        .iter()
        .rev()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .expect("the read left no result")
        .payload
        .to_string();
    assert!(
        last.contains("kept-in-scratch") && last.contains("\"exit_code\":0"),
        "the reopened chat could not read its own scratch file: {last}"
    );
}

/// A rewound branch reopens on its chat's scratch folder, alongside the chat itself.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_branch_shares_its_chats_scratch_folder() {
    let world = World::new();
    let folder = world.folder("project");
    let backend = world.backend();
    let mut client = backend.connect().await;
    let chat = client.open(&folder).await;
    first_message(&mut client, &chat).await;
    let scratch = scratch_shown(&world);
    let write =
        json!({"command": "echo kept-in-scratch > note.txt", "network": false, "workdir": scratch});
    scripted(&mut client, &world, &chat, &[format!("shell_exec {write}")]).await;
    let points = client
        .ask("session.rewind.points", Some(&chat), json!({}))
        .await;
    let at = points["result"]["points"][0]["id"].clone();
    let rewound = client
        .ask(
            "session.rewind",
            Some(&chat),
            json!({"at": at, "scope": "conversation"}),
        )
        .await;
    let branch = rewound["result"]["session"].as_str().unwrap().to_string();
    drop(client);
    drop(backend);

    let backend = world.backend();
    let mut client = reopen(&backend, &folder, &branch).await;
    first_message(&mut client, &branch).await;
    assert_eq!(
        scratch_shown(&world),
        scratch,
        "the reopened branch was shown another scratch folder"
    );
    let read = json!({"command": "cat note.txt", "network": false, "workdir": scratch});
    scripted(
        &mut client,
        &world,
        &branch,
        &[format!("shell_exec {read}")],
    )
    .await;
    let last = logged(&world, &branch)
        .await
        .iter()
        .rev()
        .find(|event| event.kind == kernel::EventKind::ToolObs)
        .expect("the read left no result")
        .payload
        .to_string();
    assert!(
        last.contains("kept-in-scratch") && last.contains("\"exit_code\":0"),
        "the branch could not read its chat's scratch file: {last}"
    );

    let mut original = reopen(&backend, &folder, &chat).await;
    first_message(&mut original, &chat).await;
    assert_eq!(
        scratch_shown(&world),
        scratch,
        "the chat opened beside its branch was shown another scratch folder"
    );
}
