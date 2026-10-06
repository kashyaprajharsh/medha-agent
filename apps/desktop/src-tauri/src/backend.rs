//! The desktop's one connection to the Medha backend. Every chat and every
//! question about a folder goes over it. A backend already running is joined;
//! otherwise one is started.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};

const ROLES: wire::Roles = wire::Roles {
    host: "backend",
    guest: "client",
};
const PROTOCOL: u64 = 1;
/// The system may check a new binary before it runs, which holds a first start up.
const START: Duration = Duration::from_secs(30);
/// How long a start that has already exited is still given to be found running.
const AFTER_EXIT: Duration = Duration::from_secs(3);
/// How long a chat that is ending is waited for before it is resumed.
const LEAVING: Duration = Duration::from_secs(10);
const STOPPED: &str = "Medha backend stopped unexpectedly";
pub(crate) const OPEN_ELSEWHERE: &str =
    "This chat is open in another Medha window. Close it there to continue it here.";

/// What a chat is told: a frame it wrote or was answered with, or that it is over.
pub(crate) enum Said {
    Frame(Value),
    Ended(Option<String>),
}

pub(crate) type Hear = Arc<dyn Fn(Said) + Send + Sync>;

/// A chat as it was opened here. One resumed later under the same id is another chat.
pub(crate) struct Chat {
    session: String,
    opened: u64,
}

enum Reply {
    Wait(mpsc::SyncSender<Result<Value, String>>),
    /// A chat's own request: the answer joins its frames under the id it used.
    Chat {
        session: String,
        opened: u64,
        id: Value,
    },
}

#[derive(Default)]
struct Routes {
    next: u64,
    replies: HashMap<u64, Reply>,
    chats: HashMap<String, (u64, Hear)>,
    /// Chats of this window's own that it told to stop or heard end, until one is opened again.
    over: HashMap<String, Instant>,
    gone: bool,
}

impl Routes {
    /// Kept only while the backend may still be letting the chat go.
    fn is_over(&mut self, session: String) {
        self.over.retain(|_, since| since.elapsed() < LEAVING);
        self.over.insert(session, Instant::now());
    }

    fn hearing(&self, session: &str, opened: Option<u64>) -> Option<Hear> {
        self.chats
            .get(session)
            .filter(|(number, _)| opened.is_none_or(|opened| opened == *number))
            .map(|(_, hear)| Arc::clone(hear))
    }
}

pub(crate) struct Connection {
    lines: tokio::sync::mpsc::UnboundedSender<String>,
    routes: Mutex<Routes>,
}

fn outcome(mut frame: Value) -> Result<Value, String> {
    match frame["error"]["message"].as_str() {
        Some(error) => Err(error.to_owned()),
        None => Ok(frame["result"].take()),
    }
}

impl Connection {
    fn routes(&self) -> MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ids are given and frames queued under one lock, so they leave in the order they were made.
    fn post(
        &self,
        routes: &mut Routes,
        mut frame: Value,
        reply: Option<Reply>,
    ) -> Result<(), String> {
        if routes.gone {
            return Err(STOPPED.into());
        }
        if let Some(reply) = reply {
            routes.next += 1;
            frame["id"] = json!(routes.next);
            routes.replies.insert(routes.next, reply);
        }
        self.lines
            .send(format!("{frame}\n"))
            .map_err(|_| STOPPED.to_string())
    }

    fn ask(&self, frame: Value) -> Result<Value, String> {
        let (answer, answered) = mpsc::sync_channel(1);
        self.post(&mut self.routes(), frame, Some(Reply::Wait(answer)))?;
        answered.recv().unwrap_or_else(|_| Err(STOPPED.into()))
    }

    /// One question about a folder: its history, settings or extensions.
    pub(crate) fn about(&self, folder: &Path, mut request: Value) -> Result<Value, String> {
        request["folder"] = json!(folder);
        self.ask(request)
    }

