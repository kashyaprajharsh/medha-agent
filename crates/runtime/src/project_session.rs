//! Refresh project authority between requests without involving an approval gate.

use crate::{SessionOptions, project_trust::ProjectTrust};
use kernel::{AutonomyLevel, BlastRadius, Decision, Policy, StreamSink, ToolIntent};
use lockfile::MedhaLock;
use sandbox::{ExecBackend, ExecError, ExecRequest};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub struct LiveSettings {
    folder: PathBuf,
    options: SessionOptions,
    lock: RwLock<SettingsState>,
    home: sandbox::HomeScope,
    approved: sandbox::ApprovedRoots,
    network: sandbox::NetworkGrant,
    scratch: Option<PathBuf>,
    operator_mode: Mutex<Option<AutonomyLevel>>,
    children: Mutex<HashMap<PathBuf, std::sync::Weak<LiveSettings>>>,
}

#[derive(Clone)]
struct SettingsState {
    lock: MedhaLock,
    raw: MedhaLock,
    config: sandbox::SandboxConfig,
    backend: Arc<dyn ExecBackend>,
    generation: u64,
}
impl std::ops::Deref for SettingsState {
    type Target = MedhaLock;
    fn deref(&self) -> &MedhaLock {
        &self.lock
    }
}

impl LiveSettings {
    pub fn new(
        folder: PathBuf,
        options: SessionOptions,
        lock: MedhaLock,
        home: sandbox::HomeScope,
        approved: sandbox::ApprovedRoots,
        network: sandbox::NetworkGrant,
        scratch: Option<PathBuf>,
    ) -> Arc<Self> {
        let state = Self::build_state(lock, &options, &home, &approved, &network);
        Arc::new(Self {
            folder,
            options,
            lock: RwLock::new(state),
            home,
            approved,
            network,
            scratch,
            operator_mode: Mutex::default(),
            children: Mutex::default(),
        })
    }

    pub fn refresh(&self) {
        let raw = MedhaLock::load(self.folder.join("medha.lock"))
            .ok()
            .flatten()
            .unwrap_or_default();
        let safe = match ProjectTrust::for_folder(&self.folder) {
            Ok(Some(trust)) => trust.filter(&self.folder, &raw),
            Ok(None) => crate::workspace::load_lock(&self.folder, &Quiet)
                .unwrap_or_else(|_| raw.without_risky_settings()),
            Err(_) => raw.without_risky_settings(),
        };
        let mut next = Self::build_state(
            safe,
            &self.options,
            &self.home,
            &self.approved,
            &self.network,
        );
        next.raw = raw;
        let mut current = self
            .lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        next.generation = current.generation.wrapping_add(1);
        *current = next;
    }

    fn scoped(self: &Arc<Self>, folder: &Path) -> Arc<Self> {
        let folder = folder
            .canonicalize()
            .unwrap_or_else(|_| folder.to_path_buf());
        let mut children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if folder != self.folder
            && let Some(existing) = children.get(&folder).and_then(std::sync::Weak::upgrade)
        {
            return existing;
        }
        let scoped = Self::new(
            folder.to_path_buf(),
            self.options.clone(),
            crate::workspace::load_lock(&folder, &Quiet).unwrap_or_default(),
            self.home.clone(),
            self.approved.clone(),
            self.network.clone(),
            self.scratch.clone(),
        );
        *scoped
            .operator_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = *self
            .operator_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        children.retain(|_, child| child.strong_count() > 0);
        if folder != self.folder {
            children.insert(folder, Arc::downgrade(&scoped));
        }
        scoped
    }

    fn build_state(
        lock: MedhaLock,
        options: &SessionOptions,
        home: &sandbox::HomeScope,
        approved: &sandbox::ApprovedRoots,
        network: &sandbox::NetworkGrant,
    ) -> SettingsState {
        let mut config = lock.sandbox.to_config();
        config.home = home.clone();
        if let Some(backend) = options.sandbox {
            config.backend = backend;
        }
        let backend = sandbox::select_backend(
            &config,
            lock.sandbox.extra_writable_paths(),
            approved.clone(),
            network.clone(),
        );
        SettingsState {
            raw: lock.clone(),
            lock,
            config,
            backend,
            generation: 0,
        }
    }

