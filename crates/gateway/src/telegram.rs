//! Telegram through its Bot API: long polling, so nothing on this computer listens to the
//! internet. The token is part of every request address, so no error ever shows an address.

use crate::platform::{Attachment, Button, Heard, Incoming, Platform, SetupProblem, split};
use crate::telegram_format::to_telegram;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

const API: &str = "https://api.telegram.org";
/// Telegram holds a poll open this long when nothing arrives.
const POLL_SECONDS: u64 = 50;
/// A longer "retry after" is not slept through: one chat must not freeze the others.
const LONGEST_WAIT: Duration = Duration::from_secs(5);
/// Telegram limits each chat to about one message a second.
const PER_CHAT: Duration = Duration::from_secs(1);
/// The longest message, counted as Telegram counts it once the formatting is read.
const LIMIT: usize = 4096;
/// The most a bot may download.
const DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

pub struct Telegram {
    http: reqwest::Client,
    token: String,
    me: Bot,
    offset: AtomicI64,
    last_sent: Mutex<HashMap<String, Instant>>,
}

/// Who the bot is, for mentions in groups.
#[derive(Clone)]
pub struct Bot {
    pub id: i64,
    pub username: String,
}

impl Telegram {
    /// Checks the token and forgets messages sent while the gateway was off.
    pub async fn connect(token: &str) -> Result<Self> {
        let http = client()?;
        let me = check(&http, token).await.map_err(|error| {
            match error
                .downcast_ref::<Refused>()
                .filter(|refused| refused.token())
            {
                Some(refused) => SetupProblem(refused.to_string()).into(),
                None => error,
            }
        })?;
        call(
            &http,
            token,
            "deleteWebhook",
            json!({"drop_pending_updates": true}),
        )
        .await?;
        Ok(Self {
            http,
            token: token.to_owned(),
            me,
            offset: AtomicI64::new(0),
            last_sent: Mutex::default(),
        })
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        call(&self.http, &self.token, method, body).await
    }

    /// Sends or changes one message, keeping to Telegram's pace and waiting out a short flood limit once.
    async fn post(&self, method: &str, chat: &str, body: Value) -> Result<Value> {
        let wait = {
            let mut last = self
                .last_sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let wait = last
                .get(chat)
                .map_or(Duration::ZERO, |at| PER_CHAT.saturating_sub(at.elapsed()));
            last.insert(chat.to_owned(), Instant::now() + wait);
            wait
        };
        tokio::time::sleep(wait).await;
        match self.call(method, body.clone()).await {
            Err(error) => match error
                .downcast_ref::<Refused>()
                .and_then(|refused| refused.retry_after)
            {
                Some(after) if after <= LONGEST_WAIT => {
                    tokio::time::sleep(after).await;
                    self.call(method, body).await
                }
                _ => Err(error),
            },
            sent => sent,
        }
    }

    /// Markdown formatted first; if Telegram cannot read the formatting, the same words plainly.
    async fn text(&self, method: &str, mut body: Value, text: &str) -> Result<Value> {
        let chat = body["chat_id"].as_str().unwrap_or_default().to_owned();
        body["text"] = json!(to_telegram(text).0);
        body["parse_mode"] = json!("HTML");
        body["link_preview_options"] = json!({"is_disabled": true});
        match self.post(method, &chat, body.clone()).await {
            Err(error)
                if error
                    .downcast_ref::<Refused>()
                    .is_some_and(Refused::formatting) =>
            {
                body["text"] = json!(text);
                body.as_object_mut().map(|body| body.remove("parse_mode"));
                self.post(method, &chat, body).await
            }
            sent => sent,
        }
    }

    /// Sends `text`, the last part with `markup`, and gives that part's id.
    async fn message(&self, chat: &str, text: &str, markup: Option<Value>) -> Result<String> {
        let mut parts = Vec::new();
        fitted(text, &mut parts);
        let last = parts.pop().unwrap_or_default();
        for part in parts {
            self.text("sendMessage", json!({"chat_id": chat}), &part)
                .await?;
        }
        let mut body = json!({"chat_id": chat});
        if let Some(markup) = markup {
            body["reply_markup"] = markup;
        }
        let sent = self.text("sendMessage", body, &last).await?;
        sent["message_id"]
            .as_i64()
            .map(|id| id.to_string())
            .context("Telegram gave no message id")
    }

