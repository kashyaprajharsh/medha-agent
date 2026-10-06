//! The desktop's one connection to the Medha backend. Every chat and every
//! question about a folder goes over it. A backend already running is joined;
//! otherwise one is started.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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
const QUIET: &str = "Medha did not answer in time. Try again.";
pub(crate) const NOT_TAKING: &str =
    "Medha has not taken what was already sent. Try again in a moment.";
pub(crate) const TOO_LARGE: &str =
    "That is too much to send at once. Send fewer or smaller attachments.";
const UNSENT_BYTES: usize = 64 * 1024 * 1024;
/// How many requests may be waiting for their answers at once, over every chat and folder.
const UNANSWERED: usize = 1024;
/// What stops a chat or lets go of one must arrive when nothing else is taken,
/// so it has a little room of its own past those limits, and no more than that.
const STOP_BYTES: usize = 4 * 1024;
const STOPS_UNSENT: usize = 1024 * 1024;
const STOPS_UNANSWERED: usize = 64;
/// How many chat starts whose wait was given up are still remembered, to stop them if they come.
const LATE: usize = 64;
/// What naming the chat and numbering the request add to a frame on its way out.
pub(crate) const WRAPPER: usize = 128;
/// How long a quick answer is waited for, and one that may install, download or start a chat.
const SOON: Duration = Duration::from_secs(60);
const EVENTUALLY: Duration = Duration::from_secs(10 * 60);
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
        since: Instant,
    },
}

#[derive(Default)]
struct Routes {
    next: u64,
    /// Requests somebody still waits on. One whose wait ended, or whose chat did, is not kept.
    replies: HashMap<u64, Reply>,
    /// Chat starts nobody waits on any more.
    late: HashMap<u64, Instant>,
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

    /// A chat that is no longer heard takes its unanswered requests with it.
    fn forget(&mut self, session: &str, opened: u64) -> Option<Hear> {
        let (_, hear) = self.chats.remove(session).filter(|(n, _)| *n == opened)?;
        self.replies.retain(|_, reply| {
            !matches!(reply, Reply::Chat { session: of, opened: n, .. } if of == session && *n == opened)
        });
        Some(hear)
    }

    /// Whether every place for an unanswered request is taken. A chat's request
    /// waits as long as anything asked of the backend may; when room is needed,
    /// one older than `waited` gives its place up, and its answer has nobody.
    fn is_full(&mut self, places: usize, waited: Duration) -> bool {
        if self.replies.len() >= places {
            self.replies.retain(
                |_, reply| !matches!(reply, Reply::Chat { since, .. } if since.elapsed() >= waited),
            );
        }
        self.replies.len() >= places
    }