    fn backend(&self) -> Arc<dyn ExecBackend> {
        self.lock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .backend
            .clone()
    }

    pub fn exec(self: &Arc<Self>) -> Arc<dyn ExecBackend> {
        Arc::new(LiveExec {
            settings: self.clone(),
            folder: None,
            scoped: Mutex::default(),
        })
    }
}

impl Policy for LiveSettings {
    fn operator_autonomy(&self, autonomy: AutonomyLevel) {
        *self
            .operator_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(autonomy);
    }
    fn authorize(
        &self,
        autonomy: AutonomyLevel,
        intent: &ToolIntent,
        radius: Option<BlastRadius>,
    ) -> Decision {
        if intent.tool == "lsp.start" && !crate::project_trust::content_allowed(&self.folder) {
            return Decision::Deny {
                reason: "Project language servers are disabled; run /trust to enable them.".into(),
            };
        }
        let lock = self
            .lock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mode = self
            .operator_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .or(self.options.autonomy)
            .unwrap_or_else(|| {
                AutonomyLevel::parse(&lock.policy.autonomy).unwrap_or(AutonomyLevel::Careful)
            });
        let mode = if autonomy == AutonomyLevel::Plan {
            autonomy
        } else {
            mode
        };
        let jail = match lock.backend.containment() {
            kernel::Containment::OsFsJail | kernel::Containment::OsFsJailNoNet => {
                policy::Jail::Active
            }
            _ if lock.config.backend == sandbox::BackendKind::Native
                && !sandbox::native_backend_available() =>
            {
                policy::Jail::Missing
            }
            _ => policy::Jail::Off,
        };
        let mut policy =
            policy::DefaultPolicy::requiring_approval(crate::approvals::approve_list_from(
                lock.policy.approve.clone(),
                self.options.approve.as_deref().unwrap_or_default(),
            ))
            .with_workspace(&self.folder)
            .with_memory_write_approval(&lock.memory.write_approval)
            .with_jail(jail);
        if let Some(scratch) = &self.scratch {
            policy = policy.with_scratch(scratch);
        }
        policy.authorize(mode, intent, radius)
    }
}

struct LiveExec {
    settings: Arc<LiveSettings>,
    folder: Option<PathBuf>,
    scoped: Mutex<Option<(u64, Arc<dyn ExecBackend>)>>,
}
impl LiveExec {
    fn backend(&self) -> Arc<dyn ExecBackend> {
        let current = self
            .settings
            .lock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(folder) = &self.folder else {
            return current.backend.clone();
        };
        let mut scoped = self
            .scoped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((generation, backend)) = scoped.as_ref()
            && *generation == current.generation
        {
            return backend.clone();
        }
        let backend = current
            .backend
            .scoped_to(folder)
            .unwrap_or_else(|| current.backend.clone());
        *scoped = Some((current.generation, backend.clone()));
        backend
    }
}
#[async_trait::async_trait]
impl ExecBackend for LiveExec {
    fn protect(
        &self,
        request: &ExecRequest,
    ) -> Result<sandbox::command_resources::CommandResources, ExecError> {
        self.backend().protect(request)
    }
    fn build_command(&self, request: &ExecRequest) -> Result<tokio::process::Command, ExecError> {
        self.backend().build_command(request)
    }
    fn spawn_owned(
        &self,
        request: &ExecRequest,
        dir: Option<&Path>,
        limits: sandbox::exec::CaptureLimits,
    ) -> Result<sandbox::exec::BgProc, ExecError> {
        self.backend().spawn_owned(request, dir, limits)
    }
    fn requires_strict_cleanup(&self) -> bool {
        self.backend().requires_strict_cleanup()
    }
    fn build_plugin_command(
        &self,
        request: &ExecRequest,
        package: &Path,
        data: &Path,
    ) -> Result<tokio::process::Command, ExecError> {
        self.backend().build_plugin_command(request, package, data)
    }
    fn scoped_to(&self, folder: &Path) -> Option<Arc<dyn ExecBackend>> {
        let settings = self.settings.scoped(folder);
        settings.backend().scoped_to(folder).map(|_| {
            Arc::new(Self {
                settings,
                folder: Some(folder.to_path_buf()),
                scoped: Mutex::default(),
            }) as Arc<dyn ExecBackend>
        })
    }
    fn label(&self) -> &str {
        match self.backend().label() {
            "host" => "host",
            "container" => "container",
            "ssh" => "ssh",
            "unavailable" => "unavailable",
            _ => "native",
        }
    }
    fn containment(&self) -> kernel::Containment {
        self.backend().containment()
    }
    fn denies_network(&self, request: &ExecRequest) -> bool {
        self.backend().denies_network(request)
    }
}

