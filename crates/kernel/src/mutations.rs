//! Chat-owned exact mutation proposals. These never invoke the approval gate.

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub protected_paths: Vec<String>,
    pub checkpoint_count: usize,
    pub checkpoint_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            protected_paths: Vec::new(),
            checkpoint_count: 20,
            checkpoint_bytes: 500 * 1024 * 1024,
        }
    }
}

#[async_trait]
pub trait Apply: Send + Sync {
    async fn apply(&self) -> Result<Value, String>;
}

struct Proposal {
    review: Value,
    apply: Option<Arc<dyn Apply>>,
    shown: bool,
    created: u64,
    bytes: usize,
}

#[derive(Default)]
pub struct Guard {
    proposals: Mutex<BTreeMap<String, Proposal>>,
    workspace: Mutex<Option<std::path::PathBuf>>,
    checkpoints: Mutex<BTreeMap<std::path::PathBuf, Value>>,
    settings: Mutex<Settings>,
    answered: Mutex<VecDeque<String>>,
    sequence: std::sync::atomic::AtomicU64,
}

impl Guard {
    pub fn configure(&self, settings: Settings) -> Result<(), String> {
        if settings.checkpoint_count == 0 || settings.checkpoint_bytes == 0 {
            return Err("Checkpoint count and byte allowance must be positive".into());
        }
        for pattern in &settings.protected_paths {
            glob::Pattern::new(pattern)
                .map_err(|error| format!("Invalid protected path pattern: {error}"))?;
        }
        *self.settings.lock().unwrap() = settings;
        Ok(())
    }
    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
    pub fn classify(
        &self,
        path: &std::path::Path,
        after_web: bool,
    ) -> Option<crate::protected_paths::Kind> {
        if let Some(kind) = crate::protected_paths::builtin(path, after_web) {
            return Some(kind);
        }
        let workspace = crate::egress::execution_workspace().or_else(|| self.workspace());
        let path = workspace
            .as_ref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path);
        let options = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.settings
            .lock()
            .unwrap()
            .protected_paths
            .iter()
            .any(|pattern| {
                glob::Pattern::new(pattern)
                    .is_ok_and(|pattern| pattern.matches_path_with(path, options))
            })
            .then_some(crate::protected_paths::Kind::Operator)
    }
    pub fn begin_message(&self) {
        self.checkpoints.lock().unwrap().clear();
    }
    pub fn checkpoint(&self, root: &std::path::Path) -> Option<Value> {
        self.checkpoints.lock().unwrap().get(root).cloned()
    }
    pub fn remember_checkpoint(&self, root: &std::path::Path, value: Value) {
        self.checkpoints
            .lock()
            .unwrap()
            .insert(root.to_owned(), value);
    }
    pub fn set_workspace(&self, path: &std::path::Path) {
        if let Ok(path) = path.canonicalize() {
            *self
                .workspace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
        }
    }

    pub fn workspace(&self) -> Option<std::path::PathBuf> {
        self.workspace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn propose(
        &self,
        identity: &[u8],
        destination: &str,
        reason: &str,
        preview: &str,
        apply: Arc<dyn Apply>,
    ) -> Result<String, String> {
        self.insert(identity, destination, reason, preview, Some(apply))
    }

    pub fn refuse(
        &self,
        identity: &[u8],
        destination: &str,
        reason: &str,
        preview: &str,
    ) -> Result<String, String> {
        self.insert(identity, destination, reason, preview, None)
    }

    fn insert(
        &self,
        identity: &[u8],
        destination: &str,
        reason: &str,
        preview: &str,
        apply: Option<Arc<dyn Apply>>,
    ) -> Result<String, String> {
        let id = format!("{:x}", Sha256::digest(identity));
        if self.answered.lock().unwrap().contains(&id) {
            return Err("This exact protected change was already answered or dismissed; nothing was applied".into());
        }
        let bytes = identity.len().saturating_add(preview.len());
        if bytes > 64 * 1024 * 1024 {
            return Err(
                "The exact protected change exceeds the bounded review memory; nothing was applied"
                    .into(),
            );
        }
        let mut proposals = self
            .proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !proposals.contains_key(&id)
            && (proposals.len() >= 64
                || proposals
                    .values()
                    .map(|proposal| proposal.bytes)
                    .sum::<usize>()
                    .saturating_add(bytes)
                    > 64 * 1024 * 1024)
        {
            let Some(oldest) = proposals
                .iter()
                .min_by_key(|(_, proposal)| proposal.created)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            proposals.remove(&oldest);
        }
        proposals.entry(id.clone()).or_insert_with(|| Proposal {
            review: json!({"id":id,"destination":destination,"reason":reason,"preview":crate::redact::Secrets::default().redact(preview),"allowable":apply.is_some()}),
            apply,
            shown: false,
            created:self.sequence.fetch_add(1,std::sync::atomic::Ordering::Relaxed),
            bytes,
        });
        Ok(id)
    }

    pub fn reviews(&self) -> Vec<Value> {
        self.proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|proposal| proposal.review.clone())
            .collect()
    }

    pub fn notices(&self) -> Vec<Value> {
        self.proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
            .filter_map(|proposal| {
                if proposal.shown {
                    return None;
                }
                proposal.shown = true;
                Some(proposal.review.clone())
            })
            .collect()
    }

    pub async fn allow(&self, id: &str) -> Result<Value, String> {
        let proposal = self
            .proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .ok_or("That protected change is no longer available")?;
        self.remember_answer(id);
        proposal
            .apply
            .ok_or("This action cannot run safely; repair the stated problem first")?
            .apply()
            .await
    }

    pub fn dismiss(&self, id: &str) -> Result<(), String> {
        self.remember_answer(id);
        self.proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| "That protected change is no longer available".into())
    }

    fn remember_answer(&self, id: &str) {
        let mut answered = self.answered.lock().unwrap();
        answered.push_back(id.to_owned());
        while answered.len() > 256 {
            answered.pop_front();
        }
    }
}
