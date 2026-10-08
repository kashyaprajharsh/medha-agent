//! External ACP sessions exercise the same backend as the application clients.
use super::*;

fn initialize(editor: &mut EditorProcess) {
    let hello = editor.rpc(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    assert_eq!(hello["result"]["protocolVersion"], 1, "{hello}");
    assert_eq!(hello["result"]["agentCapabilities"]["loadSession"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn numbered_cancel_never_interrupts_another_viewers_newer_turn() {
    let world = World::new();
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let session = viewer.open(&world.folder("cancel-generation")).await;
    let first = viewer.send(&session, "finished first").await;
    viewer.until(|frame| kind(frame, "turn.done")).await;
    world.provider.asked();
    let next = viewer.send(&session, "HOLD second viewer turn").await;
    assert!(world.provider.asked().contains("HOLD second viewer turn"));
    let stale = viewer
        .ask(
            "turn.cancel",
            Some(&session),
            json!({"turn":first["result"]["turn"]}),
        )
        .await;
    assert_eq!(stale["result"]["cancelled"], false, "{stale}");
    let snapshot = viewer
        .ask("session.presentation", Some(&session), json!({}))
        .await;
    assert_eq!(snapshot["result"]["running"], true);
    let stopped = viewer
        .ask(
            "turn.cancel",
            Some(&session),
            json!({"turn":next["result"]["turn"]}),
        )
        .await;
    assert_eq!(stopped["result"]["cancelled"], true, "{stopped}");
    viewer.until(|frame| kind(frame, "turn.cancelled")).await;
    world.provider.release.send(()).unwrap();
}

fn load(editor: &mut EditorProcess, folder: &Path, session: &str, method: &str) -> Value {
    let result = editor.rpc(
        method,
        json!({"cwd":folder,"sessionId":session,"mcpServers":[]}),
    );
    assert_eq!(result["result"]["sessionId"], session, "{result}");
    editor.session = session.to_owned();
    result
}

fn chunks(editor: &EditorProcess, session: &str, kind: &str) -> String {
    editor
        .heard
        .iter()
        .filter(|frame| {
            frame["method"] == "session/update"
                && frame["params"]["sessionId"] == session
                && frame["params"]["update"]["sessionUpdate"] == kind
        })
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_images_only_reach_the_model_and_load_from_saved_artifacts() {
    use base64::Engine;
    let world = World::new();
    let folder = world.folder("image-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut editor = EditorProcess::start(
        &folder,
        &world.home(),
        &world.provider,
        &[("MEDHA_IMAGE_INPUT", "native")],
    );
    let session = editor.session.clone();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let mut image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(image.into_inner());
    let completed = editor.rpc(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"image","mimeType":"image/png","data":encoded}]}),
    );
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let request = world.provider.seen.recv_timeout(WAIT).unwrap();
    assert!(last_user(&request).contains("Describe the attached image(s)."));
    assert!(
        last_user(&request).contains("data:image/png;base64,"),
        "{request}"
    );
    viewer.until(|frame| kind(frame, "turn.done")).await;
    editor.disconnect();
    let mut loaded = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut loaded);
    load(&mut loaded, &folder, &session, "session/load");
    let images: Vec<_> = loaded
        .heard
        .iter()
        .filter(|frame| {
            frame["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                && frame["params"]["update"]["content"]["type"] == "image"
        })
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(
        images[0]["params"]["update"]["content"]["mimeType"],
        "image/png"
    );
    let restored = base64::engine::general_purpose::STANDARD
        .decode(
            images[0]["params"]["update"]["content"]["data"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(image::load_from_memory(&restored).unwrap().width(), 2);
    let invalid = loaded.rpc(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"must not send partial text"},
            {"type":"image","mimeType":"image/png","data":"invalid"}]}),
    );
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    assert!(
        world
            .provider
            .seen
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    let active = viewer
        .ask(
            "message.send",
            Some(&session),
            json!({"content":"HOLD live image",
        "images":[{"mime":"image/png","data":encoded}]}),
        )
        .await;
    assert_eq!(active["result"]["accepted"], true, "{active}");
    assert!(world.provider.asked().contains("HOLD live image"));
    let mut late = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut late);
    load(&mut late, &folder, &session, "session/load");
    assert_eq!(
        late.heard
            .iter()
            .filter(
                |frame| frame["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                    && frame["params"]["update"]["content"]["type"] == "image"
            )
            .count(),
        2,
        "loading an active image turn lost or duplicated an image"
    );
    late.rpc("session/cancel", json!({"sessionId":session}));
    viewer.until(|frame| kind(frame, "turn.cancelled")).await;
    world.provider.release.send(()).unwrap();
}