    fn heard(&self, update: &Value) -> Option<Heard> {
        if let Some(pressed) = update.get("callback_query") {
            return Some(Heard::Pressed {
                chat: pressed["message"]["chat"]["id"].as_i64()?.to_string(),
                user: pressed["from"]["id"].as_i64()?.to_string(),
                choice: pressed["data"].as_str()?.to_owned(),
            });
        }
        let message = update.get("message")?;
        let from = &message["from"];
        if from["is_bot"] == true {
            return None;
        }
        let text = message["text"]
            .as_str()
            .or_else(|| message["caption"].as_str())
            .unwrap_or_default();
        Some(Heard::Message(Incoming {
            chat: message["chat"]["id"].as_i64()?.to_string(),
            user: from["id"].as_i64()?.to_string(),
            name: [from["first_name"].as_str(), from["last_name"].as_str()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" "),
            text: text.to_owned(),
            files: attachments(message),
            group: message["chat"]["type"] != "private",
            mentioned: mentions(message, text, &self.me),
        }))
    }
}

#[async_trait::async_trait]
impl Platform for Telegram {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn limit(&self) -> usize {
        LIMIT
    }

    async fn hear(&self) -> Result<Vec<Heard>> {
        let offset = self.offset.load(Ordering::Acquire);
        let updates = self
            .call(
                "getUpdates",
                json!({"offset": offset, "timeout": POLL_SECONDS,
                "allowed_updates": ["message", "callback_query"]}),
            )
            .await?;
        let mut heard = Vec::new();
        for update in updates.as_array().into_iter().flatten() {
            if let Some(id) = update["update_id"].as_i64() {
                self.offset.fetch_max(id + 1, Ordering::AcqRel);
            }
            if let Some(id) = update["callback_query"]["id"].as_str() {
                // Stops the button's spinner; what the press did is said in the chat.
                let _ = self
                    .call("answerCallbackQuery", json!({"callback_query_id": id}))
                    .await;
            }
            heard.extend(self.heard(update));
        }
        Ok(heard)
    }

    async fn send(&self, chat: &str, text: &str) -> Result<()> {
        self.message(chat, text, None).await.map(|_| ())
    }

    async fn ask(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<Option<String>> {
        let rows: Vec<Value> = buttons
            .iter()
            .map(|button| json!([{"text": button.label, "callback_data": button.choice}]))
            .collect();
        self.message(chat, text, Some(json!({"inline_keyboard": rows})))
            .await
            .map(Some)
    }

    async fn send_live(&self, chat: &str, text: &str) -> Result<Option<String>> {
        self.message(chat, text, None).await.map(Some)
    }

    /// Leaving out `reply_markup` takes the buttons away.
    async fn edit(&self, chat: &str, message: &str, text: &str) -> Result<()> {
        let body = json!({"chat_id": chat, "message_id": message.parse::<i64>()?});
        match self.text("editMessageText", body, text).await {
            Err(error)
                if error
                    .downcast_ref::<Refused>()
                    .is_some_and(Refused::unchanged) =>
            {
                Ok(())
            }
            edited => edited.map(|_| ()),
        }
    }

    /// In every scope, since Telegram shows the narrowest one that matches a chat.
    async fn offer_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let commands: Vec<Value> = commands
            .iter()
            .map(|(command, description)| json!({"command": command, "description": description}))
            .collect();
        for scope in ["default", "all_private_chats", "all_group_chats"] {
            self.call(
                "setMyCommands",
                json!({"commands": commands, "scope": {"type": scope}}),
            )
            .await?;
        }
        Ok(())
    }

    async fn typing(&self, chat: &str) -> Result<()> {
        self.call(
            "sendChatAction",
            json!({"chat_id": chat, "action": "typing"}),
        )
        .await
        .map(|_| ())
    }

    async fn fetch(&self, attachment: &Attachment) -> Result<Vec<u8>> {
        let file = self
            .call("getFile", json!({"file_id": attachment.id}))
            .await?;
        let path = file["file_path"]
            .as_str()
            .context("Telegram gave no file path")?;
        let url = format!("{API}/file/bot{}/{path}", self.token);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(hidden)?
            .error_for_status()
            .map_err(hidden)?;
        if response
            .content_length()
            .is_some_and(|length| length > DOWNLOAD_BYTES)
        {
            bail!("the file is larger than Telegram lets a bot download");
        }
        Ok(response.bytes().await.map_err(hidden)?.to_vec())
    }

    async fn send_file(&self, chat: &str, path: &Path) -> Result<()> {
        let name = path
            .file_name()
            .map_or_else(|| "file".into(), |name| name.to_string_lossy().into_owned());
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat.to_owned())
            .part(
                "document",
                reqwest::multipart::Part::bytes(bytes).file_name(name),
            );
        let url = format!("{API}/bot{}/sendDocument", self.token);
        let response = self
            .http
            .post(url)
            .multipart(form)
            .send()
            .await
            .map_err(hidden)?;
        answer(response).await.map(|_| ())
    }
}

