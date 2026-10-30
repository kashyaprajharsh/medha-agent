//! Telegram setup by scanning a code: Medha Setup creates the person's own bot and hands its token
//! here sealed to a key that never leaves this computer.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const SERVICE: &str = "https://setup.manojavamlabs.com";
const POLL_EVERY: Duration = Duration::from_secs(2);
const WAIT: Duration = Duration::from_secs(10 * 60);

/// A setup waiting for the person to scan and create their bot.
pub struct Waiting {
    pub link: String,
    service: String,
    pairing: String,
    poll_token: String,
    receiver: Mutex<Option<handover::Receiver>>,
    ends: Instant,
    client: reqwest::Client,
}

/// The new bot, as the service named it; the owner is confirmed with Telegram before it is trusted.
pub struct Handed {
    pub token: String,
    pub bot: String,
    pub owner: i64,
}

#[derive(Deserialize)]
struct Started {
    pairing_id: String,
    poll_token: String,
    link: String,
}

#[derive(Deserialize)]
struct Polled {
    status: String,
    sealed: Option<handover::Sealed>,
    bot_username: Option<String>,
    owner_user_id: Option<i64>,
}

/// `MEDHA_SETUP_URL` points at another service, for testing one.
pub fn service() -> String {
    std::env::var("MEDHA_SETUP_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| SERVICE.to_owned())
}

pub async fn begin(service: &str) -> Result<Waiting> {
    let receiver = handover::Receiver::new().map_err(|_| anyhow!("could not make a key"))?;
    let response = client()?
        .post(format!("{service}/v1/telegram/pairings"))
        .json(&json!({"public_key": receiver.public_key()}))
        .send()
        .await
        .context("could not reach Medha Setup")?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Medha Setup refused ({status}): {}",
            response.text().await.unwrap_or_default()
        );
    }
    let started: Started = response
        .json()
        .await
        .context("Medha Setup gave an unreadable answer")?;
    Ok(Waiting {
        link: started.link,
        service: service.to_owned(),
        pairing: started.pairing_id,
        poll_token: started.poll_token,
        receiver: Mutex::new(Some(receiver)),
        ends: Instant::now() + WAIT,
        client: client()?,
    })
}

impl Waiting {
    /// Waits up to ten minutes for the person to create their bot.
    pub async fn wait(&self) -> Result<Handed> {
        self.wait_for(WAIT)
            .await?
            .context("no bot was created within ten minutes; start setup again")
    }

    /// Waits up to `within` for the bot; `None` while the person has not made it yet.
    pub async fn wait_for(&self, within: Duration) -> Result<Option<Handed>> {
        let until = (Instant::now() + within).min(self.ends);
        loop {
            if let Some(handed) = self.poll().await? {
                return Ok(Some(handed));
            }
            if Instant::now() + POLL_EVERY >= until {
                if Instant::now() >= self.ends {
                    bail!("no bot was created within ten minutes; start setup again");
                }
                return Ok(None);
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// How long the person still has to scan and create the bot.
    pub fn remaining(&self) -> Duration {
        self.ends.saturating_duration_since(Instant::now())
    }

    /// Asks the service once; a network hiccup is a quiet poll.
    async fn poll(&self) -> Result<Option<Handed>> {
        let url = format!("{}/v1/telegram/pairings/{}", self.service, self.pairing);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.poll_token)
            .send()
            .await;
        let polled = match response {
            Ok(response) if response.status().is_success() => response.json::<Polled>().await.ok(),
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                bail!("this setup ended; start setup again")
            }
            _ => None,
        };
        let Some(Polled {
            sealed: Some(sealed),
            bot_username: Some(bot),
            owner_user_id: Some(owner),
            ..
        }) = polled.filter(|polled| polled.status == "ready")
        else {
            return Ok(None);
        };
        // The key opens one handover only, and is gone after it.
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .context("this setup already received its bot")?;
        let token = receiver
            .open(&sealed)
            .map_err(|_| anyhow!("the token could not be opened"))?;
        Ok(Some(Handed { token, bot, owner }))
    }
}

/// The link as a QR code made of half-block characters, for a terminal.
pub fn qr(link: &str) -> Option<String> {
    let code = qrcode::QrCode::new(link.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

/// The link as a QR code in SVG, dark on light as every camera reads it, for a window.
pub fn qr_svg(link: &str) -> Option<String> {
    let code = qrcode::QrCode::new(link.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .quiet_zone(true)
            .build(),
    )
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("starting the setup client")
}
