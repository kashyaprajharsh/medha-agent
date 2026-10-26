//! Every platform the gateway can use, with what setup asks for. Adding a platform is one
//! entry here and its own file.

use crate::platform::Platform;
use crate::telegram;
use anyhow::{Result, bail};
use std::sync::Arc;

pub struct Manifest {
    pub name: &'static str,
    pub label: &'static str,
    /// How to get the token, shown before setup asks for it.
    pub steps: &'static str,
    pub token: &'static str,
}

pub const ALL: &[Manifest] = &[Manifest {
    name: "telegram",
    label: "Telegram",
    steps: "1. In Telegram, open @BotFather and send /newbot.\n2. Pick a name and a username ending in \"bot\".\n3. BotFather replies with a token like 123456:ABC-DEF.",
    token: "Bot token",
}];

pub fn find(name: &str) -> Option<&'static Manifest> {
    ALL.iter().find(|manifest| manifest.name == name)
}

/// Checks a token with the platform and says which account it belongs to.
pub async fn check(name: &str, token: &str) -> Result<String> {
    match name {
        "telegram" => Ok(format!(
            "@{}",
            telegram::check(&telegram::client()?, token).await?.username
        )),
        _ => bail!("Medha does not know the platform {name}"),
    }
}

pub async fn connect(name: &str, token: &str) -> Result<Arc<dyn Platform>> {
    match name {
        "telegram" => Ok(Arc::new(telegram::Telegram::connect(token).await?)),
        _ => bail!("Medha does not know the platform {name}"),
    }
}
