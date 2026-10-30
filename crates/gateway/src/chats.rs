//! Which Medha conversation each platform chat continues, and the project it chose at `/new`,
//! kept across gateway restarts.

use crate::store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Chats {
    path: PathBuf,
}

/// A chat's place; a file written before projects could be chosen holds the conversation alone.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    #[default]
    Empty,
    Conversation(String),
    Chosen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<PathBuf>,
    },
}

impl Entry {
    fn conversation(&self) -> Option<&str> {
        match self {
            Entry::Conversation(conversation) => Some(conversation),
            Entry::Chosen { conversation, .. } => conversation.as_deref(),
            Entry::Empty => None,
        }
    }

    fn folder(&self) -> Option<&Path> {
        match self {
            Entry::Chosen { folder, .. } => folder.as_deref(),
            _ => None,
        }
    }

    fn with(conversation: Option<String>, folder: Option<PathBuf>) -> Self {
        match (conversation, folder) {
            (None, None) => Entry::Empty,
            (Some(conversation), None) => Entry::Conversation(conversation),
            (conversation, folder) => Entry::Chosen {
                conversation,
                folder,
            },
        }
    }
}

type Map = BTreeMap<String, Entry>;

impl Chats {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn conversation(&self, key: &str) -> Result<Option<String>> {
        let map: Map = store::read(&self.path)?;
        Ok(map
            .get(key)
            .and_then(Entry::conversation)
            .map(str::to_owned))
    }

    /// The folder this chat picked at `/new`, if any; whether it is still allowed is the
    /// caller's to check each time.
    pub fn folder(&self, key: &str) -> Result<Option<PathBuf>> {
        let map: Map = store::read(&self.path)?;
        Ok(map.get(key).and_then(Entry::folder).map(Path::to_path_buf))
    }

    pub fn remember(&self, key: &str, conversation: &str) -> Result<()> {
        self.change(key, |_, folder| (Some(conversation.to_owned()), folder))
    }

    /// A project chosen at `/new`: the chat works in `folder` from now on, in a conversation
    /// started there.
    pub fn choose_folder(&self, key: &str, folder: &Path) -> Result<()> {
        self.change(key, |_, _| (None, Some(folder.to_path_buf())))
    }

    /// Every platform chat with the conversation it continues.
    pub fn all(&self) -> Result<BTreeMap<String, String>> {
        let map: Map = store::read(&self.path)?;
        Ok(map
            .into_iter()
            .filter_map(|(key, entry)| Some((key, entry.conversation()?.to_owned())))
            .collect())
    }

    /// `/new`: the next message starts a fresh conversation, in the folder the chat chose.
    pub fn forget(&self, key: &str) -> Result<()> {
        self.change(key, |_, folder| (None, folder))
    }

    fn change(
        &self,
        key: &str,
        next: impl FnOnce(Option<String>, Option<PathBuf>) -> (Option<String>, Option<PathBuf>),
    ) -> Result<()> {
        store::change(&self.path, |map: &mut Map| {
            let entry = map.remove(key).unwrap_or_default();
            let (conversation, folder) = next(
                entry.conversation().map(str::to_owned),
                entry.folder().map(Path::to_path_buf),
            );
            match Entry::with(conversation, folder) {
                Entry::Empty => {}
                entry => {
                    map.insert(key.to_owned(), entry);
                }
            }
        })
    }
}

/// One platform chat, named so two platforms' chat ids never collide.
pub fn key(platform: &str, chat: &str) -> String {
    format!("{platform}:{chat}")
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod tests;
