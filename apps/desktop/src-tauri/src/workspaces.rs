//! Workspace routing is a desktop adapter, never a second agent runtime.
//! Projects use their existing store. Personal chats each get their own folder
//! and existing Medha store; the library combines their session lists.
use crate::{live::LiveSessions, service::Service, terminal::Terminals};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

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
pub(crate) const SLOW_REQUESTS: [&str; 9] = [
    "usage.summary",
    "extensions.mcp.registry",
    "extensions.install",
    "extensions.update.preview",
    "extensions.update.apply",
    "extensions.skill.install",
    "extensions.marketplace.add",
    "extensions.marketplace.refresh",
    "settings.model.discover",
];

pub struct Runtime {
    pub path: PathBuf,
    service: Mutex<Option<Service>>,
    jobs: Mutex<Option<Service>>,
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
        result
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
    runtimes: Mutex<HashMap<PathBuf, Arc<Runtime>>>,
    sessions: Mutex<HashMap<String, PathBuf>>,
    chats: Mutex<HashMap<String, PathBuf>>,
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
                name: "Personal".into(),
                path: personal.canonicalize().map_err(|e| e.to_string())?,
                personal: true,
            },
        );
        let registry = Self {
            data,
            initial: "personal".into(),
            entries: Mutex::new(entries),
            runtimes: Mutex::new(HashMap::new()),
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
    pub fn resolve(
        &self,
        id: &str,
        key: Option<&str>,
        session: Option<&str>,
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
        let mut summaries = Vec::new();
        for entry in std::fs::read_dir(&workspace.path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir()
                || !entry.file_name().to_string_lossy().starts_with("chat-")
            {
                continue;
            }
            let path = entry.path().canonicalize().map_err(|e| e.to_string())?;
            summaries.push(
                self.runtime(path)?
                    .request_params("usage.summary", params.clone())?,
            );
        }
        Ok(crate::usage::merge(summaries, days))
    }
    pub fn list_sessions(&self, id: &str) -> Result<Value, String> {
        let workspace = self.workspace(id)?;
        if !workspace.personal {
            return self
                .runtime(workspace.path)?
                .request("sessions.list", None, None);
        }
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(&workspace.path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir()
                || !entry.file_name().to_string_lossy().starts_with("chat-")
            {
                continue;
            }
            let path = entry.path().canonicalize().map_err(|e| e.to_string())?;
            let value = self
                .runtime(path.clone())?
                .request("sessions.list", None, None)?;
            let items = value.as_array().ok_or("Invalid session list")?;
            let mut sessions = self.sessions.lock().map_err(|e| e.to_string())?;
            for row in items {
                if let Some(id) = row["id"].as_str() {
                    sessions.insert(id.into(), path.clone());
                }
            }
            rows.extend(items.iter().cloned());
        }
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
}
