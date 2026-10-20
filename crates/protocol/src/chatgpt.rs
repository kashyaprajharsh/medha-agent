//! ChatGPT plan sign-in and key recognition for every surface. The backend owns the browser
//! callback and the tokens; no answer here ever carries a token.

use crate::{Command, ScopeKind, Secret};
use serde::{Deserialize, Serialize};

/// A registered ChatGPT account as the backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptAccount {
    pub client_id: String,
    pub email: Option<String>,
    pub active: bool,
    pub signed_in: bool,
    pub plan_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptAccounts {}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptAccountList {
    pub accounts: Vec<ChatGptAccount>,
}
impl Command for ChatGptAccounts {
    const METHOD: &'static str = "application.chatgpt.accounts";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptAccountList;
}

/// Open a sign-in: a new account, or `client_id` signing in a registered one again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginChatGptSignIn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Ask again for plan use declined before; never on an ordinary sign-in.
    #[serde(default)]
    pub consent: bool,
}
/// Where to send the person, and the number that names this sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptSignIn {
    pub link: String,
    pub id: u64,
}
impl Command for BeginChatGptSignIn {
    const METHOD: &'static str = "application.chatgpt.begin";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptSignIn;
}

/// Wait for the browser to bring the named sign-in back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishChatGptSignIn {
    pub id: u64,
}
/// The account signed in, or one more pass OpenAI's guide asks for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptSignInOutcome {
    #[serde(default)]
    pub account: Option<ChatGptAccount>,
    #[serde(default)]
    pub again: Option<ChatGptSignIn>,
}
impl Command for FinishChatGptSignIn {
    const METHOD: &'static str = "application.chatgpt.finish";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptSignInOutcome;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelChatGptSignIn {
    pub id: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptCancelled {
    pub cancelled: bool,
}
impl Command for CancelChatGptSignIn {
    const METHOD: &'static str = "application.chatgpt.cancel";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptCancelled;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchChatGptAccount {
    pub client_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptSwitched {
    pub switched: bool,
}
impl Command for SwitchChatGptAccount {
    const METHOD: &'static str = "application.chatgpt.switch";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptSwitched;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignOutChatGpt {
    pub client_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptSignedOut {
    pub signed_out: bool,
    /// Whether OpenAI confirmed the session ended; the local tokens are gone either way.
    pub revoked: bool,
}
impl Command for SignOutChatGpt {
    const METHOD: &'static str = "application.chatgpt.sign_out";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ChatGptSignedOut;
}

/// Name a pasted key's provider from its opening letters; nothing is sent or kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecogniseKey {
    pub key: Secret,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyMatch {
    One,
    Several,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyOwner {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecognisedKey {
    #[serde(rename = "match")]
    pub matched: KeyMatch,
    pub providers: Vec<KeyOwner>,
    /// Shown, but the key may still be used (an organisation admin key).
    #[serde(default)]
    pub warning: Option<String>,
}
impl Command for RecogniseKey {
    const METHOD: &'static str = "application.models.recognise_key";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = RecognisedKey;
}
