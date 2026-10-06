//! Exercise the desktop's real forms, as it asks the backend, against isolated Medha state.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "common/backend.rs"]
mod backend;

struct Bridge {
    backend: backend::Backend,
    workspace: PathBuf,
}
impl Bridge {
    fn start(workspace: &Path, home: &Path) -> Self {
        Self {
            backend: backend::Backend::start(home, &[]),
            workspace: workspace.to_path_buf(),
        }
    }
    /// The reply as a form reads it: its result, or the error it shows.
    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.backend.ask(&self.workspace, method, params) {
            Ok(result) => json!({ "result": result }),
            Err(error) => json!({ "error": error }),
        }
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }
}
#[test]
fn of_several_saves_made_from_the_same_text_one_is_kept_and_the_rest_are_told() {
    let root = tempfile::tempdir().unwrap();
    let (workspace, home) = (root.path().join("project"), root.path().join("home"));
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let _backend = backend::Backend::start(&home, &[]);

    for round in 0..5 {
        let before = std::fs::read_to_string(workspace.join("MEDHA.md")).unwrap_or_default();
        let together = std::sync::Arc::new(std::sync::Barrier::new(4));
        let editors: Vec<_> = (0..4)
            .map(|editor| {
                let (workspace, before, together) =
                    (workspace.clone(), before.clone(), together.clone());
                let mut client = backend::Backend::join(&home);
                std::thread::spawn(move || {
                    let content = format!("round {round}, editor {editor}");
                    let save = json!({"kind": "project", "before": before, "content": content});
                    together.wait();
                    client
                        .ask(&workspace, "instructions.save", save)
                        .map(|_| content)
                })
            })
            .collect();
        let outcomes: Vec<_> = editors
            .into_iter()
            .map(|editor| editor.join().unwrap())
            .collect();
        let kept: Vec<_> = outcomes.iter().flatten().collect();
        assert_eq!(kept.len(), 1, "round {round}: {outcomes:?}");
        let written = std::fs::read_to_string(workspace.join("MEDHA.md")).unwrap();
        assert_eq!(&written, kept[0], "the file is not the save that was kept");
    }
}

#[test]
fn desktop_forms_share_model_limits_instructions_and_skill_state_with_medha() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("project");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut bridge = Bridge::start(&workspace, &home);
    let profile = json!({"protocol":"open-ai-chat","base_url":"http://127.0.0.1:11434/v1","model":"local-test","auth":"none","max_ctx":32768,"max_output_tokens":4096,"image_input":"native"});
    bridge.ok(
        "settings.model.save",
        json!({"name":"local-test","profile":profile,"default":true}),
    );
    let settings = bridge.ok("settings.list", json!({}));
    assert_eq!(settings["models"][0]["profile"]["max_ctx"], 32768);
    assert_eq!(settings["models"][0]["profile"]["max_output_tokens"], 4096);
    assert_eq!(settings["models"][0]["default"], true);
    assert!(settings["models"][0].get("key").is_none());
    assert!(
        settings["presets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Ollama (local)")
    );
    let protocols = settings["protocols"].as_array().unwrap();
    for option in protocols {
        // Picker values must deserialize as the actual shared protocol enum.
        let _: kernel::Protocol = serde_json::from_value(option["value"].clone()).unwrap();
    }
    let chat = protocols
        .iter()
        .find(|row| row["value"] == "open-ai-chat")
        .unwrap();
    assert_eq!(chat["available"], true);
    assert_eq!(chat["discovery"], true);
    assert!(
        chat["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Ollama (local)" && row["auth"] == "none")
    );
    let gemini = protocols
        .iter()
        .find(|row| row["value"] == "gemini-interactions")
        .unwrap();
    assert_eq!(gemini["providers"][0]["name"], "Google Gemini");
    assert_eq!(gemini["providers"][0]["auth"], "x-goog-api-key");
    assert!(
        gemini["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row["name"].as_str().unwrap().contains("Ollama"))
    );
    for value in ["anthropic-messages", "open-ai-responses"] {
        let option = protocols.iter().find(|row| row["value"] == value).unwrap();
        assert_eq!(option["available"], false);
        assert_eq!(option["discovery"], false);
    }

    // Exercise discovery over the real desktop bridge using its picker value.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "discovery did not reach the server"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("discovery server: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = BufReader::new(socket.try_clone().unwrap());
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        assert!(line.starts_with("GET /v1/models "), "{line}");
        loop {
            line.clear();
            assert!(input.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        let body = r#"{"data":[{"id":"discovered-local-model"}]}"#;
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let found = bridge.ok("settings.model.discover", json!({"profile": {
        "protocol": chat["value"], "base_url": endpoint, "model": "model-discovery", "auth": "none"
    }}));
    assert_eq!(found["models"][0]["id"], "discovered-local-model");
    server.join().unwrap();
    let defaults = bridge.ok("settings.defaults", json!({}));
    assert_eq!(defaults["profile"], "local-test");
    let mut invalid = profile.clone();
    invalid["max_ctx"] = json!(0);
    assert!(
        bridge
            .call(
                "settings.model.save",
                json!({"name":"bad","profile":invalid})
            )
            .get("error")
            .is_some()
    );
    bridge.ok(
        "instructions.save",
        json!({"kind":"agents","before":"","content":"Use readable tools."}),
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
        "Use readable tools."
    );
    assert!(
        bridge
            .call(
                "instructions.save",
                json!({"kind":"agents","before":"","content":"overwrite"})
            )
            .get("error")
            .is_some()
    );
    assert!(
        bridge
            .call(
                "instructions.save",
                json!({"kind":"../../escape","content":"no"})
            )
            .get("error")
            .is_some()
    );
    let source = root.path().join("skill");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"),"---\nname: simple-test\ndescription: Read source before editing\n---\n\nRead the source and explain it.\n").unwrap();
    bridge.ok("extensions.skill.install", json!({"source":source}));
    bridge.ok(
        "extensions.skill.configure",
        json!({"name":"simple-test","enabled":false}),
    );
    let catalog = bridge.ok("extensions.list", json!({}));
    assert_eq!(catalog["skills"][0]["enabled"], false);
    assert!(home.join("skills/simple-test/SKILL.disabled").exists());
    bridge.ok(
        "extensions.skill.configure",
        json!({"name":"simple-test","enabled":true}),
    );
    assert!(!home.join("skills/simple-test/SKILL.disabled").exists());
    bridge.ok("settings.model.remove", json!({"name":"local-test"}));
    assert!(
        bridge.ok("settings.list", json!({}))["models"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