    fn gave_up(&mut self, id: u64) {
        if self.late.len() >= LATE
            && let Some(oldest) = self
                .late
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(id, _)| *id)
        {
            self.late.remove(&oldest);
        }
        self.late.insert(id, Instant::now());
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
    /// Bytes handed to `lines` that the backend has not taken yet.
    unsent: Arc<AtomicUsize>,
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
    ) -> Result<u64, String> {
        if routes.gone {
            return Err(STOPPED.into());
        }
        let id = routes.next + 1;
        if reply.is_some() {
            frame["id"] = json!(id);
        }
        // Measured as it leaves: a frame the backend could not read would end
        // the connection, and with it every chat on it.
        let line = format!("{frame}\n");
        if line.len() > wire::MAX_FRAME {
            return Err(TOO_LARGE.into());
        }
        let stops = line.len() <= STOP_BYTES
            && matches!(
                frame["method"].as_str(),
                Some("session.close" | "session.detach" | "shutdown" | "cancel" | "interrupt")
            );
        let (bytes, places) = match stops {
            true => (UNSENT_BYTES + STOPS_UNSENT, UNANSWERED + STOPS_UNANSWERED),
            false => (UNSENT_BYTES, UNANSWERED),
        };
        let unsent = self.unsent.load(Ordering::Relaxed) + line.len() > bytes;
        if unsent || (reply.is_some() && routes.is_full(places, EVENTUALLY)) {
            return Err(NOT_TAKING.into());
        }
        if let Some(reply) = reply {
            routes.next = id;
            routes.replies.insert(id, reply);
        }
        self.unsent.fetch_add(line.len(), Ordering::Relaxed);
        self.lines
            .send(line)
            .map(|()| routes.next)
            .map_err(|_| STOPPED.to_string())
    }

    /// Whether a frame of this size would be taken now, without making it to find out.
    pub(crate) fn has_room(&self, bytes: usize) -> bool {
        let mut routes = self.routes();
        let unsent = self.unsent.load(Ordering::Relaxed) + bytes > UNSENT_BYTES;
        routes.gone || !(unsent || routes.is_full(UNANSWERED, EVENTUALLY))
    }

    /// Nothing waits without end on a backend that has gone quiet, and a wait
    /// that was given up holds no place. Only a chat's start is remembered past
    /// it, so a chat that starts after all is not left running.
    fn ask(&self, frame: Value, within: Duration) -> Result<Value, String> {
        let (answer, answered) = mpsc::sync_channel(1);
        let starts = frame["method"] == "session.create";
        let id = self.post(&mut self.routes(), frame, Some(Reply::Wait(answer)))?;
        match answered.recv_timeout(within) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(STOPPED.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut routes = self.routes();
                if routes.replies.remove(&id).is_some() && starts {
                    routes.gave_up(id);
                }
                drop(routes);
                // It may have arrived in the moment the wait ran out.
                answered.try_recv().unwrap_or_else(|_| Err(QUIET.into()))
            }
        }
    }

    /// One question about a folder: its history, settings or extensions.
    pub(crate) fn about(&self, folder: &Path, mut request: Value) -> Result<Value, String> {
        request["folder"] = json!(folder);
        self.ask(request, EVENTUALLY)
    }

    pub(crate) fn is_live(&self, session: &str) -> bool {
        self.ask(json!({ "method": "session.list" }), SOON)
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
            let create = json!({ "method": "session.create", "params": params });
            let error = match self.ask(create, EVENTUALLY) {
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
        if let Err(error) = self.ask(attach, SOON) {
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
            since: Instant::now(),
        });
        if matches!(frame["method"].as_str(), Some("session.close" | "shutdown")) {
            routes.is_over(chat.session.clone());
        }
        frame["session"] = json!(chat.session);
        self.post(&mut routes, frame, reply).map(|_| ())
    }

    /// Stops hearing a chat without stopping it. Whoever else watches it goes
    /// on; a chat that was this window's alone ends for want of anyone watching.
    pub(crate) fn leave(&self, chat: &Chat) {
        let mut routes = self.routes();
        if routes.forget(&chat.session, chat.opened).is_none() {
            return;
        }
        routes.is_over(chat.session.clone());
        let detach = json!({ "method": "session.detach", "session": chat.session });
        let _ = self.post(&mut routes, detach, None);
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
                    let opened = routes.chats.get(&session).map(|(opened, _)| *opened);
                    let ended = opened.and_then(|opened| routes.forget(&session, opened));
                    if ended.is_some() {
                        routes.is_over(session);
                    }
                    ended
                };
                let reason = said["params"]["error"].as_str().map(str::to_owned);
                if let Some(hear) = ended {
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
        let (reply, late) = {
            let mut routes = self.routes();
            (
                routes.replies.remove(&id),
                routes.late.remove(&id).is_some(),
            )
        };
        match reply {
            Some(Reply::Wait(answer)) => {
                let _ = answer.send(outcome(frame));
            }
            Some(Reply::Chat {
                session,
                opened,
                id,
                ..
            }) => {
                frame["id"] = id;
                let hear = self.routes().hearing(&session, Some(opened));
                if let Some(hear) = hear {
                    hear(Said::Frame(frame));
                }
            }
            // A chat that started after its wait was given up has nobody; it is told to stop.
            None if late => {
                if let Some(session) = frame["result"]["session"].as_str() {
                    let mut routes = self.routes();
                    for method in ["session.attach", "session.close"] {
                        let _ = self.post(
                            &mut routes,
                            json!({ "method": method, "session": session }),
                            None,
                        );
                    }
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

/// How many bytes a frame is as it leaves, found without writing it out.
pub(crate) fn size(frame: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(1);
    let _ = serde_json::to_writer(&mut count, frame);
    count.0
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
    let unsent = Arc::<AtomicUsize>::default();
    let connection = Arc::new(Connection {
        lines,
        unsent: Arc::clone(&unsent),
        routes: Mutex::default(),
    });
    tokio::spawn(async move {
        while let Some(line) = queued.recv().await {
            if writing.write_all(line.as_bytes()).await.is_err() || writing.flush().await.is_err() {
                break;
            }
            unsent.fetch_sub(line.len(), Ordering::Relaxed);
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
                    "Could not start Medha backend. See the serve log in Medha's logs folder."
                        .into(),
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

#[cfg(test)]
impl Backend {
    /// Joined to a backend a test runs itself, which this never starts or stops.
    pub(crate) fn joined_to(address: String, token: &str) -> Arc<Self> {
        let Ok(connection) = reading().block_on(join(address, token.into())) else {
            panic!("the test's backend did not admit the client");
        };
        Arc::new(Self {
            executable: None,
            env: Vec::new(),
            owned: true,
            link: Mutex::new(Link {
                connection: Some(connection),
                started: None,
            }),
        })
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
pub(crate) mod tests;
