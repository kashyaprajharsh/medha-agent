//! Exercise the desktop's real forms, as it asks the backend, against isolated Medha state.
use protocol::Command;
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
fn unnamed_discovered_models_have_unique_valid_names_and_both_catalogues_refresh() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("project");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut bridge = Bridge::start(&workspace, &home);
    let profile = json!({
        "protocol": "gemini-interactions",
        "base_url": "https://generativelanguage.googleapis.com/v1",
        "model": "gemini-3.8-flash",
        "auth": "x-goog-api-key",
        "max_ctx": 1048576,
    });
    for name in ["gemini-3-8-flash", "gemini-3-8-flash-2"] {
        let saved = bridge.ok(
            "settings.model.save",
            json!({"name": "", "profile": profile, "key": "dummy-gemini-key", "default": true}),
        );
        assert_eq!(saved["name"], name);
        let desktop = bridge.ok("settings.list", json!({}));
        assert!(desktop["models"].as_array().unwrap().iter().any(|model| {
            model["name"] == name
                && model["profile"]["model"] == "gemini-3.8-flash"
                && model["default"] == true
        }));
        let reply = bridge.ok(
            protocol::ReadResource::METHOD,
            json!(protocol::ReadResource::Models),
        );
        let protocol::ResourceResult::Models(catalogue) = serde_json::from_value(reply).unwrap()
        else {
            panic!("the TUI model catalogue was not returned");
        };
        assert!(catalogue.profiles.iter().any(|model| {
            model.name == name && model.model == "gemini-3.8-flash" && model.default
        }));
    }
    let config: runtime::config::Config =
        toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.default_model.as_deref(), Some("gemini-3-8-flash-2"));

    // More keys for the same server are kept in the order they were added,
    // the same key is not taken twice, and no key is ever written to settings.
    let server = "https://generativelanguage.googleapis.com/v1";
    let another = |key: &str| json!({"group": "model", "id": server, "key": key, "add": true});
    bridge.ok("settings.keys.set", another("dummy-second-key"));
    bridge.ok("settings.keys.set", another("dummy-third-key"));
    for taken in ["dummy-gemini-key", "dummy-third-key"] {
        let twice = bridge.call("settings.keys.set", another(taken));
        assert!(twice.get("error").is_some(), "{twice}");
    }
    let extra =
        |bridge: &mut Bridge| bridge.ok("settings.keys", json!({}))["models"][0]["extra"].clone();
    let both = json!([{"slot": 2, "present": true}, {"slot": 3, "present": true}]);
    assert_eq!(extra(&mut bridge), both);
    let settings = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        !settings.contains("dummy-"),
        "a key was written to settings"
    );

    bridge.ok(
        "settings.keys.remove",
        json!({"group": "model", "id": server, "slot": 2}),
    );
    assert_eq!(extra(&mut bridge), json!([{"slot": 3, "present": true}]));
    let reply = bridge.ok(
        protocol::ReadResource::METHOD,
        json!(protocol::ReadResource::Models),
    );
    let protocol::ResourceResult::Models(catalogue) = serde_json::from_value(reply).unwrap() else {
        panic!("the TUI model catalogue was not returned");
    };
    assert_eq!(
        catalogue.profiles[0].extra_keys,
        [3],
        "the terminal sees the same keys"
    );
}

#[test]
fn of_several_saves_made_from_the_same_text_one_is_kept_and_the_rest_are_told() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    // Two Medhas with homes of their own edit the one project.
    let homes = [root.path().join("home-a"), root.path().join("home-b")];
    let _backends = homes
        .each_ref()
        .map(|home| backend::Backend::start(home, &[]));

    for round in 0..5 {
        let before = std::fs::read_to_string(workspace.join("MEDHA.md")).unwrap_or_default();
        let together = std::sync::Arc::new(std::sync::Barrier::new(4));
        let editors: Vec<_> = (0..4)
            .map(|editor| {
                let (workspace, before, together) =
                    (workspace.clone(), before.clone(), together.clone());
                let mut client = backend::Backend::join(&homes[editor % 2]);
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
    assert!(!settings["catalogue_revision"].as_str().unwrap().is_empty());
    assert!(
        settings["unavailable_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "anthropic" && row["reason"].is_string())
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
    let google = gemini["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == "google")
        .unwrap();
    assert_eq!(google["auth"], "x-goog-api-key");
    assert_eq!(
        google["url"],
        "https://generativelanguage.googleapis.com/v1"
    );
    assert!(
        gemini["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row["name"].as_str().unwrap().contains("Ollama"))
    );
    let anthropic = protocols
        .iter()
        .find(|row| row["value"] == "anthropic-messages")
        .unwrap();
    assert_eq!(
        (&anthropic["available"], &anthropic["discovery"]),
        (&json!(false), &json!(false))
    );
    // A pasted key is read as it would be saved; a known but unsupported provider says why.
    let pasted = bridge.ok(
        "settings.model.recognise_key",
        json!({"key": "Bearer sk-ant-api03-abc"}),
    );
    assert_eq!(pasted["match"], "one");
    assert_eq!(
        (
            &pasted["providers"][0]["id"],
            &pasted["providers"][0]["available"]
        ),
        (&json!("anthropic"), &json!(false))
    );
    assert!(pasted["providers"][0]["reason"].is_string());
    let admin = bridge.ok(
        "settings.model.recognise_key",
        json!({"key": "sk-admin-abc"}),
    );
    assert_eq!(admin["providers"][0]["id"], "openai");
    assert!(admin["warning"].is_string());
    // New OpenAI setups use Responses; it lists models like Chat.
    let responses = protocols
        .iter()
        .find(|row| row["value"] == "open-ai-responses")
        .unwrap();
    assert_eq!(
        (&responses["available"], &responses["discovery"]),
        (&json!(true), &json!(true))
    );
    let openai = responses["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "openai");
    assert_eq!(openai.unwrap()["url"], "https://api.openai.com/v1");

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
        // An accepted socket can inherit the listener's non-blocking mode.
        socket.set_nonblocking(false).unwrap();
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
    // A stand-in must be a saved model, and leaves the list when it is removed.
    let unknown = bridge.call("settings.fallback.save", json!({"names":["nobody"]}));
    assert!(unknown.get("error").is_some(), "{unknown}");
    bridge.ok(
        "settings.fallback.save",
        json!({"names":["local-test","local-test"]}),
    );
    let listed = bridge.ok("settings.list", json!({}));
    assert_eq!(listed["fallback"], json!(["local-test"]));
    bridge.ok("settings.model.remove", json!({"name":"local-test"}));
    let emptied = bridge.ok("settings.list", json!({}));
    assert!(emptied["models"].as_array().unwrap().is_empty());
    assert_eq!(emptied["fallback"], json!([]));
}
