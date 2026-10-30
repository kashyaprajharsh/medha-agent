//! Messaging settings kept in `config.toml`: where messaging chats work and which platforms are on.
//! Platform tokens stay in the credential store under [`credential_id`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Messaging {
    /// The folder a messaging chat works in until it picks another project at `/new`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// More folders a chat may pick at `/new`; added only on this computer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<PathBuf>,
    /// Show each tool step in the chat, as one message rewritten as the work goes on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub steps: bool,
    /// Set up platforms by name, such as `telegram`.
    #[serde(default, flatten)]
    pub platforms: BTreeMap<String, Platform>,
}

impl Messaging {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Every folder a chat may work in: the default first, then the others as added.
    pub fn allowed(&self) -> Vec<PathBuf> {
        let mut allowed: Vec<PathBuf> = self.folder.iter().cloned().collect();
        for folder in &self.folders {
            if !allowed.contains(folder) {
                allowed.push(folder.clone());
            }
        }
        allowed
    }

    /// New chats start in `folder`; the old default stays open to chats that chose it.
    pub fn make_default(&mut self, folder: PathBuf) {
        if let Some(old) = self.folder.replace(folder.clone())
            && old != folder
            && !self.folders.contains(&old)
        {
            self.folders.push(old);
        }
        self.folders.retain(|kept| *kept != folder);
    }

    /// Set up platforms that are switched on.
    pub fn enabled(&self) -> impl Iterator<Item = &str> {
        self.platforms
            .iter()
            .filter(|(_, platform)| platform.enabled)
            .map(|(name, _)| name.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    #[serde(default = "on")]
    pub enabled: bool,
    /// Non-secret values from setup, such as an email server's address.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, String>,
}

fn on() -> bool {
    true
}

/// The setting naming a platform's home chat, where notices go.
pub const HOME: &str = "home";

impl Platform {
    pub fn home(&self) -> Option<&str> {
        self.settings.get(HOME).map(String::as_str)
    }
}

/// `/sethome`: notices for `platform` go to `chat` from now on.
pub fn set_home(platform: &str, chat: &str) -> anyhow::Result<()> {
    let mut cfg = crate::config::load()?.unwrap_or_default();
    cfg.gateway
        .platforms
        .entry(platform.to_owned())
        .or_insert_with(|| Platform {
            enabled: true,
            settings: BTreeMap::new(),
        })
        .settings
        .insert(HOME.into(), chat.into());
    crate::config::save(&cfg)
}

/// A folder a phone may work in: a project, never the home folder, a root, or Medha's own data.
pub fn admit_folder(folder: &std::path::Path) -> anyhow::Result<PathBuf> {
    let folder = folder
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("{} is not a folder on this computer", folder.display()))?;
    anyhow::ensure!(folder.is_dir(), "{} is not a folder", folder.display());
    let broad = folder.parent().is_none()
        || dirs::home_dir()
            .and_then(|home| home.canonicalize().ok())
            .is_some_and(|home| home.starts_with(&folder));
    anyhow::ensure!(
        !broad,
        "{} holds far more than one project. Choose a project folder inside it.",
        folder.display()
    );
    let medha = crate::config::medha_home()?;
    let medha = medha.canonicalize().unwrap_or(medha);
    anyhow::ensure!(
        !folder.starts_with(&medha) && !medha.starts_with(&folder),
        "{} holds Medha's own settings and keys. Choose a project folder.",
        folder.display()
    );
    Ok(folder)
}

/// Where a platform's token is kept in the credential store.
pub fn credential_id(platform: &str) -> String {
    format!("messaging://{platform}")
}

#[cfg(test)]
#[path = "messaging_tests.rs"]
mod tests;