/// The bot behind `token`, or why Telegram refused it.
pub async fn check(http: &reqwest::Client, token: &str) -> Result<Bot> {
    let me = call(http, token, "getMe", json!({})).await?;
    Ok(Bot {
        id: me["id"].as_i64().context("Telegram gave no bot id")?,
        username: me["username"]
            .as_str()
            .context("Telegram gave no bot name")?
            .to_owned(),
    })
}

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(POLL_SECONDS + 20))
        .build()
        .context("starting the Telegram client")
}

async fn call(http: &reqwest::Client, token: &str, method: &str, body: Value) -> Result<Value> {
    let url = format!("{API}/bot{token}/{method}");
    let response = http.post(url).json(&body).send().await.map_err(hidden)?;
    answer(response).await
}

async fn answer(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.map_err(hidden)?;
    if body["ok"] == true {
        return Ok(body["result"].clone());
    }
    Err(Refused {
        code: body["error_code"]
            .as_u64()
            .unwrap_or(u64::from(status.as_u16())),
        description: body["description"]
            .as_str()
            .unwrap_or("no reason given")
            .to_owned(),
        retry_after: body["parameters"]["retry_after"]
            .as_u64()
            .map(Duration::from_secs),
    }
    .into())
}

/// What Telegram said when it refused a request.
#[derive(Debug)]
struct Refused {
    code: u64,
    description: String,
    retry_after: Option<Duration>,
}

impl Refused {
    /// Telegram answers a revoked token with 401 and a malformed one with 404.
    fn token(&self) -> bool {
        matches!(self.code, 401 | 404)
    }

    fn formatting(&self) -> bool {
        self.code == 400 && self.description.contains("can't parse entities")
    }

    /// An edit to the same words; nothing needed changing.
    fn unchanged(&self) -> bool {
        self.code == 400 && self.description.contains("message is not modified")
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            401 | 404 => write!(
                f,
                "Telegram does not accept this bot token; make a new one with @BotFather"
            ),
            409 => write!(
                f,
                "another program is reading this bot's messages; stop the other Medha gateway or bot"
            ),
            _ => write!(f, "Telegram refused ({}): {}", self.code, self.description),
        }
    }
}

impl std::error::Error for Refused {}

/// A network error without the request address, which holds the token.
fn hidden(error: reqwest::Error) -> anyhow::Error {
    anyhow!("could not reach Telegram: {}", error.without_url())
}

fn attachments(message: &Value) -> Vec<Attachment> {
    let mut files = Vec::new();
    // Telegram sends each photo in several sizes; the last is the largest.
    if let Some(photo) = message["photo"].as_array().and_then(|sizes| sizes.last()) {
        files.extend(photo["file_id"].as_str().map(|id| Attachment {
            id: id.to_owned(),
            name: "photo.jpg".into(),
            mime: "image/jpeg".into(),
            size: photo["file_size"].as_u64(),
        }));
    }
    for kind in ["document", "audio", "video", "voice"] {
        let file = &message[kind];
        if let Some(id) = file["file_id"].as_str() {
            files.push(Attachment {
                id: id.to_owned(),
                name: file["file_name"].as_str().unwrap_or(kind).to_owned(),
                mime: file["mime_type"]
                    .as_str()
                    .unwrap_or("application/octet-stream")
                    .to_owned(),
                size: file["file_size"].as_u64(),
            });
        }
    }
    files
}

/// In a group, the bot answers only when named, replied to, or given a command meant for it.
fn mentions(message: &Value, text: &str, me: &Bot) -> bool {
    let handle = format!("@{}", me.username.to_lowercase());
    let named = message["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entity| match entity["type"].as_str() {
            Some("mention") => {
                entity_text(text, entity).is_some_and(|said| said.to_lowercase() == handle)
            }
            Some("text_mention") => entity["user"]["id"].as_i64() == Some(me.id),
            Some("bot_command") => {
                entity_text(text, entity).is_some_and(|said| said.to_lowercase().ends_with(&handle))
            }
            _ => false,
        });
    named || message["reply_to_message"]["from"]["id"].as_i64() == Some(me.id)
}

/// Telegram counts entity offsets in UTF-16 units.
fn entity_text(text: &str, entity: &Value) -> Option<String> {
    let start = usize::try_from(entity["offset"].as_u64()?).ok()?;
    let length = usize::try_from(entity["length"].as_u64()?).ok()?;
    let units: Vec<u16> = text.encode_utf16().collect();
    String::from_utf16(units.get(start..start + length)?).ok()
}

/// Parts whose formatted text fits a message: a drawn table adds words, so a part that fit as
/// typed may not once formatted.
fn fitted(text: &str, parts: &mut Vec<String>) {
    let length = text.chars().count();
    if length < 2 || to_telegram(text).1 <= LIMIT {
        return parts.push(text.to_owned());
    }
    for part in split(text, length.div_ceil(2)) {
        fitted(&part, parts);
    }
}

#[cfg(test)]
#[path = "telegram_tests.rs"]
mod tests;