pub struct LiveContext {
    pub settings: Arc<LiveSettings>,
    skills: Arc<tools::SkillStore>,
    known: HashSet<String>,
    prompt: Option<String>,
    slot: Mutex<Option<context::identity::ProjectSlot>>,
    cached: Mutex<ProjectContent>,
    notices: Mutex<BTreeSet<String>>,
    fallback: Option<kernel::Pricing>,
}

struct ProjectContent {
    project: String,
    summary: String,
}

impl LiveContext {
    pub fn new(
        settings: Arc<LiveSettings>,
        skills: Arc<tools::SkillStore>,
        known: HashSet<String>,
        prompt: Option<String>,
        fallback: Option<kernel::Pricing>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            skills,
            known,
            prompt,
            slot: Mutex::default(),
            cached: Mutex::new(ProjectContent {
                project: String::new(),
                summary: context::prompts::compaction_summary(),
            }),
            notices: Mutex::default(),
            fallback,
        })
    }

    pub fn summary_prompt(&self) -> String {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .summary
            .clone()
    }

    pub fn bind_project_slot(&self, slot: context::identity::ProjectSlot) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(slot);
    }

    pub fn replace_memory(&self, memory: &str) -> String {
        let content = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = slot
            .as_mut()
            .expect("prompt builder binds the project slot before memory");
        slot.set_tail(memory::recall::append_k3("", memory));
        slot.render(&content.project)
    }

    fn notice_once(&self, key: String, text: &str, sink: &dyn StreamSink) {
        if self
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
        {
            sink.notice(text);
        }
    }
}

impl kernel::context::RequestContext for LiveContext {
    fn owns_memory_slot(&self) -> bool {
        true
    }
    fn fork(
        &self,
        workspace: Option<&Path>,
        tools: HashSet<String>,
    ) -> Option<Arc<dyn kernel::context::RequestContext>> {
        let settings = self
            .settings
            .scoped(workspace.unwrap_or(&self.settings.folder));
        let skills = Arc::new(self.skills.rebase(settings.folder.join(".medha/skills")));
        let context = Self::new(settings, skills, tools, self.prompt.clone(), self.fallback);
        *context
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Some(context)
    }

    fn policy(&self) -> Option<Arc<dyn Policy>> {
        Some(self.settings.clone())
    }
    fn summary_prompt(&self) -> Option<String> {
        Some(LiveContext::summary_prompt(self))
    }
    fn refresh_memory(&self, memory: &str) -> Option<String> {
        Some(self.replace_memory(memory))
    }