    fn is_live(&self, session: &str) -> bool {
        self.ask(json!({ "method": "session.list" }))
            .is_ok_and(|listed| {
                listed["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|live| live["session"] == session)
            })
    }

    /// Starts a chat, or resumes one, and hears everything it says from its
    /// first frame. The chat ends with this connection, as one on a pipe did.
    ///
    /// `leaving` says the chat being resumed is this window's own and already
    /// on its way out, because it slept or was just closed: its end is waited
    /// for. A chat that is live for any other reason is someone's, and is left alone.
    pub(crate) fn open_chat(
        &self,
        folder: &Path,
        resume: Option<&str>,
        leaving: bool,
        settings: Option<&Value>,
        hear: Hear,
    ) -> Result<Chat, String> {
        let mut params = json!({ "folder": folder, "ends_with_client": true });
        if let Some(id) = resume {
            params["resume"] = json!(id);
        }
        if let Some(settings) = settings {
            params["settings"] = settings.clone();
        }
        let deadline = Instant::now() + LEAVING;
        let made = loop {
            let error = match self.ask(json!({ "method": "session.create", "params": params })) {
                Ok(made) => break made,
                Err(error) => error,
            };
            if !resume.is_some_and(|id| self.is_live(id)) {
                return Err(error);
            }
            let ours = leaving || resume.is_some_and(|id| self.routes().over.contains_key(id));
            if !ours || Instant::now() >= deadline {
                return Err(OPEN_ELSEWHERE.into());
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let session = made["session"]
            .as_str()
            .ok_or("Medha backend did not name the chat")?
            .to_owned();
        let opened = {
            let mut routes = self.routes();
            routes.next += 1;
            let opened = routes.next;
            routes.chats.insert(session.clone(), (opened, hear));
            routes.over.remove(&session);
            opened
        };
        let attach =
            json!({ "method": "session.attach", "session": session, "params": { "after": 0 } });
        if let Err(error) = self.ask(attach) {
            self.routes().chats.remove(&session);
            return Err(error);
        }
        Ok(Chat { session, opened })
    }

    /// A chat's own request; one with an id is answered among the chat's frames.
    /// Nothing is sent once the chat is over, so it never reaches a later chat of the same id.
    pub(crate) fn tell(&self, chat: &Chat, mut frame: Value) -> Result<(), String> {
        let mut routes = self.routes();
        if routes.hearing(&chat.session, Some(chat.opened)).is_none() {
            return Err(STOPPED.into());
        }
        let reply = frame.get("id").cloned().map(|id| Reply::Chat {
            session: chat.session.clone(),
            opened: chat.opened,
            id,
        });
        if matches!(frame["method"].as_str(), Some("session.close" | "shutdown")) {
            routes.is_over(chat.session.clone());
        }
        frame["session"] = json!(chat.session);
        self.post(&mut routes, frame, reply)
    }

    fn heard(&self, mut frame: Value) {
        if frame["method"] == "session.event" {
            let Some(session) = frame["params"]["session"].as_str().map(str::to_owned) else {
                return;
            };
            let said = frame["params"]["frame"].take();
            if said["method"] == "session.ended" {
                let ended = {
                    let mut routes = self.routes();
                    let ended = routes.chats.remove(&session);
                    if ended.is_some() {
                        routes.is_over(session);
                    }
                    ended
                };
                let reason = said["params"]["error"].as_str().map(str::to_owned);
                if let Some((_, hear)) = ended {
                    hear(Said::Ended(reason));
                }
            } else {
                let hear = self.routes().hearing(&session, None);
                if let Some(hear) = hear {
                    hear(Said::Frame(said));
                }
            }
            return;
        }
        let Some(id) = frame["id"].as_u64() else {
            return;
        };
        let reply = self.routes().replies.remove(&id);
        match reply {
            Some(Reply::Wait(answer)) => {
                let _ = answer.send(outcome(frame));
            }
            Some(Reply::Chat {
                session,
                opened,
                id,
            }) => {
                frame["id"] = id;
                let hear = self.routes().hearing(&session, Some(opened));
                if let Some(hear) = hear {
                    hear(Said::Frame(frame));
                }
            }
            None => {}
        }
    }

    /// Whoever waits for an answer is released, and every chat is told it is over.
    fn lost(&self) {
        let (replies, chats) = {
            let mut routes = self.routes();
            routes.gone = true;
            (
                std::mem::take(&mut routes.replies),
                std::mem::take(&mut routes.chats),
            )
        };
        drop(replies);
        for (_, hear) in chats.into_values() {
            hear(Said::Ended(Some("The Medha backend stopped.".into())));
        }
    }
}

async fn read(connection: Arc<Connection>, mut reader: impl AsyncBufRead + Unpin) {
    while let Some(frame) = wire::read_frame(&mut reader).await {
        connection.heard(frame);
    }
    connection.lost();
}

enum Refused {
    Absent,
    Incompatible,
}

/// The connection is read on a thread of its own. Whoever waits for an answer
/// blocks, and some wait on the app's own runtime: read there, the answer could
/// queue behind the very thread that is waiting for it.
fn reading() -> &'static tokio::runtime::Runtime {
    static READING: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    READING.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("medha-backend")
            .enable_all()
            .build()
            .expect("a thread to read the backend on")
    })
}

