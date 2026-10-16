//! Checkout identity and machine-local authority shared by every surface.

use crate::config;
use anyhow::{Context, Result, bail};
use lockfile::{MedhaLock, RiskySetting};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RECORD: &str = "project_trust.toml";

pub use context::workspace::project_root as root;

#[derive(Default, Serialize, Deserialize)]
struct Record {
    root: PathBuf,
    trusted: bool,
    #[serde(default)]
    legacy_revoked: bool,
    #[serde(default)]
    accepted: BTreeMap<PathBuf, BTreeMap<String, String>>,
}

#[derive(Clone)]
pub struct ProjectTrust {
    pub root: PathBuf,
    record: PathBuf,
    home: PathBuf,
    state_home: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct Review {
    pub root: PathBuf,
    pub folder: PathBuf,
    pub trusted: bool,
    pub settings: Vec<RiskySetting>,
    pub ignored: Vec<String>,
    pub fingerprint: String,
}

impl ProjectTrust {
    pub fn for_folder(folder: &Path) -> Result<Option<Self>> {
        let home = dirs::home_dir().context("home directory unavailable")?;
        Self::in_home(folder, &home, &config::medha_home()?)
    }

    pub(crate) fn in_home(folder: &Path, home: &Path, state_home: &Path) -> Result<Option<Self>> {
        let Some(root) = root(folder, home)? else {
            return Ok(None);
        };
        let state = match config::existing_state_dir_in(state_home, &root) {
            Some(state) => state,
            None => config::select_state_dir(state_home, &root)?,
        };
        let record = state.join(RECORD);
        Ok(Some(Self {
            root,
            record,
            home: home.to_path_buf(),
            state_home: state_home.to_path_buf(),
        }))
    }

    fn read(&self) -> Record {
        std::fs::read_to_string(&self.record)
            .ok()
            .and_then(|text| toml::from_str::<Record>(&text).ok())
            .filter(|record| record.root == self.root)
            .unwrap_or_default()
    }

    pub fn trusted(&self) -> bool {
        self.read().trusted
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.record
            .parent()
            .expect("trust record has a parent")
            .join("project_skill_scans")
    }

    pub fn allows_content(&self, folder: &Path) -> bool {
        self.trusted() && root(folder, &self.home).ok().flatten().as_ref() == Some(&self.root)
    }

    pub fn allows_asset(&self, workspace: &Path, asset: &Path) -> bool {
        let (Ok(workspace), Ok(asset)) = (workspace.canonicalize(), asset.canonicalize()) else {
            return false;
        };
        if !self.allows_content(&workspace) || !asset.starts_with(&workspace) {
            return false;
        }
        for parent in asset.ancestors().take_while(|parent| *parent != workspace) {
            if std::fs::symlink_metadata(parent.join(".git")).is_ok() {
                return false;
            }
        }
        true
    }