    fn begin_message(
        &self,
        events: &[kernel::Event],
        sink: &dyn StreamSink,
    ) -> Vec<(String, serde_json::Value)> {
        self.settings.refresh();
        {
            let mut seen = self
                .notices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for event in events
                .iter()
                .filter(|event| event.kind == kernel::EventKind::ProjectTrustNotice)
            {
                if let Some(key) = event.payload["key"].as_str() {
                    seen.insert(key.to_owned());
                }
            }
        }
        let mut pending = Vec::new();
        let mut project = String::new();
        let mut summary = context::prompts::compaction_summary();
        let discovery = self.skills.discover(&self.known);
        if let Ok(Some(trust)) = ProjectTrust::for_folder(&self.settings.folder) {
            if !trust.trusted() {
                let key = format!("folder:{}", trust.root.display());
                let raw = self
                    .settings
                    .lock
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .raw
                    .clone();
                if has_disabled_content(&self.settings.folder, &raw, discovery.project_present)
                    && self
                        .notices
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key.clone())
                    && let Ok(review) = trust.review(&self.settings.folder)
                {
                    pending.push((
                        key,
                        serde_json::to_value(review).expect("trust review serializes"),
                    ));
                }
            } else if let Ok(review) = trust.review(&self.settings.folder) {
                let safe = self
                    .settings
                    .lock
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let ignored = review
                    .settings
                    .iter()
                    .filter(|setting| !safe.risky_settings().contains(setting))
                    .map(|setting| format!("{} = {}", setting.key, setting.value))
                    .collect::<Vec<_>>();
                if !ignored.is_empty()
                    && self
                        .notices
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(format!("settings:{}", review.fingerprint))
                {
                    pending.push((
                        format!("settings:{}", review.fingerprint),
                        serde_json::to_value(review).expect("trust review serializes"),
                    ));
                }
            }
        }
        match prompt_report(&self.settings.folder, "system") {
            Ok(Some((text, warnings))) => {
                for warning in warnings {
                    self.notice_once(warning.clone(), &warning, sink);
                }
                project.push_str(&format!("\n\n## Project prompt guidance\n{text}"));
            }
            Err(error) => self.notice_once(error.clone(), &error, sink),
            Ok(None) => {}
        }
        match prompt_report(&self.settings.folder, "compaction_summary") {
            Ok(Some((text, warnings))) => {
                summary.push_str(&format!("\n\n## Project summary guidance\n{text}"));
                for warning in warnings {
                    self.notice_once(warning.clone(), &warning, sink);
                }
            }
            Err(error) => self.notice_once(error.clone(), &error, sink),
            Ok(None) => {}
        }
        for (path, error) in &discovery.errors {
            self.notice_once(
                format!("{}:{error}", path.display()),
                &format!("{}: {error}", path.display()),
                sink,
            );
        }
        for listing in discovery.effective() {
            for warning in listing.skill.warnings() {
                self.notice_once(
                    format!("{}:{warning}", listing.skill.path.display()),
                    warning,
                    sink,
                );
            }
        }
        let manifest = self
            .skills
            .manifest_from(&discovery, &self.known, self.prompt.as_deref());
        if !manifest.is_empty() {
            project.push_str(&format!("\n\n{manifest}"));
        }
        *self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ProjectContent { project, summary };
        pending
    }

    fn refresh(&self, system: &str, _: &dyn StreamSink) -> String {
        let content = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| context::identity::ProjectSlot::after(system))
            .render(&content.project)
    }

    fn pricing(&self, default: Option<kernel::Pricing>) -> Option<kernel::Pricing> {
        let lock = self
            .settings
            .lock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (lock.pricing.input_per_mtok, lock.pricing.output_per_mtok) {
            (Some(input), Some(output)) => Some(kernel::Pricing {
                input_per_mtok: input,
                output_per_mtok: output,
                cached_input_per_mtok: lock.pricing.cached_input_per_mtok,
                cache_write_per_mtok: lock.pricing.cache_write_per_mtok,
                indicative: false,
            }),
            _ => {
                let mut pricing = self
                    .fallback
                    .or(default.filter(|pricing| pricing.indicative))?;
                pricing.input_per_mtok = lock
                    .pricing
                    .input_per_mtok
                    .unwrap_or(pricing.input_per_mtok);
                pricing.output_per_mtok = lock
                    .pricing
                    .output_per_mtok
                    .unwrap_or(pricing.output_per_mtok);
                pricing.cached_input_per_mtok = lock
                    .pricing
                    .cached_input_per_mtok
                    .or(pricing.cached_input_per_mtok);
                pricing.cache_write_per_mtok = lock
                    .pricing
                    .cache_write_per_mtok
                    .or(pricing.cache_write_per_mtok);
                pricing.indicative = true;
                Some(pricing)
            }
        }
    }
}

fn has_disabled_content(folder: &Path, raw: &MedhaLock, has_skills: bool) -> bool {
    has_skills
        || ["system", "compaction_summary"].iter().any(|id| {
            std::fs::symlink_metadata(folder.join(".medha/prompts").join(format!("{id}.md")))
                .is_ok()
        })
        || !raw.risky_settings().is_empty()
        || (raw.lsp.enabled && lsp::would_start_in(folder, &lsp::Config::default()))
}

