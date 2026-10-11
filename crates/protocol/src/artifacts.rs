//! Generated outputs are a projection of durable source events. Labels are
//! presentation, never identity. Revisions retain their exact file manifests.
use crate::{Command, ScopeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactAsset {
    pub path: String,
    /// Set when the asset's bytes were kept, as older revisions did. Without
    /// it the asset is read from the document's folder as it is now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub size: usize,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSnapshot {
    pub path: String,
    pub kind: String,
    pub hash: String,
    pub size: usize,
    #[serde(default)]
    pub assets: Vec<ArtifactAsset>,
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRevision {
    pub artifact_id: String,
    pub revision_id: String,
    pub predecessor: Option<String>,
    pub source_event: String,
    pub source_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<String>,
    pub ts: f64,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub inline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ArtifactSnapshot>,
    /// A file shown where it lies, as it is now: no copy was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<Value>,
}

/// The saved output a message is about. The first output of that kind in the
/// reply is recorded as its next revision. Unknown or foreign IDs are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputReference {
    pub artifact_id: String,
    pub revision_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListArtifacts {
    pub session_id: String,
    #[serde(default)]
    pub cursor: usize,
}
impl Command for ListArtifacts {
    const METHOD: &'static str = "application.artifacts";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ArtifactPage;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPage {
    pub revisions: Vec<ArtifactRevision>,
    pub next_cursor: Option<usize>,
    pub omitted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArtifact {
    pub session_id: String,
    pub revision_id: String,
    pub asset: Option<String>,
    #[serde(default)]
    pub offset: usize,
}
impl Command for ReadArtifact {
    const METHOD: &'static str = "application.artifacts.read";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ArtifactBytes;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactBytes {
    pub data: String,
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile {
    pub session_id: String,
    pub revision_id: String,
}
impl Command for ArtifactFile {
    const METHOD: &'static str = "application.artifacts.file";
    const SCOPE: ScopeKind = ScopeKind::Folder;
    type Output = ArtifactSnapshot;
}
