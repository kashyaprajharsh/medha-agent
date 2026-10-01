//! Workspace routing is a desktop adapter, never a second agent runtime.
//! Projects use their existing store. Personal chats each get their own folder
//! and existing Medha store; the library combines their session lists.
use crate::{live::LiveSessions, service::Service, terminal::Terminals};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Seconds an empty chat folder is left alone before it counts as unused.
const UNUSED_CHAT_GRACE: f64 = 600.0;
const SERVICE_IDLE: Duration = Duration::from_secs(300);
const SERVICE_SWEEP: Duration = Duration::from_secs(60);

type Runtimes = Mutex<HashMap<PathBuf, Arc<Runtime>>>;

/// Runs while the registry lives: idle services stop instead of piling up for every chat ever opened.
fn rest_idle_services(runtimes: Weak<Runtimes>) {
    loop {
        std::thread::sleep(SERVICE_SWEEP);
        let Some(runtimes) = runtimes.upgrade() else {
            return;
        };
        let all: Vec<_> = runtimes
            .lock()
            .map(|runtimes| runtimes.values().cloned().collect())
            .unwrap_or_default();
        drop(runtimes);
        for runtime in all {
            runtime.rest(Instant::now());
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub personal: bool,
}

/// Requests that wait on git or the network. They run in a second service
/// process: the backend answers one request at a time, and a slow clone must
/// never hold up session history.
pub(crate) const SLOW_REQUESTS: [&str; 10] = [
    "usage.summary",
    "library.usage",
    "extensions.mcp.registry",
    "extensions.install",
    "extensions.update.preview",
    "extensions.update.apply",
    "extensions.skill.install",
    "extensions.marketplace.add",
    "extensions.marketplace.refresh",
    "settings.model.discover",
];

/// Requests that change the chat's own folder or its state, not the user's settings.
const CHAT_WRITES: [&str; 15] = [
    "settings.tools.save",
    "instructions.save",
    "extensions.install",
    "extensions.enable",
    "extensions.disable",
    "extensions.remove",
    "extensions.rollback",
    "extensions.update.apply",
    "extensions.hooks.add",
    "extensions.hooks.remove",
    "extensions.skill.configure",
    "extensions.skill.install",
    "extensions.skill.remove",
    "extensions.skills.lock",
    "extensions.skills.sync",
];

pub struct Runtime {
    pub path: PathBuf,
    service: Mutex<Option<Service>>,
    jobs: Mutex<Option<Service>>,
    used: Mutex<Instant>,
    pub live: LiveSessions,
    pub terminals: Terminals,
}
impl Runtime {
    fn new(path: PathBuf) -> Self {
        Self {
            live: LiveSessions::new(path.clone()),
            terminals: Terminals::new(path.clone()),
            path,
            service: Mutex::new(None),
            jobs: Mutex::new(None),
            used: Mutex::new(Instant::now()),
        }
    }
    fn with_service<T>(
        &self,
        slot: &Mutex<Option<Service>>,
        call: impl FnOnce(&mut Service) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut service = slot.lock().map_err(|e| e.to_string())?;
        if service.is_none() {
            *service = Some(Service::start(&self.path)?);
        }
        let result = call(service.as_mut().unwrap());
        // A backend that stopped is started again on the next request.
        if result.as_ref().is_err_and(|error| {
            error.contains("stopped unexpectedly") || error.starts_with("Backend request failed")
        }) {
            *service = None;
        }
        if let Ok(mut used) = self.used.lock() {
            *used = Instant::now();
        }
        result
    }
    /// Stops service processes nobody has asked anything of lately; a request in
    /// flight holds its slot, so it is never cut off. The next request starts one again.
    fn rest(&self, now: Instant) {
        let idle = self
            .used
            .lock()
            .is_ok_and(|used| now.saturating_duration_since(*used) >= SERVICE_IDLE);
        if idle {
            for slot in [&self.service, &self.jobs] {
                if let Ok(mut service) = slot.try_lock() {
                    service.take();
                }
            }
        }
    }
    pub fn request(
        &self,
        method: &str,
        session: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Value, String> {
        self.with_service(&self.service, |service| {
            service.request(method, session, cursor)
        })
    }
    pub fn request_params(&self, method: &str, params: Value) -> Result<Value, String> {
        let slot = if SLOW_REQUESTS.contains(&method) {
            &self.jobs
        } else {
            &self.service
        };
        self.with_service(slot, |service| service.request_params(method, params))
    }
}

pub struct Workspaces {
    data: PathBuf,
    initial: String,
    entries: Mutex<Vec<Workspace>>,
    runtimes: Arc<Runtimes>,
    sessions: Mutex<HashMap<String, PathBuf>>,
    chats: Mutex<HashMap<String, PathBuf>>,
    /// Held while the app runs, so the next one to open knows it is not alone.
    _running: Option<std::fs::File>,
    /// Set when no other app was open at launch, and spent by the first listing:
    /// unused chats are cleared only then, before any of them can be in use.
    prune: AtomicBool,
}

/// Marks this app as running and reports whether it is the only one.
fn mark_running(data: &std::path::Path) -> (Option<std::fs::File>, bool) {
    let open = |name: &str| {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data.join(name))
    };
    let (Ok(deciding), Ok(file)) = (open("launching.lock"), open("running.lock")) else {
        return (None, false);
    };
    // One app decides at a time: two launched together cannot both find themselves alone.
    if deciding.lock().is_err() {
        return (None, false);
    }
    let alone = file.try_lock().is_ok() && file.unlock().is_ok();
    let held = file.lock_shared().is_ok();
    (Some(file), alone && held)
}

impl Workspaces {
    pub fn new(data: PathBuf, explicit: Option<PathBuf>) -> Result<Self, String> {
        let personal = data.join("personal");
        std::fs::create_dir_all(&personal).map_err(|e| e.to_string())?;
        let saved = data.join("workspaces.json");
        let mut entries: Vec<Workspace> = if saved.exists() {
            serde_json::from_slice(&std::fs::read(&saved).map_err(|e| e.to_string())?)
                .map_err(|e| format!("Could not read saved projects: {e}"))?
        } else {
            vec![]
        };
        entries.retain(|w| !w.personal && w.id != "personal");
        entries.insert(
            0,
            Workspace {
                id: "personal".into(),
                name: "Chats".into(),
                path: personal.canonicalize().map_err(|e| e.to_string())?,
                personal: true,
            },
        );
        let runtimes = Arc::new(Mutex::new(HashMap::new()));
        let watched = Arc::downgrade(&runtimes);
        std::thread::spawn(move || rest_idle_services(watched));
        let (running, alone) = mark_running(&data);
        let registry = Self {
            _running: running,
            prune: AtomicBool::new(alone),
            data,
            initial: "personal".into(),
            entries: Mutex::new(entries),
            runtimes,
            sessions: Mutex::new(HashMap::new()),
            chats: Mutex::new(HashMap::new()),
        };
        if let Some(path) = explicit {
            let selected = registry.add(path)?;
            Ok(Self {
                initial: selected.id,
                ..registry
            })
        } else {
            Ok(registry)
        }
    }
    pub fn list(&self) -> Result<Value, String> {
        Ok(
            json!({ "initial": self.initial, "workspaces": self.entries.lock().map_err(|e| e.to_string())?.clone() }),
        )
    }
    pub fn add(&self, path: PathBuf) -> Result<Workspace, String> {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        if !path.is_dir() {
            return Err("Choose a folder".into());
        }
        let mut entries = self.entries.lock().map_err(|e| e.to_string())?;
        if let Some(existing) = entries.iter().find(|w| w.path == path) {
            return Ok(existing.clone());
        }
        let mut number = entries.len();
        while entries.iter().any(|w| w.id == format!("project-{number}")) {
            number += 1;
        }
        let item = Workspace {
            id: format!("project-{number}"),
            name: path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            path,
            personal: false,
        };
        entries.push(item.clone());
        let saved = self.data.join("workspaces.json");
        let temporary = self.data.join("workspaces.json.tmp");
        let result = std::fs::write(
            &temporary,
            serde_json::to_vec_pretty(&*entries).map_err(|e| e.to_string())?,
        )
        .and_then(|()| std::fs::rename(&temporary, &saved));
        if let Err(error) = result {
            entries.pop();
            return Err(error.to_string());
        }
        Ok(item)
    }
    pub fn workspace(&self, id: &str) -> Result<Workspace, String> {
        self.entries
            .lock()
            .map_err(|e| e.to_string())?
            .iter()
            .find(|w| w.id == id)
            .cloned()
            .ok_or_else(|| "Workspace is unavailable".into())
    }
    fn runtime(&self, path: PathBuf) -> Result<Arc<Runtime>, String> {
        let mut runtimes = self.runtimes.lock().map_err(|e| e.to_string())?;
        Ok(runtimes
            .entry(path.clone())
            .or_insert_with(|| Arc::new(Runtime::new(path)))
            .clone())
    }
    /// Never creates anything: a Personal draft owns no folder until it starts.
    pub fn resolve(
        &self,
        id: &str,
        key: Option<&str>,
        session: Option<&str>,
    ) -> Result<Arc<Runtime>, String> {
        self.place(id, key, session, false)
    }
    /// Where a chat really begins: its first message, or a terminal opened in it.
    pub fn start(
        &self,
        id: &str,
        key: Option<&str>,
        session: Option<&str>,
    ) -> Result<Arc<Runtime>, String> {
        self.place(id, key, session, true)
    }
    /// Where a settings or extension request runs. A draft has no folder to read,
    /// so reading asks the library; writing into the chat starts it, or the
    /// change would land where the chat never looks.
    pub fn for_request(
        &self,
        id: &str,
        key: Option<&str>,
        session: Option<&str>,
        method: &str,
    ) -> Result<Arc<Runtime>, String> {
        if CHAT_WRITES.contains(&method) {
            return self.start(id, key, session);
        }
        let runtime = self.resolve(id, key, session)?;
        if runtime.path.exists() {
            return Ok(runtime);
        }
        self.runtime(self.workspace(id)?.path)
    }
    fn place(
        &self,
        id: &str,
        key: Option<&str>,
        session: Option<&str>,
        create: bool,
    ) -> Result<Arc<Runtime>, String> {
        let workspace = self.workspace(id)?;
        if !workspace.personal {
            return self.runtime(workspace.path);
        }
        if let Some(session) = session {
            if !crate::live::is_ulid(session) {
                return Err("Invalid session".into());
            }
            if !self
                .sessions
                .lock()
                .map_err(|e| e.to_string())?
                .contains_key(session)
            {
                self.list_sessions(id)?;
            }
            let path = self
                .sessions
                .lock()
                .map_err(|e| e.to_string())?
                .get(session)
                .cloned()
                .ok_or("Personal chat was not found")?;
            if let Some(key) = key {
                self.chats
                    .lock()
                    .map_err(|e| e.to_string())?
                    .insert(key.into(), path.clone());
            }
            return self.runtime(path);
        }
        if let Some(key) = key {
            if !crate::live::is_token(key) {
                return Err("Invalid chat key".into());
            }
            let mut chats = self.chats.lock().map_err(|e| e.to_string())?;
            if let Some(path) = chats.get(key) {
                return self.runtime(path.clone());
            }
            let path = workspace.path.join(format!("chat-{key}"));
            if !create && !path.exists() {
                return self.runtime(path);
            }
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            if !path.starts_with(&workspace.path) {
                return Err("Personal chat folder is outside the library".into());
            }
            chats.insert(key.into(), path.clone());
            return self.runtime(path);
        }
        self.runtime(workspace.path)
    }
    /// Tokens and cost for the workspace, adding up each Personal chat folder.
    pub fn usage_summary(&self, id: &str, days: u64) -> Result<Value, String> {
        let workspace = self.workspace(id)?;
        let params = json!({ "days": days });
        if !workspace.personal {
            return self
                .runtime(workspace.path)?
                .request_params("usage.summary", params);
        }
        self.runtime(workspace.path)?
            .request_params("library.usage", params)
    }
    pub fn list_sessions(&self, id: &str) -> Result<Value, String> {
        let workspace = self.workspace(id)?;
        if !workspace.personal {
            return self
                .runtime(workspace.path)?
                .request("sessions.list", None, None);
        }
        let prune_before = self.prune.swap(false, Ordering::AcqRel).then(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |now| now.as_secs_f64() - UNUSED_CHAT_GRACE)
        });
        let value = self
            .runtime(workspace.path.clone())?
            .request_params("library.sessions", json!({ "prune_before": prune_before }))?;
        let mut rows = value.as_array().cloned().ok_or("Invalid session list")?;
        let mut sessions = self.sessions.lock().map_err(|e| e.to_string())?;
        for row in &mut rows {
            let folder = row.as_object_mut().and_then(|row| row.remove("folder"));
            let folder = folder.as_ref().and_then(Value::as_str).map(PathBuf::from);
            if let (Some(id), Some(folder)) = (row["id"].as_str(), folder)
                && folder.starts_with(&workspace.path)
            {
                sessions.insert(id.into(), folder);
            }
        }
        drop(sessions);
        rows.sort_by(|a, b| {
            b["last_ts"]
                .as_f64()
                .unwrap_or_default()
                .total_cmp(&a["last_ts"].as_f64().unwrap_or_default())
        });
        Ok(Value::Array(rows))
    }
    pub fn close_terminals(&self) {
        if let Ok(runtimes) = self.runtimes.lock() {
            for runtime in runtimes.values() {
                runtime.terminals.close_all();
            }
        }
    }
}