fn prompt_report(folder: &Path, id: &str) -> Result<Option<(String, Vec<String>)>, String> {
    if !crate::project_trust::content_allowed(folder) {
        return Ok(None);
    }
    let path = folder.join(".medha/prompts").join(format!("{id}.md"));
    if !path.exists() {
        return Ok(None);
    }
    let actual = path.canonicalize().map_err(|e| e.to_string())?;
    if actual != path || !actual.starts_with(folder) {
        return Err("project prompt crosses a symlink".into());
    }
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("project prompt is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 128 * 1024 {
        return Err("project prompt exceeds 128 KiB".into());
    }
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut findings = Vec::new();
    policy::guard::scan_instructions(id, &text, &mut findings);
    if findings
        .iter()
        .any(|finding| finding.severity == policy::guard::Severity::Dangerous)
    {
        return Err(format!(
            "dangerous project prompt blocked: {}",
            path.display()
        ));
    }
    let warnings = findings
        .iter()
        .map(|finding| format!("{}: {}", path.display(), finding.reason))
        .collect();
    Ok(Some((text, warnings)))
}

pub fn lsp_config(source: &Path, folder: &Path, base: &lsp::Config) -> Option<lsp::Config> {
    let trust = ProjectTrust::for_folder(source).ok().flatten()?;
    lsp_config_for_trust(&trust, source, folder, base)
}

pub(crate) fn lsp_config_for_trust(
    trust: &ProjectTrust,
    source: &Path,
    folder: &Path,
    base: &lsp::Config,
) -> Option<lsp::Config> {
    if !trust.allows_asset(source, folder) {
        return None;
    }
    let raw = MedhaLock::load(source.join("medha.lock"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let lock = trust.filter(source, &raw);
    let mut config = base.clone();
    config.enabled = lock.lsp.enabled;
    config.allow_network = lock.lsp.allow_network;
    for entry in lock.lsp.servers {
        if entry.trust != "workspace" || entry.id.is_empty() {
            continue;
        }
        let settings = if entry.settings.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::to_value(entry.settings).ok()?
        };
        if entry.command.is_empty() && entry.languages.is_empty() {
            if let Some(server) = config
                .servers
                .iter_mut()
                .find(|server| server.id == entry.id)
            {
                server.settings = settings;
            }
        } else if !entry.command.is_empty() && !entry.languages.is_empty() {
            config.servers.retain(|server| server.id != entry.id);
            config.servers.push(lsp::ServerAdapter {
                id: entry.id,
                command: entry.command,
                languages: lsp::language_mappings(&entry.languages),
                root_markers: entry.root_markers,
                settings,
                requires_approval: false,
            });
        }
    }
    Some(config)
}

pub struct LiveVerifier {
    settings: Arc<LiveSettings>,
    exec: Arc<dyn ExecBackend>,
    timeout: std::time::Duration,
}

pub(crate) struct Quiet;
impl crate::Notices for Quiet {
    fn say(&self, _: &str) {}
}
impl LiveVerifier {
    pub fn new(
        settings: Arc<LiveSettings>,
        exec: Arc<dyn ExecBackend>,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            settings,
            exec,
            timeout,
        }
    }
}
#[async_trait::async_trait]
impl kernel::Verifier for LiveVerifier {
    fn required(&self) -> bool {
        self.settings.options.require_verify
            || self
                .settings
                .lock
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .verify
                .required
    }
    async fn check(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<kernel::VerifyReport> {
        let command = self.settings.options.verify_command.clone().or_else(|| {
            self.settings
                .lock
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .verify
                .command
                .clone()
        });
        match command {
            Some(command) => crate::verify::CommandVerifier { command, required: self.required(), dir: self.settings.folder.clone(), limit: self.timeout, exec: self.exec.clone() }.check(cancel).await,
            None if self.required() => Some(kernel::VerifyReport { ok: false, summary: "Project verification command is not accepted; run /trust to accept its exact value.".into(), output: String::new() }),
            None => None,
        }
    }
}

#[cfg(test)]
#[path = "project_session_tests.rs"]
mod tests;
