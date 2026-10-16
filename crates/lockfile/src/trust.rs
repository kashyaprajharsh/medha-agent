//! Repository config asks for privilege; only a machine-local record grants it.
//!
//! `medha.lock` ships inside a checkout, so it is untrusted input. Tuning
//! values apply freely, but settings that *relax* the safety floor are ignored
//! until the operator accepts them for this workspace — the same shape as an
//! approval-gated project language server.

use serde::{Deserialize, Serialize};

use crate::MedhaLock;

/// One privilege-relaxing setting a repository asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskySetting {
    /// Dotted key, e.g. `sandbox.backend`.
    pub key: String,
    /// What the repository asked for, rendered for display and fingerprinting.
    pub value: String,
    /// Why it matters, shown verbatim to the operator.
    pub effect: &'static str,
}

impl std::fmt::Display for RiskySetting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} = {} — {}", self.key, self.value, self.effect)
    }
}

fn relaxes_autonomy(value: &str) -> bool {
    matches!(value.trim().to_lowercase().as_str(), "normal" | "yolo")
}

fn selects_external_executor(value: &str) -> bool {
    value.trim().to_lowercase() != "native"
}

fn opens_network(value: &str) -> bool {
    matches!(value.trim().to_lowercase().as_str(), "allow" | "on")
}

/// Default-gated tools this lock removed from the approval list.
fn dropped_approvals(approve: &[String]) -> Vec<String> {
    crate::default_approve()
        .into_iter()
        .filter(|tool| !approve.contains(tool))
        .collect()
}