fn python() -> PathBuf {
    for executable in ["python3", "python"] {
        if let Ok(output) = Command::new(executable)
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            && output.status.success()
        {
            let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
            if path.is_absolute() {
                return path;
            }
        }
    }
    panic!("The ACP stdio-MCP integration fixture requires Python (provided by CI runners)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_mcp_is_chat_scoped_survives_reload_and_does_not_persist_credentials() {
    let world = World::new();
    let folder = world.folder("session-mcp-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut editor = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut editor);
    let program = python();
    let script = r#"import sys, json, os
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); ident=m.get('id')
 if method=='initialize':
  open('editor-mcp-started.txt', 'w').write(os.environ['EDITOR_MCP_TOKEN'])
  result={'protocolVersion':m['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'editor-test','version':'1'}}
 elif method=='tools/list':
  result={'tools':[{'name':'ping','description':'Ping the editor server','inputSchema':{'type':'object'}}]}
 elif method=='tools/call':
  result={'content':[{'type':'text','text':'pong'}]}
 else: result={}
 if ident is not None:
  print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#;
    let mcp = json!([{"name":"Editor Test", "command":program, "args":["-c",script],
        "env":[{"name":"EDITOR_MCP_TOKEN","value":"isolated-editor-secret"}]}]);
    let made = editor.rpc("session/new", json!({"cwd":folder,"mcpServers":mcp}));
    let session = made["result"]["sessionId"].as_str().unwrap().to_owned();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let deadline = Instant::now() + WAIT;
    loop {
        let bootstrap = viewer
            .ask(
                "session.inspect",
                Some(&session),
                json!(protocol::SessionRead::Bootstrap),
            )
            .await;
        let decoded: protocol::SessionResult =
            serde_json::from_value(bootstrap["result"].clone()).unwrap();
        if matches!(decoded, protocol::SessionResult::Bootstrap(protocol::Bootstrap { tools, .. })
            if tools.iter().any(|tool| tool.name.starts_with("mcp__editor_") && tool.name.ends_with("__ping")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session MCP never offered its tool: {bootstrap}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        std::fs::read_to_string(folder.join("editor-mcp-started.txt")).unwrap(),
        "isolated-editor-secret"
    );
    let reload = viewer
        .ask("extensions.reload", Some(&session), json!({}))
        .await;
    assert!(reload.get("error").is_none(), "{reload}");
    let bootstrap = viewer
        .ask(
            "session.inspect",
            Some(&session),
            json!(protocol::SessionRead::Bootstrap),
        )
        .await;
    assert!(
        bootstrap.to_string().contains("mcp__editor_"),
        "reload removed session MCP: {bootstrap}"
    );
    let other = viewer.open(&world.folder("no-editor-mcp")).await;
    let bootstrap = viewer
        .ask(
            "session.inspect",
            Some(&other),
            json!(protocol::SessionRead::Bootstrap),
        )
        .await;
    assert!(
        !bootstrap.to_string().contains("mcp__editor_"),
        "session MCP reached another chat: {bootstrap}"
    );
    for name in ["config.toml", "credentials.toml"] {
        let saved = std::fs::read_to_string(world.home().join(name)).unwrap_or_default();
        assert!(
            !saved.contains("isolated-editor-secret"),
            "ephemeral token was persisted to {name}"
        );
    }
    assert!(
        !editor
            .heard
            .iter()
            .any(|frame| frame.to_string().contains("isolated-editor-secret"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_loads_active_history_once_cancels_and_detaches_only_itself() {
    let world = World::new();
    let folder = world.folder("active-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let session = viewer.open(&folder).await;
    viewer.send(&session, "durable first message").await;
    assert!(world.provider.asked().contains("durable first message"));
    viewer.until(|frame| kind(frame, "turn.done")).await;
    viewer.send(&session, "HOLD active editor load").await;
    assert!(world.provider.asked().contains("HOLD active editor load"));

    let mut editor = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut editor);
    load(&mut editor, &folder, &session, "session/load");
    assert_eq!(
        chunks(&editor, &session, "user_message_chunk"),
        "durable first messageHOLD active editor load"
    );
    assert_eq!(
        chunks(&editor, &session, "agent_message_chunk"),
        "echo: durable first message"
    );
    editor.rpc("session/cancel", json!({"sessionId":session}));
    viewer.until(|frame| kind(frame, "turn.cancelled")).await;
    world.provider.release.send(()).unwrap();
    editor.disconnect();

    let settings = viewer
        .ask("session.settings", Some(&session), json!({}))
        .await;
    assert!(settings.get("error").is_none(), "{settings}");
    viewer.send(&session, "still alive after editor exit").await;
    assert!(
        world
            .provider
            .asked()
            .contains("still alive after editor exit")
    );
    viewer.until(|frame| kind(frame, "turn.done")).await;
    let status = viewer.ask("backend.status", None, json!({})).await;
    assert_eq!(status["result"]["chats"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_prompt_waits_for_completion_and_cancellation_does_not_start_queued_input() {
    let world = World::new();
    let folder = world.folder("prompt-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut editor = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
    let session = editor.session.clone();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let first = editor.send(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"HOLD editor prompt"}]}),
    );
    assert!(world.provider.asked().contains("HOLD editor prompt"));
    if let Ok(frame) = editor.frames.recv_timeout(Duration::from_millis(100)) {
        assert_ne!(frame["id"], first, "prompt completed while model was held");
        editor.heard.push(frame);
    }
    let second = editor.rpc(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"must not become a steer"}]}),
    );
    assert_eq!(second["error"]["code"], -32002, "{second}");
    editor.rpc("session/cancel", json!({"sessionId":session}));
    assert_eq!(editor.result(first)["result"]["stopReason"], "cancelled");
    viewer.until(|frame| kind(frame, "turn.cancelled")).await;
    world.provider.release.send(()).unwrap();
    assert!(
        world
            .provider
            .seen
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );

    let finished = editor.ask("message.send", json!({"content":"editor finished"}));
    assert_eq!(finished["result"]["stopReason"], "end_turn", "{finished}");
    assert!(world.provider.asked().contains("editor finished"));
    assert!(chunks(&editor, &session, "agent_message_chunk").contains("echo: editor finished"));
    assert_eq!(
        chunks(&editor, &session, "user_message_chunk"),
        "",
        "ACP echoed a prompt that the sending editor already displays"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_editor_load_replays_history_while_resume_skips_it_and_eof_releases_the_chat() {
    let world = World::new();
    let folder = world.folder("inactive-editor");
    let backend = world.backend();
    let mut observer = backend.connect().await;
    let mut first = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
    let session = first.session.clone();
    let answer = first.ask("message.send", json!({"content":"saved editor input"}));
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    assert!(world.provider.asked().contains("saved editor input"));
    first.disconnect();
    let deadline = Instant::now() + WAIT;
    loop {
        let live = observer.ask("session.list", None, json!({})).await;
        if live["result"]["sessions"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stdin EOF retained an unwatched chat: {live}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut loaded = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut loaded);
    load(&mut loaded, &folder, &session, "session/load");
    assert_eq!(
        chunks(&loaded, &session, "user_message_chunk"),
        "saved editor input"
    );
    assert_eq!(
        chunks(&loaded, &session, "agent_message_chunk"),
        "echo: saved editor input"
    );
    let mut resumed = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut resumed);
    load(&mut resumed, &folder, &session, "session/resume");
    assert_eq!(chunks(&resumed, &session, "user_message_chunk"), "");
    assert_eq!(chunks(&resumed, &session, "agent_message_chunk"), "");
    let completed = resumed.ask("message.send", json!({"content":"continued saved chat"}));
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let request = world.provider.seen.recv_timeout(WAIT).unwrap();
    assert!(
        user_texts(&request).contains("saved editor input"),
        "{request}"
    );
    assert!(
        user_texts(&request).contains("continued saved chat"),
        "{request}"
    );
    loaded.disconnect();
    assert!(
        resumed
            .ask("session.settings", json!({}))
            .get("error")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_load_replays_unicode_history_larger_than_the_presentation_window() {
    use kernel::EventLog;
    let world = World::new();
    let folder = world.folder("large-editor-history");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let session = viewer.open(&folder).await;
    let databases: Vec<_> = std::fs::read_dir(world.home().join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("events.db"))
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(
        databases.len(),
        1,
        "the fixture must only touch its own workspace"
    );
    let log = store::SqliteLog::open(&databases[0]).unwrap();
    let saved = kernel::Session {
        id: session.parse().unwrap(),
        ..Default::default()
    };
    let content = "你好 🪕 \"\\\n".repeat(60_000);
    let mut expected = String::new();
    for index in 0..6 {
        let text = format!("history {index}: {content}");
        log.append(kernel::Event::model_text(&saved, &text))
            .await
            .unwrap();
        expected.push_str(&text);
    }
    assert!(expected.len() > 4 * 1024 * 1024);
    drop(log);
    viewer.ask("session.close", Some(&session), json!({})).await;
    let deadline = Instant::now() + WAIT;
    loop {
        let listed = viewer.ask("session.list", None, json!({})).await;
        if listed["result"]["sessions"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "chat did not finish closing");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut editor = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut editor);
    load(&mut editor, &folder, &session, "session/load");
    assert_eq!(chunks(&editor, &session, "agent_message_chunk"), expected);
    assert!(
        editor
            .ask("session.settings", json!({}))
            .get("error")
            .is_none()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_tui_editor_and_application_client_follow_the_same_chat() {
    let world = World::new();
    let folder = world.folder("three-viewers");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut terminal = TerminalClient::start(&world, &folder);
    terminal.until("test-model");
    let listed = viewer.ask("session.list", None, json!({})).await;
    let session = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chat| chat["about"]["folder"] == json!(folder))
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_owned();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let mut editor = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut editor);
    load(&mut editor, &folder, &session, "session/load");
    let answered = editor.ask("message.send", json!({"content":"FROM_EDITOR"}));
    assert_eq!(answered["result"]["stopReason"], "end_turn");
    assert!(world.provider.asked().contains("FROM_EDITOR"));
    viewer.until(|frame| kind(frame, "turn.done")).await;
    terminal.until("echo: FROM_EDITOR");
    terminal.type_text("FROM_TERMINAL\r");
    assert!(world.provider.asked().contains("FROM_TERMINAL"));
    viewer.until(|frame| kind(frame, "turn.done")).await;
    loop {
        editor.frame();
        if chunks(&editor, &session, "agent_message_chunk").contains("echo: FROM_TERMINAL") {
            break;
        }
    }
    assert_eq!(
        chunks(&editor, &session, "user_message_chunk"),
        "FROM_TERMINAL"
    );
    editor.disconnect();
    terminal.exit();
    let listed = viewer.ask("session.list", None, json!({})).await;
    assert_eq!(
        listed["result"]["sessions"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    let result = viewer.send(&session, "application viewer remains").await;
    assert_eq!(result["result"]["accepted"], true, "{result}");
    assert!(
        world
            .provider
            .asked()
            .contains("application viewer remains")
    );
    viewer.until(|frame| kind(frame, "turn.done")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unoffered_editor_approval_is_denied_and_another_viewers_answer_wins() {
    let world = World::new();
    let folder = world.folder("invalid-approval-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut editor = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
    let session = editor.session.clone();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let prompt = editor.send(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"RUN must be denied"}]}),
    );
    let permission = loop {
        let frame = editor.frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    viewer.until(|frame| frame["method"] == "approval").await;
    writeln!(
        editor.input.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":permission["id"],
        "result":{"outcome":{"outcome":"selected","optionId":"unoffered-persistent-grant"}}})
    )
    .unwrap();
    let (_, settled) = viewer
        .until(|frame| frame["method"] == "approval.resolved")
        .await;
    assert_eq!(settled["params"]["approved"], false);
    editor.result(prompt);
    viewer.until(|frame| kind(frame, "turn.done")).await;
    assert!(!folder.join("made-by-the-test.txt").exists());

    let fresh = viewer.open(&folder).await;
    let mut following = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    initialize(&mut following);
    load(&mut following, &folder, &fresh, "session/load");
    viewer
        .send(&fresh, "RUN answered by application viewer")
        .await;
    let (_, gate) = viewer.until(|frame| frame["method"] == "approval").await;
    let pending = loop {
        let frame = following.frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    let allowed = viewer
        .ask(
            "approval.respond",
            Some(&fresh),
            json!({"gate_id":gate["params"]["gate_id"],"decision":"once"}),
        )
        .await;
    assert_eq!(allowed["result"]["accepted"], true, "{allowed}");
    viewer.until(|frame| kind(frame, "turn.done")).await;
    assert!(folder.join("made-by-the-test.txt").exists());
    // The delayed editor answer must not grant a different action or alter the
    // settled decision, even after the owner has finished the turn.
    writeln!(
        following.input.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":pending["id"],
        "result":{"outcome":{"outcome":"cancelled"}}})
    )
    .unwrap();
    assert!(
        following
            .ask("session.settings", json!({}))
            .get("error")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_approvals_are_scoped_offered_choices_and_settle_for_every_viewer() {
    let world = World::new();
    let folder = world.folder("approval-editor");
    let backend = world.backend();
    let mut viewer = backend.connect().await;
    let mut editor = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
    let session = editor.session.clone();
    viewer
        .ask("session.attach", Some(&session), json!({"after":0}))
        .await;
    let prompt = editor.send(
        "session/prompt",
        json!({"sessionId":session,
        "prompt":[{"type":"text","text":"RUN editor approval"}]}),
    );
    let permission = loop {
        let frame = editor.frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    let (_, gate) = viewer.until(|frame| frame["method"] == "approval").await;
    let once = permission["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|choice| choice["kind"] == "allow_once")
        .unwrap()["optionId"]
        .clone();
    assert_eq!(permission["params"]["sessionId"], session);
    assert!(
        permission["id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("medha|{session}|"))
    );
    writeln!(
        editor.input.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":permission["id"],
        "result":{"outcome":{"outcome":"selected","optionId":once}}})
    )
    .unwrap();
    let (_, resolved) = viewer
        .until(|frame| frame["method"] == "approval.resolved")
        .await;
    assert_eq!(resolved["params"]["gate_id"], gate["params"]["gate_id"]);
    assert_eq!(resolved["params"]["approved"], true);
    assert_eq!(editor.result(prompt)["result"]["stopReason"], "end_turn");
    viewer.until(|frame| kind(frame, "turn.done")).await;
    assert!(folder.join("made-by-the-test.txt").is_file());
    let stale = viewer
        .ask(
            "approval.respond",
            Some(&session),
            json!({"gate_id":gate["params"]["gate_id"],"decision":"once"}),
        )
        .await;
    assert!(stale.get("error").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_path_choices_match_persisted_scope_in_future_chats() {
    fn read(
        editor: &mut EditorProcess,
        path: &Path,
        mut select: impl FnMut(&Value) -> Value,
    ) -> Vec<Value> {
        let id = editor.send(
            "session/prompt",
            json!({"sessionId":editor.session,
            "prompt":[{"type":"text","text":format!("READ_PATH {}",path.display())}]}),
        );
        let mut permissions = Vec::new();
        loop {
            let frame = editor.frame();
            if frame["method"] == "session/request_permission" {
                let option = select(&frame);
                writeln!(
                    editor.input.as_mut().unwrap(),
                    "{}",
                    json!({
                        "jsonrpc":"2.0","id":frame["id"],
                        "result":{"outcome":{"outcome":"selected","optionId":option}}
                    })
                )
                .unwrap();
                permissions.push(frame);
            } else if frame["id"] == id {
                assert_eq!(frame["result"]["stopReason"], "end_turn", "{frame}");
                return permissions;
            }
        }
    }

    for scope in ["once", "path", "folder"] {
        let world = World::new();
        let folder = world.folder("path-editor");
        let outside = world.folder("outside/shared folder");
        let first = outside.join("first.txt");
        let sibling = outside.join("second.txt");
        std::fs::write(&first, "outside-first").unwrap();
        std::fs::write(&sibling, "outside-second").unwrap();
        let _backend = world.backend();
        let mut editor = EditorProcess::start(&folder, &world.home(), &world.provider, &[]);
        let file_name = format!(
            "Always allow read access to this file: {} (this project, including commands)",
            first.display()
        );
        let folder_name = format!(
            "Always allow read access to this folder and its contents: {} (this project, including commands)",
            outside.display()
        );
        let initial = read(&mut editor, &first, |frame| {
            let options = frame["params"]["options"].as_array().unwrap();
            assert!(options.iter().any(|option| option["name"] == file_name));
            assert!(options.iter().any(|option| option["name"] == folder_name));
            let name = match scope {
                "once" => "Allow once",
                "path" => &file_name,
                _ => &folder_name,
            };
            options
                .iter()
                .find(|option| option["name"] == name)
                .unwrap()["optionId"]
                .clone()
        });
        assert_eq!(initial.len(), 1);
        world.provider.seen.recv_timeout(WAIT).unwrap();
        let completed_read = world.provider.seen.recv_timeout(WAIT).unwrap();
        assert!(
            completed_read["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["content"]
                .to_string()
                .contains("outside-first"),
            "{completed_read}"
        );

        let mut grants = Vec::new();
        for project in std::fs::read_dir(world.home().join("projects")).unwrap() {
            let trust = project.unwrap().path().join("trust.lock");
            if trust.is_file() {
                let value: toml::Value =
                    toml::from_str(&std::fs::read_to_string(trust).unwrap()).unwrap();
                grants.extend(
                    value["permissions"]["trusted_paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .cloned(),
                );
            }
        }
        if scope == "once" {
            assert!(grants.is_empty(), "a once answer persisted: {grants:?}");
        } else {
            assert_eq!(grants.len(), 1);
            let target = if scope == "folder" { &outside } else { &first };
            assert_eq!(grants[0]["path"].as_str(), target.to_str());
            assert_eq!(grants[0]["permission"].as_str(), Some("Read"));
        }

        let made = editor.rpc("session/new", json!({"cwd":folder,"mcpServers":[]}));
        editor.session = made["result"]["sessionId"].as_str().unwrap().to_owned();
        for (target, expected_prompt) in [(&first, scope == "once"), (&sibling, scope != "folder")]
        {
            let asked = read(&mut editor, target, |frame| {
                frame["params"]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|option| option["kind"] == "reject_once")
                    .unwrap()["optionId"]
                    .clone()
            });
            assert_eq!(
                !asked.is_empty(),
                expected_prompt,
                "{scope} changed access to {} in a future chat",
                target.display()
            );
            world.provider.seen.recv_timeout(WAIT).unwrap();
            let request = world.provider.seen.recv_timeout(WAIT).unwrap();
            let result =
                request["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
            let content = if target == &first {
                "outside-first"
            } else {
                "outside-second"
            };
            assert_eq!(result.contains(content), !expected_prompt, "{request}");
        }
        editor.disconnect();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_cwd_validation_and_multiple_sessions_keep_provider_inputs_separate() {
    let world = World::new();
    let backend = world.backend();
    let mut observer = backend.connect().await;
    let folder = world.folder("launcher");
    let folders = [world.folder("editor-one"), world.folder("editor-two")];
    let mut editor = EditorProcess::launch(&folder, &world.home(), &world.provider, &[]);
    let before = editor.rpc("session/new", json!({"cwd":folders[0],"mcpServers":[]}));
    assert!(before.get("error").is_some());
    initialize(&mut editor);
    let relative = editor.rpc("session/new", json!({"cwd":"relative","mcpServers":[]}));
    assert!(relative.get("error").is_some());
    let mut sessions = Vec::new();
    for (index, folder) in folders.iter().enumerate() {
        let reply = editor.rpc("session/new", json!({"cwd":folder,"mcpServers":[]}));
        let session = reply["result"]["sessionId"].as_str().unwrap().to_owned();
        let prompt = editor.rpc(
            "session/prompt",
            json!({"sessionId":session,
            "prompt":[{"type":"text","text":format!("distinct editor {index}")}]}),
        );
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{prompt}");
        let body = world.provider.asked();
        assert!(body.contains(&format!("distinct editor {index}")), "{body}");
        assert!(
            !body.contains(&format!("distinct editor {}", 1 - index)),
            "cross-session input: {body}"
        );
        sessions.push(session);
    }
    let listed = observer.ask("session.list", None, json!({})).await;
    for (folder, session) in folders.iter().zip(sessions) {
        let about = listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["session"] == session)
            .unwrap();
        assert_eq!(about["about"]["folder"], json!(folder));
    }
    assert!(
        editor
            .heard
            .iter()
            .all(|frame| frame["method"] != "ready" && frame["method"] != "event")
    );
}
