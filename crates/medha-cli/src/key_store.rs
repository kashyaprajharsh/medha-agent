//! Moving saved keys between the private file and the system keychain.
//! No key leaves one store until every key has arrived in the other.

use super::{
    Config, CredentialsFile, KEYRING_SERVICE, credentials_path, key_store_forced, mcp_key_id,
    mcp_oauth_id, prefer_keychain, search_cred_id, with_credentials_lock, write_credentials_file,
};
use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// The keychain as a move needs it; tests stand a map in for it.
trait Vault {
    fn put(&self, id: &str, key: &str) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<String>>;
    fn forget(&self, id: &str);
}

struct Keychain;

impl Vault for Keychain {
    fn put(&self, id: &str, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, id)?;
        entry.set_password(key)?;
        // A store that cannot give a key back must not hold the only copy.
        anyhow::ensure!(entry.get_password()? == key, "the key did not read back");
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<String>> {
        match keyring::Entry::new(KEYRING_SERVICE, id).and_then(|entry| entry.get_password()) {
            Ok(key) => Ok(Some(key).filter(|key| !key.is_empty())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn forget(&self, id: &str) {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, id) {
            let _ = entry.delete_credential();
        }
    }
}

/// Every credential the saved settings can reach. A keychain cannot be listed,
/// so these are the entries a move out of it looks for.
fn credential_ids(cfg: &Config) -> Vec<String> {
    let mut ids: Vec<String> = cfg
        .models
        .values()
        .map(|model| model.base_url.clone())
        .collect();
    ids.extend(
        [tools::SearchProvider::Tavily, tools::SearchProvider::Brave]
            .into_iter()
            .filter_map(search_cred_id)
            .map(str::to_owned),
    );
    for (id, server) in &cfg.mcp {
        ids.push(mcp_key_id(id, server));
        if !server.url.is_empty() {
            ids.push(mcp_oauth_id(id, &server.url));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Moves `keys` to or from the vault and returns the vault entries to forget
/// once the file is written. On any failure both stores are as they were.
fn relocate(
    keys: &mut BTreeMap<String, String>,
    ids: &[String],
    to_keychain: bool,
    vault: &dyn Vault,
) -> Result<Vec<String>> {
    if to_keychain {
        let mut arrived: Vec<&str> = Vec::new();
        for (id, key) in keys.iter() {
            if let Err(error) = vault.put(id, key) {
                arrived.iter().for_each(|id| vault.forget(id));
                return Err(error.context(
                    "The system keychain did not accept your keys, so they stay in the file",
                ));
            }
            arrived.push(id);
        }
        keys.clear();
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for id in ids {
        let key = vault
            .get(id)
            .context("The system keychain could not be read, so your keys stay in it")?;
        if let Some(key) = key {
            found.push((id.clone(), key));
        }
    }
    let stale = found.iter().map(|(id, _)| id.clone()).collect();
    keys.extend(found);
    Ok(stale)
}

/// Keep keys in the keychain or in the private file from now on, and move the
/// saved ones there.
pub(crate) fn move_keys(cfg: &Config, to_keychain: bool) -> Result<()> {
    anyhow::ensure!(
        !key_store_forced(),
        "MEDHA_CRED_STORE is set, so it decides where keys are kept"
    );
    with_credentials_lock(|| {
        let path = credentials_path()?;
        let mut creds: CredentialsFile = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        // Already there: the other store holds only leftovers, which must not win.
        let stale = if prefer_keychain() == to_keychain {
            Vec::new()
        } else {
            relocate(
                &mut creds.keys,
                &credential_ids(cfg),
                to_keychain,
                &Keychain,
            )?
        };
        creds.store = Some(if to_keychain { "keychain" } else { "file" }.into());
        write_credentials_file(&path, &creds)?;
        stale.iter().for_each(|id| Keychain.forget(id));
        Ok(())
    })
}

#[cfg(test)]
#[path = "key_store_tests.rs"]
mod tests;
