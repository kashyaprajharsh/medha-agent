//! More than one key for a model's server: where the extra ones are kept, and
//! the order all of them are tried in. The settings hold the order, never a key.

use crate::config::{self, Config};
use anyhow::Result;

/// Where extra key `slot` of a server is kept in the credential store.
pub(crate) fn id(server: &str, slot: u32) -> String {
    format!("{server}#key{slot}")
}

/// The store id of every extra key the settings list, for moving them with the rest.
pub(crate) fn ids(cfg: &Config) -> Vec<String> {
    let listed = cfg.extra_keys.iter();
    listed
        .flat_map(|(server, slots)| slots.iter().map(move |slot| id(server, *slot)))
        .collect()
}

/// The extra keys saved for a server, in order: each one's slot, and whether
/// the credential store still holds it.
pub fn slots(cfg: &Config, server: &str) -> Vec<(u32, bool)> {
    let listed = cfg.extra_keys.get(server).into_iter().flatten();
    listed
        .map(|slot| (*slot, config::key_present(&id(server, *slot))))
        .collect()
}

/// Every key for a server in the order they are tried: its first key, then the
/// extra ones as they were added. A key the store no longer holds is left out.
pub fn keys(cfg: &Config, server: &str) -> Vec<String> {
    let extra = cfg.extra_keys.get(server).into_iter().flatten();
    let stored = std::iter::once(server.to_owned()).chain(extra.map(|slot| id(server, *slot)));
    stored.filter_map(|id| config::load_key(&id)).collect()
}

/// Save another key for a server, tried after the ones already there. Returns
/// its slot. The same key twice would only be refused twice, so it is not taken.
pub fn add(server: &str, key: &str) -> Result<u32> {
    let mut added = 0;
    config::edit(|cfg| {
        anyhow::ensure!(
            !keys(cfg, server).iter().any(|saved| saved == key.trim()),
            "This key is already saved for this server"
        );
        let slots = cfg.extra_keys.entry(server.to_owned()).or_default();
        added = slots.iter().max().map_or(2, |last| last + 1);
        config::store_key(&id(server, added), key)?;
        slots.push(added);
        Ok(())
    })?;
    Ok(added)
}

/// Forget one extra key of a server.
pub fn remove(server: &str, slot: u32) -> Result<()> {
    config::edit(|cfg| {
        let slots = cfg.extra_keys.entry(server.to_owned()).or_default();
        anyhow::ensure!(
            slots.contains(&slot),
            "That key is not saved for this server"
        );
        slots.retain(|kept| *kept != slot);
        if slots.is_empty() {
            cfg.extra_keys.remove(server);
        }
        config::remove_key(&id(server, slot))
    })
}