async fn join(address: String, token: String) -> Result<Arc<Connection>, Refused> {
    let stream = wire::connect(&address).await.map_err(|_| Refused::Absent)?;
    let (reading, mut writing) = tokio::io::split(stream);
    let mut reader = BufReader::new(reading);
    let welcome = wire::greet(&mut reader, &mut writing, &token, ROLES)
        .await
        .map_err(|_| Refused::Absent)?;
    if welcome["result"]["protocol"] != PROTOCOL {
        return Err(Refused::Incompatible);
    }
    let (lines, mut queued) = tokio::sync::mpsc::unbounded_channel::<String>();
    let connection = Arc::new(Connection {
        lines,
        routes: Mutex::default(),
    });
    tokio::spawn(async move {
        while let Some(line) = queued.recv().await {
            if writing.write_all(line.as_bytes()).await.is_err() || writing.flush().await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(read(Arc::clone(&connection), reader));
    Ok(connection)
}

#[derive(Default)]
struct Link {
    connection: Option<Arc<Connection>>,
    started: Option<Child>,
}

pub(crate) struct Backend {
    executable: Option<PathBuf>,
    env: Vec<(String, String)>,
    /// One started for a test is stopped with it; the app's own is left for whoever comes next.
    owned: bool,
    link: Mutex<Link>,
}

impl Backend {
    pub(crate) fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<Backend>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(Self {
                executable: None,
                env: Vec::new(),
                owned: false,
                link: Mutex::default(),
            })
        }))
    }

    #[cfg(test)]
    pub(crate) fn owned(executable: PathBuf, env: Vec<(String, String)>) -> Arc<Self> {
        Arc::new(Self {
            executable: Some(executable),
            env,
            owned: true,
            link: Mutex::default(),
        })
    }

    /// A test stops the backend it started; nothing else ever would.
    #[cfg(test)]
    pub(crate) fn stop(&self) {
        let mut link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut child) = link.started.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Where the backend says how to reach it, as the backend itself works it out.
    fn directory(&self) -> Result<PathBuf, String> {
        let named = self
            .env
            .iter()
            .find(|(key, _)| key == "MEDHA_HOME")
            .map(|(_, home)| PathBuf::from(home))
            .or_else(|| std::env::var_os("MEDHA_HOME").map(PathBuf::from));
        let home = match named {
            Some(home) => home,
            None => dirs::home_dir()
                .ok_or("Could not find your home folder")?
                .join(".medha"),
        };
        Ok(home.join("serve"))
    }

    fn reach(&self, directory: &Path) -> Result<Arc<Connection>, Refused> {
        let read = |name: &str| std::fs::read_to_string(directory.join(name));
        let (Ok(address), Ok(token)) = (read("address"), read("token")) else {
            return Err(Refused::Absent);
        };
        let (joined, outcome) = mpsc::sync_channel(1);
        reading().spawn(async move {
            let _ = joined.send(join(address, token).await);
        });
        outcome.recv().unwrap_or(Err(Refused::Absent))
    }

    fn start(&self) -> Result<Child, String> {
        let executable = match &self.executable {
            Some(executable) => executable.clone(),
            None => executable()?,
        };
        let mut command = Command::new(executable);
        command
            .arg("serve")
            .envs(self.env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        inherit_nothing(&mut command);
        if !self.owned {
            outlive_the_app(&mut command);
        }
        command
            .spawn()
            .map_err(|error| format!("Could not start Medha backend: {error}"))
    }

    /// The live connection, joining a running backend or starting one first.
    pub(crate) fn connection(&self) -> Result<Arc<Connection>, String> {
        let incompatible = || "Medha backend protocol version is incompatible".to_string();
        let mut link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(live) = link.connection.as_ref().filter(|live| !live.routes().gone) {
            return Ok(Arc::clone(live));
        }
        let directory = self.directory()?;
        let mut started: Option<Child> = None;
        let mut exited: Option<Instant> = None;
        let deadline = Instant::now() + START;
        let connection = loop {
            match self.reach(&directory) {
                Ok(connection) => break connection,
                Err(Refused::Incompatible) => return Err(incompatible()),
                Err(Refused::Absent) => {}
            }
            match started.as_mut() {
                None => started = Some(self.start()?),
                // One that lost the race to start exits at once; the winner is found on the next look.
                Some(child) => {
                    if exited.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                        exited = Some(Instant::now());
                    }
                }
            }
            let gave_up = exited.is_some_and(|at| at.elapsed() >= AFTER_EXIT);
            if gave_up || Instant::now() >= deadline {
                return Err(
                    "Could not start Medha backend. See logs/serve.log in Medha's folder.".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        match started {
            Some(child) if self.owned => link.started = Some(child),
            // Waited for so that it does not linger as a finished process.
            Some(mut child) => drop(std::thread::spawn(move || child.wait())),
            None => {}
        }
        link.connection = Some(Arc::clone(&connection));
        Ok(connection)
    }
}

/// A terminal being opened at this moment is not yet marked to stay behind. A
/// backend that took its end would never read it, and closing that terminal
/// would hang its shell.
#[cfg(unix)]
fn inherit_nothing(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let known = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0;
    let highest = if known {
        limit.rlim_cur.min(65_536)
    } else {
        1024
    } as libc::c_int;
    // Only `close` runs between fork and exec, which is safe there.
    unsafe {
        command.pre_exec(move || {
            for fd in 3..highest {
                libc::close(fd);
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn inherit_nothing(_command: &mut Command) {}

#[cfg(unix)]
fn outlive_the_app(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn outlive_the_app(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

pub fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MEDHA_DESKTOP_BACKEND") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) { "medha.exe" } else { "medha" };
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .filter(|path| path.is_file())
    {
        return Ok(path);
    }
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!(
            "medha-{}{}",
            env!("TAURI_ENV_TARGET_TRIPLE"),
            suffix
        ));
    binary
        .is_file()
        .then_some(binary)
        .ok_or_else(|| "Medha backend is missing. Run npm run prepare:backend.".into())
}
