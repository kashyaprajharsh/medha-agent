//! Messaging setup through a real backend and a stand-in for Medha Setup: no answer carries a
//! token, only the newest setup moves on, and nothing is saved before the owner is confirmed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::Scope;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);
const TOKEN: &str = "424242:the-saved-bot-token";

/// Medha Setup that hands out pairings and never sees a bot made.
fn setup_service(stream: TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut stream = stream;
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&chunk[..read]),
        }
    }
    let (status, body) = if request.starts_with(b"POST /v1/telegram/pairings") {
        let id = format!("pairing-{}", std::process::id());
        (
            "201 Created",
            json!({"pairing_id": id, "poll_token": "poll", "link": "https://t.me/MedhaSetupBot?start=code", "expires_in": 600}),
        )
    } else {
        ("202 Accepted", json!({"status": "waiting"}))
    };
    let body = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

struct Serving(std::process::Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct World {
    _root: tempfile::TempDir,
    _serving: Serving,
    client: std::sync::Arc<medha_client::Backend>,
    folder: PathBuf,
}

impl World {
    /// A home where Telegram was already set up by hand, so its token is in the credential file.
    fn start() -> Self {
        let root = tempfile::Builder::new().prefix("ms").tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let service = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || setup_service(stream));
            }
        });
        let home = root.path().join("home");
        let folder = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[gateway]\nfolder = {}\n\n[gateway.telegram]\nenabled = true\naccount = \"@saved_bot\"\n",
                toml_string(&folder)
            ),
        )
        .unwrap();
        std::fs::write(
            home.join("credentials.toml"),
            format!("[keys]\n\"messaging://telegram\" = \"{TOKEN}\"\n"),
        )
        .unwrap();
        let serving = Serving(
            std::process::Command::new(env!("CARGO_BIN_EXE_medha"))
                .arg("serve")
                .env("MEDHA_HOME", &home)
                .env("MEDHA_CRED_STORE", "file")
                .env("MEDHA_CATALOGUE_OFFLINE", "1")
                .env("MEDHA_SETUP_URL", service)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let read = |name: &str| {
            std::fs::read_to_string(home.join("serve").join(name))
                .ok()
                .filter(|text| !text.is_empty())
        };
        let deadline = std::time::Instant::now() + WAIT;
        let (address, token) = loop {
            if let (Some(address), Some(token)) = (read("address"), read("token")) {
                break (address, token);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the backend never listened"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        Self {
            client: medha_client::Backend::joined_to(address, &token),
            _root: root,
            _serving: serving,
            folder,
        }
    }

    fn call<T: protocol::Command>(&self, params: &T) -> Result<T::Output, String> {
        self.client
            .connection()?
            .call(Scope::Folder(&self.folder), params)
    }

    /// Every answer as the client received it, to look for the token in.
    fn raw(&self, method: &str, params: Value) -> String {
        let frame = json!({"method": method, "params": params, "folder": self.folder});
        format!(
            "{:?}",
            self.client.connection().unwrap().about(&self.folder, frame)
        )
    }
}