impl MedhaLock {
    /// Every privilege-relaxing setting this lock asks for, in a stable order.
    pub fn risky_settings(&self) -> Vec<RiskySetting> {
        let mut found = Vec::new();
        let mut note = |key: &str, value: String, effect: &'static str| {
            found.push(RiskySetting {
                key: key.to_string(),
                value,
                effect,
            });
        };
        if relaxes_autonomy(&self.policy.autonomy) {
            note(
                "policy.autonomy",
                self.policy.autonomy.clone(),
                "weakens the approval floor this workspace starts at",
            );
        }
        let dropped = dropped_approvals(&self.policy.approve);
        if !dropped.is_empty() {
            note(
                "policy.approve",
                format!("{:?}", self.policy.approve),
                "stops these tools asking before they run",
            );
        }
        if selects_external_executor(&self.sandbox.backend) {
            note(
                "sandbox.backend",
                self.sandbox.backend.clone(),
                "selects a repository-controlled command execution environment",
            );
        }
        if opens_network(&self.sandbox.network) {
            note(
                "sandbox.network",
                self.sandbox.network.clone(),
                "lets confined commands reach the network",
            );
        }
        if !self.sandbox.extra_writable.is_empty() {
            note(
                "sandbox.extra_writable",
                format!("{:?}", self.sandbox.extra_writable),
                "widens the jail beyond the workspace",
            );
        }
        for (key, value, effect) in [
            (
                "sandbox.image",
                self.sandbox.image.as_deref(),
                "selects repository-provided code used by the container executor",
            ),
            (
                "sandbox.runtime",
                self.sandbox.runtime.as_deref(),
                "selects the host program used to start a container",
            ),
            (
                "sandbox.host",
                self.sandbox.host.as_deref(),
                "selects the remote machine that receives commands",
            ),
            (
                "sandbox.remote_dir",
                self.sandbox.remote_dir.as_deref(),
                "selects the remote directory in which commands run",
            ),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                note(key, value.to_string(), effect);
            }
        }
        if let Some(command) = self
            .verify
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
        {
            note(
                "verify.command",
                command.to_string(),
                "runs this shell command after local-effect turns",
            );
        }
        if self.memory.write_approval == "none" {
            note(
                "memory.write_approval",
                self.memory.write_approval.clone(),
                "allows personal memory writes without approval",
            );
        }
        if self.lsp.allow_network {
            note(
                "lsp.allow_network",
                "true".into(),
                "allows language servers to reach the network",
            );
        }
        if !self.lsp.servers.is_empty() {
            note(
                "lsp.servers",
                toml::Value::try_from(&self.lsp.servers)
                    .expect("LSP servers serialize")
                    .to_string(),
                "provides language-server commands or executable project settings",
            );
        }
        for (key, value) in [
            ("pricing.input_per_mtok", self.pricing.input_per_mtok),
            ("pricing.output_per_mtok", self.pricing.output_per_mtok),
            (
                "pricing.cached_input_per_mtok",
                self.pricing.cached_input_per_mtok,
            ),
            (
                "pricing.cache_write_per_mtok",
                self.pricing.cache_write_per_mtok,
            ),
        ] {
            if let Some(value) = value {
                note(key, value.to_string(), "changes the cost meter's rates");
            }
        }
        found
    }

    pub fn legacy_risky_settings(&self) -> Vec<RiskySetting> {
        self.risky_settings()
            .into_iter()
            .filter_map(|mut setting| {
                if setting.key.starts_with("pricing.")
                    || setting.key.starts_with("lsp.")
                    || setting.key == "memory.write_approval"
                {
                    return None;
                }
                if setting.key == "policy.approve" {
                    setting.value = format!("drops {:?}", dropped_approvals(&self.policy.approve));
                }
                if matches!(
                    setting.key.as_str(),
                    "sandbox.image"
                        | "sandbox.runtime"
                        | "sandbox.host"
                        | "sandbox.remote_dir"
                        | "verify.command"
                ) {
                    setting.value = setting.value.trim().to_owned();
                }
                Some(setting)
            })
            .collect()
    }

    /// A copy with every privilege-relaxing setting returned to its default.
    pub fn without_risky_settings(&self) -> Self {
        let mut safe = self.clone();
        if relaxes_autonomy(&safe.policy.autonomy) {
            safe.policy.autonomy = crate::default_autonomy();
        }
        for tool in dropped_approvals(&safe.policy.approve) {
            safe.policy.approve.push(tool);
        }
        let defaults = crate::SandboxLockConfig::default();
        if selects_external_executor(&safe.sandbox.backend) {
            safe.sandbox.backend = defaults.backend;
        }
        if opens_network(&safe.sandbox.network) {
            safe.sandbox.network = defaults.network;
        }
        safe.sandbox.extra_writable = Vec::new();
        safe.sandbox.image = None;
        safe.sandbox.runtime = None;
        safe.sandbox.host = None;
        safe.sandbox.remote_dir = None;
        safe.verify.command = None;
        if safe.memory.write_approval == "none" {
            safe.memory.write_approval = crate::MemoryConfig::default().write_approval;
        }
        safe.lsp.allow_network = false;
        safe.lsp.servers.clear();
        safe.pricing = crate::PricingConfig::default();
        safe
    }

    pub fn with_accepted_settings(&self, accepted: &[RiskySetting]) -> Self {
        let mut safe = self.without_risky_settings();
        for setting in self
            .risky_settings()
            .iter()
            .filter(|setting| accepted.contains(setting))
        {
            match setting.key.as_str() {
                "policy.autonomy" => safe.policy.autonomy.clone_from(&self.policy.autonomy),
                "policy.approve" => safe.policy.approve.clone_from(&self.policy.approve),
                "sandbox.backend" => safe.sandbox.backend.clone_from(&self.sandbox.backend),
                "sandbox.network" => safe.sandbox.network.clone_from(&self.sandbox.network),
                "sandbox.extra_writable" => safe
                    .sandbox
                    .extra_writable
                    .clone_from(&self.sandbox.extra_writable),
                "sandbox.image" => safe.sandbox.image.clone_from(&self.sandbox.image),
                "sandbox.runtime" => safe.sandbox.runtime.clone_from(&self.sandbox.runtime),
                "sandbox.host" => safe.sandbox.host.clone_from(&self.sandbox.host),
                "sandbox.remote_dir" => {
                    safe.sandbox.remote_dir.clone_from(&self.sandbox.remote_dir)
                }
                "verify.command" => safe.verify.command.clone_from(&self.verify.command),
                "memory.write_approval" => safe
                    .memory
                    .write_approval
                    .clone_from(&self.memory.write_approval),
                "lsp.allow_network" => safe.lsp.allow_network = self.lsp.allow_network,
                "lsp.servers" => safe.lsp.servers.clone_from(&self.lsp.servers),
                "pricing.input_per_mtok" => {
                    safe.pricing.input_per_mtok = self.pricing.input_per_mtok
                }
                "pricing.output_per_mtok" => {
                    safe.pricing.output_per_mtok = self.pricing.output_per_mtok
                }
                "pricing.cached_input_per_mtok" => {
                    safe.pricing.cached_input_per_mtok = self.pricing.cached_input_per_mtok
                }
                "pricing.cache_write_per_mtok" => {
                    safe.pricing.cache_write_per_mtok = self.pricing.cache_write_per_mtok
                }
                _ => {}
            }
        }
        safe
    }
}

/// Stable identity of one risky set, so unrelated tuning edits do not re-prompt.
pub fn fingerprint(settings: &[RiskySetting]) -> String {
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for setting in settings {
        sha2::Digest::update(&mut hasher, (setting.key.len() as u64).to_be_bytes());
        sha2::Digest::update(&mut hasher, setting.key.as_bytes());
        sha2::Digest::update(&mut hasher, (setting.value.len() as u64).to_be_bytes());
        sha2::Digest::update(&mut hasher, setting.value.as_bytes());
    }
    let digest = sha2::Digest::finalize(hasher);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Machine-local record of accepted lockfile privilege, keyed by workspace.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AcceptedLocks {
    #[serde(default)]
    accepted: std::collections::BTreeMap<String, String>,
}

impl AcceptedLocks {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn allows(&self, workspace: &str, settings: &[RiskySetting]) -> bool {
        settings.is_empty() || self.accepted.get(workspace) == Some(&fingerprint(settings))
    }

    pub fn accept(&mut self, workspace: &str, settings: &[RiskySetting]) {
        self.accepted
            .insert(workspace.to_string(), fingerprint(settings));
    }

    pub fn revoke(&mut self, workspace: &str) {
        self.accepted.remove(workspace);
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod trust_tests;