    pub fn filter(&self, folder: &Path, lock: &MedhaLock) -> MedhaLock {
        if root(folder, &self.home).ok().flatten().as_ref() != Some(&self.root) {
            return lock.without_risky_settings();
        }
        let settings = lock.risky_settings();
        let record = self.read();
        let folder = match folder.canonicalize() {
            Ok(folder) => folder,
            Err(_) => return lock.without_risky_settings(),
        };
        let mut accepted = settings
            .iter()
            .filter(|setting| {
                record
                    .accepted
                    .get(&folder)
                    .and_then(|values| values.get(&setting.key))
                    == Some(&setting.value)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !record.legacy_revoked
            && let Some(state) = config::existing_state_dir_in(&self.state_home, &folder)
        {
            accepted.extend(crate::workspace::legacy_settings(lock, &folder, &state));
        }
        lock.with_accepted_settings(&accepted)
    }

    pub fn review(&self, folder: &Path) -> Result<Review> {
        let folder = folder.canonicalize()?;
        if root(&folder, &self.home)?.as_ref() != Some(&self.root) {
            bail!("the folder belongs to a different checkout");
        }
        let lock = MedhaLock::load(folder.join("medha.lock"))?.unwrap_or_default();
        let settings = lock.risky_settings();
        let safe = self.filter(&folder, &lock).risky_settings();
        let ignored = settings
            .iter()
            .filter(|setting| !safe.contains(setting))
            .map(|setting| setting.key.clone())
            .collect();
        Ok(Review {
            root: self.root.clone(),
            folder,
            trusted: self.trusted(),
            fingerprint: lockfile::trust::fingerprint(&settings),
            settings,
            ignored,
        })
    }

    pub fn accept(&self, shown: &Review) -> Result<()> {
        let mut mutex = self.mutex()?;
        let _guard = mutex.write()?;
        let current = self.review(&shown.folder)?;
        if shown.root != self.root || shown.fingerprint != current.fingerprint {
            bail!("project settings changed; read the notice again before accepting them");
        }
        let mut record = self.read();
        record.root.clone_from(&self.root);
        record.trusted = true;
        record.accepted.insert(
            current.folder,
            current
                .settings
                .into_iter()
                .map(|setting| (setting.key, setting.value))
                .collect(),
        );
        self.save(&record)
    }

    pub fn accept_for(
        &self,
        workspace: &Path,
        root: &Path,
        folder: &Path,
        fingerprint: String,
    ) -> Result<()> {
        let mut shown = self.review(workspace)?;
        if shown.root != root || shown.folder != folder {
            bail!("the workspace changed; review this folder before trusting it");
        }
        shown.fingerprint = fingerprint;
        self.accept(&shown)
    }

    pub fn revoke(&self) -> Result<()> {
        let mut mutex = self.mutex()?;
        let _guard = mutex.write()?;
        self.save(&Record {
            root: self.root.clone(),
            legacy_revoked: true,
            ..Record::default()
        })
    }

    fn mutex(&self) -> Result<fd_lock::RwLock<std::fs::File>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(self.record.with_extension("mutex"))?;
        Ok(fd_lock::RwLock::new(file))
    }

    fn save(&self, record: &Record) -> Result<()> {
        use std::io::Write;
        let text = toml::to_string(record)?;
        let temporary = self
            .record
            .with_extension(format!("tmp-{}", ulid::Ulid::new()));
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.record)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}

pub fn content_allowed(folder: &Path) -> bool {
    ProjectTrust::for_folder(folder)
        .ok()
        .flatten()
        .is_some_and(|trust| trust.allows_content(folder))
}

pub type SkillSource = dyn Fn(&Path, &Path) -> Option<PathBuf> + Send + Sync;

pub fn skill_source() -> std::sync::Arc<SkillSource> {
    std::sync::Arc::new(|workspace, asset| {
        let trust = ProjectTrust::for_folder(workspace).ok().flatten()?;
        trust
            .allows_asset(workspace, asset)
            .then(|| trust.cache_dir())
    })
}

fn servers() -> &'static std::sync::Mutex<Vec<std::sync::Weak<lsp::LspManager>>> {
    static SERVERS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<lsp::LspManager>>>> =
        std::sync::OnceLock::new();
    SERVERS.get_or_init(Default::default)
}

pub fn register_lsp(manager: &std::sync::Arc<lsp::LspManager>) {
    let mut servers = servers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    servers.retain(|server| server.strong_count() > 0);
    servers.push(std::sync::Arc::downgrade(manager));
}

pub async fn revoke_live(folder: &Path) -> Result<()> {
    let trust = ProjectTrust::for_folder(folder)?.context("the home folder is not a project")?;
    trust.revoke()?;
    let managers = servers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect::<Vec<_>>();
    for manager in managers {
        if root(manager.workspace(), &trust.home)
            .ok()
            .flatten()
            .as_ref()
            == Some(&trust.root)
        {
            manager.stop_all().await;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "project_trust_tests.rs"]
mod tests;