fn toml_string(path: &Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

#[test]
fn no_answer_carries_the_saved_token() {
    let world = World::start();
    let overview = world.call(&protocol::MessagingStatus {}).unwrap();
    let telegram = overview
        .platforms
        .iter()
        .find(|platform| platform.name == "telegram")
        .unwrap();
    assert!(telegram.ready, "a saved token makes the platform ready");
    assert_eq!(telegram.account.as_deref(), Some("@saved_bot"));
    assert_eq!(overview.folder.as_deref(), Some(world.folder.as_path()));
    for (method, params) in [
        ("application.messaging.status", json!({})),
        ("settings.messaging.status", json!({})),
        (
            "application.messaging.begin",
            json!({"platform": "telegram"}),
        ),
        ("settings.messaging.save", json!({"id": 1})),
    ] {
        let answer = world.raw(method, params);
        // An error answer carries no token either, so each must have said what it is for.
        let said = ["@saved_bot", "qr_svg", "first message"];
        assert!(
            said.iter().any(|word| answer.contains(word)),
            "{method}: {answer}"
        );
        assert!(!answer.contains(TOKEN), "{method} answered with the token");
        assert!(
            !answer.contains("the-saved-bot-token"),
            "{method} answered part of the token"
        );
    }
}

#[test]
fn only_the_newest_setup_moves_on_and_nothing_is_saved_before_the_owner_is_confirmed() {
    let world = World::start();
    let begin = protocol::BeginMessagingSetup {
        platform: "telegram".into(),
    };
    let first = world.call(&begin).unwrap();
    let second = world.call(&begin).unwrap();
    assert!(second.id > first.id);
    assert!(second.qr_svg.starts_with("<?xml") && second.qr_svg.contains("<svg"));
    assert!(!second.qr_text.is_empty());

    let replaced = world
        .call(&protocol::AwaitMessagingSetup { id: first.id })
        .unwrap_err();
    assert!(replaced.contains("newer setup"), "{replaced}");

    let saved = world
        .call(&protocol::SaveMessagingSetup {
            id: second.id,
            folder: None,
        })
        .unwrap_err();
    assert!(saved.contains("first message"), "{saved}");
    // Refusing to save kept the setup: it still waits for the bot.
    assert!(matches!(
        world
            .call(&protocol::AwaitMessagingSetup { id: second.id })
            .unwrap(),
        protocol::MessagingStep::Waiting { .. }
    ));

    world
        .call(&protocol::CancelMessagingSetup { id: second.id })
        .unwrap();
    let ended = world
        .call(&protocol::AwaitMessagingSetup { id: second.id })
        .unwrap_err();
    assert!(ended.contains("ended"), "{ended}");
}

#[test]
fn folders_for_the_phone_are_projects_and_the_default_cannot_be_removed() {
    let world = World::start();
    let change = |change, folder: &Path| {
        world.call(&protocol::ChangeMessagingFolder {
            change,
            folder: folder.to_path_buf(),
        })
    };
    let home = dirs::home_dir().unwrap();
    let refused = change(protocol::FolderChange::Add, &home).unwrap_err();
    assert!(refused.contains("more than one project"), "{refused}");

    let site = world.folder.parent().unwrap().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let site = site.canonicalize().unwrap();
    let added = change(protocol::FolderChange::Add, &site).unwrap();
    assert_eq!(added.folders, [world.folder.clone(), site.clone()]);

    let refused = change(protocol::FolderChange::Remove, &world.folder).unwrap_err();
    assert!(
        refused.contains("Make another folder the default"),
        "{refused}"
    );

    // A new default keeps the old one open to chats that chose it.
    let moved = change(protocol::FolderChange::Default, &site).unwrap();
    assert_eq!(moved.folder.as_deref(), Some(site.as_path()));
    assert_eq!(moved.folders, [site.clone(), world.folder.clone()]);
    let removed = change(protocol::FolderChange::Remove, &world.folder).unwrap();
    assert_eq!(removed.folders, [site]);
}

#[test]
fn tool_steps_stay_off_until_chosen_and_another_change_keeps_the_choice() {
    let world = World::start();
    let status = world.call(&protocol::MessagingStatus {}).unwrap();
    assert!(!status.steps, "steps are off until chosen");
    let shown = world
        .call(&protocol::ShowMessagingSteps { show: true })
        .unwrap();
    assert!(shown.steps);
    let site = world.folder.parent().unwrap().join("site");
    std::fs::create_dir_all(&site).unwrap();
    let added = world
        .call(&protocol::ChangeMessagingFolder {
            change: protocol::FolderChange::Add,
            folder: site,
        })
        .unwrap();
    assert!(added.steps, "a folder change kept the choice");
    let hidden = world
        .call(&protocol::ShowMessagingSteps { show: false })
        .unwrap();
    assert!(!hidden.steps);
}

#[test]
fn only_telegram_is_made_by_scanning_a_code() {
    let world = World::start();
    let refused = world
        .call(&protocol::BeginMessagingSetup {
            platform: "discord".into(),
        })
        .unwrap_err();
    assert!(refused.contains("Only Telegram"), "{refused}");
}