pub fn explicit_workspace() -> Option<PathBuf> {
    std::env::var_os("MEDHA_DESKTOP_WORKSPACE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::args_os()
                .skip(1)
                .find(|arg| !arg.to_string_lossy().starts_with('-'))
                .map(PathBuf::from)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_are_canonical_persisted_and_keep_their_runtime() {
        let root = std::env::temp_dir().join(format!("medha-workspaces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let registry = Workspaces::new(root.join("app"), None).unwrap();
        assert_eq!(registry.initial, "personal");
        let first = registry.add(root.join("repo")).unwrap();
        assert_eq!(registry.add(root.join("repo/.")).unwrap().id, first.id);
        let a = registry.resolve(&first.id, None, None).unwrap();
        let b = registry.resolve(&first.id, None, None).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(registry.resolve("unknown", None, None).is_err());
        let personal_a = registry.resolve("personal", Some("draft-a"), None).unwrap();
        let personal_b = registry.resolve("personal", Some("draft-b"), None).unwrap();
        assert_ne!(personal_a.path, personal_b.path);
        assert!(
            registry
                .resolve("personal", Some("../escape"), None)
                .is_err()
        );
        drop(registry);
        let reopened = Workspaces::new(root.join("app"), Some(root.join("repo"))).unwrap();
        assert_eq!(reopened.initial, first.id);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_draft_owns_nothing_on_disk_until_it_starts() {
        let root = std::env::temp_dir().join(format!("medha-drafts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let registry = Workspaces::new(root.join("app"), None).unwrap();
        let library = registry.workspace("personal").unwrap().path;
        let chats = || std::fs::read_dir(&library).unwrap().count();

        let draft = registry.resolve("personal", Some("draft-x"), None).unwrap();
        assert_eq!(crate::files::list(&draft.path, "").unwrap(), json!([]));
        assert_eq!(crate::git::status(&draft.path).unwrap()["files"], json!([]));
        let request = |method| registry.for_request("personal", Some("draft-x"), None, method);
        assert_eq!(request("settings.list").unwrap().path, library);
        assert_eq!(chats(), 0, "looking at a draft created a folder");

        let started = request("settings.tools.save").unwrap();
        assert!(
            started.path.is_dir() && started.path != library,
            "a draft's own setting was saved where the chat never reads"
        );
        assert!(
            Arc::ptr_eq(&draft, &started),
            "a started draft must keep its runtime, or its live chat is lost"
        );
        let again = registry.resolve("personal", Some("draft-x"), None).unwrap();
        assert!(Arc::ptr_eq(&again, &started));
        assert_eq!(chats(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_an_app_running_alone_clears_unused_chats() {
        let root = std::env::temp_dir().join(format!("medha-alone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = Workspaces::new(root.join("app"), None).unwrap();
        let second = Workspaces::new(root.join("app"), None).unwrap();
        assert!(first.prune.load(Ordering::Acquire));
        assert!(
            !second.prune.load(Ordering::Acquire),
            "a second app could delete a draft the first still has open"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_idle_service_stops_but_one_answering_a_request_is_kept() {
        let runtime = Runtime::new(std::env::temp_dir());
        *runtime.service.lock().unwrap() = Some(Service::stand_in());
        *runtime.jobs.lock().unwrap() = Some(Service::stand_in());
        runtime.rest(Instant::now());
        assert!(
            runtime.service.lock().unwrap().is_some(),
            "a service in use was stopped"
        );

        let answering = runtime.jobs.lock().unwrap();
        runtime.rest(Instant::now() + SERVICE_IDLE);
        drop(answering);
        assert!(
            runtime.service.lock().unwrap().is_none(),
            "an idle service kept running"
        );
        assert!(
            runtime.jobs.lock().unwrap().is_some(),
            "a request in flight was cut off"
        );
    }
}
