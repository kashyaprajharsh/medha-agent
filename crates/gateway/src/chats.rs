//! Which Medha conversation each platform chat continues, kept across gateway restarts.

use crate::store;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Chats {
    path: PathBuf,
}

impl Chats {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn conversation(&self, key: &str) -> Result<Option<String>> {
        let map: BTreeMap<String, String> = store::read(&self.path)?;
        Ok(map.get(key).cloned())
    }

    pub fn remember(&self, key: &str, conversation: &str) -> Result<()> {
        store::change(&self.path, |map: &mut BTreeMap<String, String>| {
            map.insert(key.to_owned(), conversation.to_owned());
        })
    }

    /// Every platform chat with the conversation it continues.
    pub fn all(&self) -> Result<BTreeMap<String, String>> {
        store::read(&self.path)
    }

    /// `/new`: the next message starts a fresh conversation.
    pub fn forget(&self, key: &str) -> Result<()> {
        store::change(&self.path, |map: &mut BTreeMap<String, String>| {
            map.remove(key);
        })
    }
}

/// One platform chat, named so two platforms' chat ids never collide.
pub fn key(platform: &str, chat: &str) -> String {
    format!("{platform}:{chat}")
}
