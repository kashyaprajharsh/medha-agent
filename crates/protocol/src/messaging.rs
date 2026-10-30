//! Messaging setup for every surface: reaching Medha from a phone. The backend holds the bot's token
//! from the moment it arrives; no answer here ever carries one.

use crate::{Command, ScopeKind, Secret};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagingStatus {}
/// What is set up, whether the gateway runs, and who may talk to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingOverview {
    /// The folder a chat works in until it picks another project at `/new`.
    pub folder: Option<PathBuf>,
    /// Every folder a chat may pick, the default first; only these are reachable from a phone.
    #[serde(default)]
    pub folders: Vec<PathBuf>,
    /// Tool steps are shown in the chat as the work goes on.
    #[serde(default)]
    pub steps: bool,
    pub running: bool,
    /// Starts at login and after a crash.
    pub installed: bool,
    pub platforms: Vec<MessagingPlatform>,
    pub people: Vec<MessagingPerson>,
    pub waiting: Vec<MessagingRequest>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingPlatform {
    pub name: String,
    pub label: String,
    /// Set up and switched on, with its token saved.
    pub ready: bool,
    /// The bot people message, such as `@medha_bot`, when setup recorded it.
    pub account: Option<String>,
    /// Notices go to a chat on this platform.
    pub home: bool,
    /// How to make a bot by hand, shown before a token is pasted.
    pub steps: String,
    /// What the pasted secret is called, such as "Bot token".
    pub token: String,
    /// Can be made by scanning a code instead of pasting a token.
    pub one_tap: bool,
    /// Made by scanning: only its owner can message it, enforced by the platform itself, so nobody
    /// else can be let in; sharing Medha takes a bot made by hand.
    #[serde(default)]
    pub owner_only: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingPerson {
    pub platform: String,
    pub user: String,
    pub name: String,
}
/// Someone who messaged the bot and was given a code to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingRequest {
    pub platform: String,
    pub user: String,
    pub name: String,
    pub minutes: u64,
}
impl Command for MessagingStatus {
    const METHOD: &'static str = "application.messaging.status";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingOverview;
}

/// Start making a bot by scanning a code; a newer start replaces an older one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginMessagingSetup {
    pub platform: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingSetup {
    pub id: u64,
    pub link: String,
    /// The link as a code of half-block characters, for a terminal.
    pub qr_text: String,
    /// The link as a code in SVG, dark on light, for a window.
    pub qr_svg: String,
    pub expires_in: u64,
}
impl Command for BeginMessagingSetup {
    const METHOD: &'static str = "application.messaging.begin";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingSetup;
}

/// Waits a little for the setup's next step and says where it stands; asked again until `heard`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwaitMessagingSetup {
    pub id: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MessagingStep {
    /// Not scanned yet, or the bot not created yet.
    Waiting { expires_in: u64 },
    /// The bot exists; the owner's first message to it is awaited.
    Created { bot: String },
    /// The first message came from the creator; the person confirms it is them.
    Heard {
        bot: String,
        name: String,
        user: String,
    },
}
impl Command for AwaitMessagingSetup {
    const METHOD: &'static str = "application.messaging.await";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingStep;
}

/// The person confirmed it is them: save the bot, pair them and start the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveMessagingSetup {
    pub id: u64,
    /// Where messaging chats work; the saved folder, then the open one, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagingSaved {
    pub account: String,
    pub folder: PathBuf,
    /// The owner was paired, so nobody needs to approve a code.
    pub paired: bool,
    /// Saved, but the gateway could not be started at login, and why.
    #[serde(default)]
    pub problem: Option<String>,
}
impl Command for SaveMessagingSetup {
    const METHOD: &'static str = "application.messaging.save";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingSaved;
}

/// A bot made by hand: its token is checked with the platform, then saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveMessagingToken {
    pub platform: String,
    pub token: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}
impl Command for SaveMessagingToken {
    const METHOD: &'static str = "application.messaging.save_token";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingSaved;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMessagingSetup {
    pub id: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagingCancelled {
    pub cancelled: bool,
}
impl Command for CancelMessagingSetup {
    const METHOD: &'static str = "application.messaging.cancel";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingCancelled;
}

/// Let in someone who was given a code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveMessagingPerson {
    pub platform: String,
    pub code: String,
}
impl Command for ApproveMessagingPerson {
    const METHOD: &'static str = "application.messaging.approve";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingPerson;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeMessagingPerson {
    pub platform: String,
    pub user: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagingRevoked {
    pub revoked: bool,
}
impl Command for RevokeMessagingPerson {
    const METHOD: &'static str = "application.messaging.revoke";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingRevoked;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderChange {
    /// New chats start here; the folder is added if it was not yet.
    Default,
    Add,
    Remove,
}
/// Which folders a phone may work in, changed only on this computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeMessagingFolder {
    pub change: FolderChange,
    pub folder: PathBuf,
}
impl Command for ChangeMessagingFolder {
    const METHOD: &'static str = "application.messaging.folder";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingOverview;
}

/// Show or hide tool steps in messaging chats; off until chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowMessagingSteps {
    pub show: bool,
}
impl Command for ShowMessagingSteps {
    const METHOD: &'static str = "application.messaging.steps";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingOverview;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayAction {
    Start,
    Stop,
}
/// Start or stop the gateway that runs at login.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlGateway {
    pub action: GatewayAction,
}
impl Command for ControlGateway {
    const METHOD: &'static str = "application.messaging.gateway";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = MessagingOverview;
}
