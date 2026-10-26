//! Messaging settings kept in `config.toml`: where messaging chats work and which platforms are on.
//! Platform tokens stay in the credential store under [`credential_id`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Messaging {
    /// The folder every messaging chat works in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Set up platforms by name, such as `telegram`.
    #[serde(default, flatten)]
    pub platforms: BTreeMap<String, Platform>,
}

impl Messaging {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
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

/// Where a platform's token is kept in the credential store.
pub fn credential_id(platform: &str) -> String {
    format!("messaging://{platform}")
}
