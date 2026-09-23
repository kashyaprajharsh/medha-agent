use crate::state::Grant;
use crate::store::{Activation, Discovery, Scope, Store};
use crate::{HookDecision, HookEnvelope, HookFailureMode, HookPoint, HookResult};
use async_trait::async_trait;
use medha_extension_api::{ExtensionComponent, HookProtocol, HookWorkdir, ProcessEntrypoint};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;
const MAX_HOOK_STDOUT_BYTES: usize = 64 * 1024;
const MAX_HOOK_STDERR_BYTES: usize = 16 * 1024;
const MAX_HOOKS_PER_POINT: usize = 16;
const MAX_HOOK_BATCH_DURATION: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
struct HookRegistration {
    plugin_id: String,
    component_id: String,
    scope: Scope,
    root: PathBuf,
    content_hash: String,
    points: Vec<HookPoint>,
    entrypoint: ProcessEntrypoint,
    failure: HookFailureMode,
    timeout: std::time::Duration,
    grant: Grant,
    matcher: Vec<String>,
    protocol: HookProtocol,
    workdir: HookWorkdir,
    source: crate::PackageSource,
}

impl HookRegistration {
    /// A tool hook whose matcher excludes this call is skipped without a spawn.
    fn wants(&self, request: &kernel::HookRequest) -> bool {
        if !self.points.contains(&request.point) {
            return false;
        }
        if self.matcher.is_empty() || !request.point.is_tool_point() {
            return true;
        }
        let tool = request
            .payload
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        self.matcher
            .iter()
            .any(|pattern| tool_matches(crate::compat::canonical_tool_name(pattern), tool))
    }
}

/// `*` matches any run of characters; everything else is literal.
fn tool_matches(pattern: &str, tool: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = tool.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

/// Snapshot of the enabled hook catalogue for one Medha runtime. Activation is
/// rechecked before every spawn so disable and package drift take effect without
/// handing a changed executable the old approval.
pub struct ProcessHookRunner {
    store: Store,
    workspace: PathBuf,
    workspace_id: String,
    /// Swapped wholesale by `reload`; an invocation works on a snapshot.
    hooks: std::sync::RwLock<Arc<Vec<HookRegistration>>>,
    offline: Arc<dyn sandbox::ExecBackend>,
    online: Arc<dyn sandbox::ExecBackend>,
    require_isolation: bool,
    diagnostics: Vec<String>,
}

impl ProcessHookRunner {
    /// Never fails: an unreadable package, state file, or over-limit hook is a
    /// diagnostic, and the session starts with whatever is safely usable.
    pub fn load(store: Store, workspace: &Path) -> Self {
        match store.discover() {
            Ok(discovery) => {
                let mut runner = Self::new(store, &discovery, workspace);
                runner.diagnostics.splice(0..0, discovery.notices());
                runner
            }
            Err(error) => {
                let mut runner = Self::new(store, &Discovery::default(), workspace);
                runner.diagnostics.push(error.to_string());
                runner
            }
        }
    }

    /// Builds the runner from a discovery the caller already reported.
    pub fn new(store: Store, discovery: &Discovery, workspace: &Path) -> Self {
        let (hooks, diagnostics) = registrations(discovery);
        let backend = |net| {
            sandbox::select_backend(
                &sandbox::SandboxConfig {
                    net,
                    ..sandbox::SandboxConfig::default()
                },
                Vec::new(),
                sandbox::ApprovedRoots::default(),
                sandbox::NetworkGrant::default(),
            )
        };
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        Self {
            store,
            workspace_id: workspace_identity(&workspace),
            workspace,
            hooks: std::sync::RwLock::new(Arc::new(hooks)),
            offline: backend(sandbox::NetPolicy::Deny),
            online: backend(sandbox::NetPolicy::Allow),
            require_isolation: true,
            diagnostics,
        }
    }

    pub fn has_hooks(&self) -> bool {
        !self.snapshot().is_empty()
    }

    fn snapshot(&self) -> Arc<Vec<HookRegistration>> {
        self.hooks
            .read()
            .map(|hooks| Arc::clone(&hooks))
            .unwrap_or_default()
    }
}

/// Every enabled hook, capped per point; over-limit hooks become diagnostics.
fn registrations(discovery: &Discovery) -> (Vec<HookRegistration>, Vec<String>) {
    {
        let mut diagnostics = Vec::new();
        let mut hooks = Vec::new();
        for plugin in discovery.enabled() {
            let grant = plugin.grant.clone().unwrap_or_default();
            for component in &plugin.package.manifest.components {
                let ExtensionComponent::Hook {
                    id,
                    points,
                    entrypoint,
                    failure,
                    timeout_ms,
                    matcher,
                    protocol,
                    workdir,
                } = component
                else {
                    continue;
                };
                hooks.push(HookRegistration {
                    plugin_id: plugin.package.manifest.id.clone(),
                    component_id: id.clone(),
                    scope: plugin.scope,
                    root: plugin.package.root.clone(),
                    content_hash: plugin.package.content_hash.clone(),
                    points: points.clone(),
                    entrypoint: entrypoint.clone(),
                    failure: *failure,
                    timeout: std::time::Duration::from_millis(*timeout_ms),
                    grant: grant.clone(),
                    matcher: matcher.clone(),
                    protocol: *protocol,
                    workdir: *workdir,
                    source: plugin.package.source.clone(),
                });
            }
        }
        hooks.sort_by(|left, right| {
            left.plugin_id
                .cmp(&right.plugin_id)
                .then_with(|| left.component_id.cmp(&right.component_id))
        });
        let mut point_counts = HashMap::new();
        hooks.retain(|hook| {
            let within = hook.points.iter().all(|point| {
                point_counts.get(point).copied().unwrap_or(0usize) < MAX_HOOKS_PER_POINT
            });
            if within {
                for point in &hook.points {
                    *point_counts.entry(*point).or_insert(0) += 1;
                }
            } else {
                diagnostics.push(format!(
                    "hook {}/{} was not loaded: more than {MAX_HOOKS_PER_POINT} hooks \
                     target one hook point",
                    hook.plugin_id, hook.component_id
                ));
            }
            within
        });
        (hooks, diagnostics)
    }
}

impl ProcessHookRunner {
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_test_backend(mut self, backend: Arc<dyn sandbox::ExecBackend>) -> Self {
        self.offline = Arc::clone(&backend);
        self.online = backend;
        self.require_isolation = false;
        self
    }

    /// Rehashes only this hook's package, off the async runtime.
    async fn registration_ready(&self, hook: &HookRegistration) -> Result<bool, String> {
        if !hook.root.exists() {
            return Ok(false);
        }
        let store = self.store.clone();
        let (root, scope) = (hook.root.clone(), hook.scope);
        let (id, source) = (hook.plugin_id.clone(), hook.source.clone());
        let plugin = tokio::task::spawn_blocking(move || store.recheck(&root, &id, &source, scope))
            .await
            .map_err(|error| format!("package check did not finish: {error}"))?
            .map_err(|error| error.to_string())?;
        match plugin.activation {
            Activation::Enabled
                if plugin.package.content_hash == hook.content_hash
                    && plugin.package.manifest.id == hook.plugin_id
                    && plugin.grant.as_ref() == Some(&hook.grant) =>
            {
                Ok(true)
            }
            Activation::Disabled => Ok(false),
            Activation::Enabled | Activation::Changed => {
                Err("package content or grant changed after activation".into())
            }
            Activation::Collision | Activation::Shadowed => {
                Err("plugin id now collides with another package".into())
            }
        }
    }

    async fn execute(
        &self,
        hook: &HookRegistration,
        request: &kernel::HookRequest,
        limit: std::time::Duration,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<HookResult, HookRunFailure> {
        let exec_request = self.exec_request(hook)?;
        let backend = if hook.grant.network {
            &self.online
        } else {
            &self.offline
        };
        if self.require_isolation {
            let jailed = match backend.containment() {
                kernel::Containment::OsFsJailNoNet => true,
                kernel::Containment::OsFsJail => hook.grant.network,
                kernel::Containment::None => false,
            };
            if !jailed || (!hook.grant.network && !backend.denies_network(&exec_request)) {
                return Err(HookRunFailure::failed(
                    "the sandbox needed to enforce this hook's grant is unavailable on this host",
                ));
            }
        }
        let input = self.encode(hook, request, limit)?;
        let command = backend
            .build_command(&exec_request)
            .map_err(|error| HookRunFailure::failed(error.to_string()))?;
        let output = sandbox::run_command_bounded_with_input(
            command,
            input,
            limit,
            MAX_HOOK_STDOUT_BYTES,
            MAX_HOOK_STDERR_BYTES,
            Some(cancel),
        )
        .await
        .map_err(|error| HookRunFailure::failed(error.to_string()))?;
        if output.cancelled {
            return Err(HookRunFailure {
                status: kernel::HookStatus::Cancelled,
                reason: "hook process was cancelled".into(),
            });
        }
        if output.timed_out {
            return Err(HookRunFailure {
                status: kernel::HookStatus::TimedOut,
                reason: "hook process exceeded its deadline".into(),
            });
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err(HookRunFailure::failed(
                "hook process exceeded its output limit",
            ));
        }
        let result = match hook.protocol {
            HookProtocol::ExitStatus => crate::compat::decode_output(
                request.point,
                output.status,
                &output.stdout,
                &output.stderr,
            )
            .map_err(HookRunFailure::failed)?,
            HookProtocol::Envelope => {
                if !output.passed() {
                    return Err(HookRunFailure::failed(format!(
                        "hook process exited with status {}",
                        output
                            .status
                            .map_or_else(|| "signal".into(), |status| status.to_string())
                    )));
                }
                serde_json::from_slice(&output.stdout).map_err(|error| {
                    HookRunFailure::failed(format!("invalid hook response: {error}"))
                })?
            }
        };
        result
            .validate_for(request.point)
            .map_err(|error| HookRunFailure::failed(error.to_string()))?;
        Ok(result)
    }

    /// The hook runs in its package, or in the workspace when it was granted
    /// read access to the workspace root; either way only granted paths open.
    fn exec_request(
        &self,
        hook: &HookRegistration,
    ) -> Result<sandbox::ExecRequest, HookRunFailure> {
        let mut read_roots = vec![hook.root.clone()];
        read_roots.extend(hook.grant.read_roots(&self.workspace));
        let write_roots = hook.grant.write_roots(&self.workspace);
        let cwd = match hook.workdir {
            HookWorkdir::Package => hook.root.clone(),
            HookWorkdir::Workspace => {
                if !read_roots
                    .iter()
                    .chain(&write_roots)
                    .any(|root| root == &self.workspace)
                {
                    return Err(HookRunFailure::failed(
                        "workdir = \"workspace\" needs read_paths or write_paths to include \".\"",
                    ));
                }
                self.workspace.clone()
            }
        };
        let data = self.store.data_dir(&hook.plugin_id);
        std::fs::create_dir_all(&data).map_err(|error| {
            HookRunFailure::failed(format!("creating plugin data folder: {error}"))
        })?;
        let mut write_roots = write_roots;
        write_roots.push(data.clone());
        let mut env = hook_environment();
        let path = |path: &Path| path.to_string_lossy().into_owned();
        env.push(("MEDHA_PROJECT_DIR".into(), path(&self.workspace)));
        env.push(("MEDHA_PLUGIN_ROOT".into(), path(&hook.root)));
        env.push(("MEDHA_PLUGIN_DATA".into(), path(&data)));
        if hook.protocol == HookProtocol::ExitStatus {
            env.extend(crate::compat::environment(
                &self.workspace,
                &hook.root,
                &data,
            ));
        }
        let (program, args) = if hook.entrypoint.shell {
            let (shell, flag) = if cfg!(windows) {
                ("cmd", "/C")
            } else {
                ("/bin/sh", "-c")
            };
            (
                shell.to_string(),
                vec![flag.to_string(), hook.entrypoint.program.clone()],
            )
        } else {
            (
                hook.root
                    .join(&hook.entrypoint.program)
                    .to_string_lossy()
                    .into_owned(),
                hook.entrypoint.args.clone(),
            )
        };
        Ok(sandbox::ExecRequest {
            program,
            args,
            cwd,
            env,
            clear_env: true,
            read_roots,
            write_roots,
        })
    }

    fn encode(
        &self,
        hook: &HookRegistration,
        request: &kernel::HookRequest,
        limit: std::time::Duration,
    ) -> Result<Vec<u8>, HookRunFailure> {
        let encoded = match hook.protocol {
            HookProtocol::ExitStatus => serde_json::to_vec(&crate::compat::encode_input(
                request.point,
                &request.session_id,
                &self.workspace,
                &request.payload,
            )),
            HookProtocol::Envelope => {
                let deadline_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .saturating_add(limit.as_millis())
                    .min(u128::from(u64::MAX)) as u64;
                serde_json::to_vec(&HookEnvelope {
                    api_version: medha_extension_api::HOST_API_VERSION,
                    event_id: request.event_id.clone(),
                    causation_id: request.causation_id.clone(),
                    depth: request.depth,
                    plugin_id: hook.plugin_id.clone(),
                    component_id: hook.component_id.clone(),
                    point: request.point,
                    workspace_id: self.workspace_id.clone(),
                    session_id: Some(request.session_id.clone()),
                    deadline_unix_ms,
                    trust: request.trust.as_str().into(),
                    payload: request.payload.clone(),
                })
            }
        };
        let mut input = encoded
            .map_err(|error| HookRunFailure::failed(format!("encoding hook input: {error}")))?;
        input.push(b'\n');
        if input.len() > MAX_HOOK_INPUT_BYTES {
            return Err(HookRunFailure::failed("hook input exceeds 64 KiB"));
        }
        Ok(input)
    }
}

#[async_trait]
impl kernel::HookRunner for ProcessHookRunner {
    /// Hooks never enter model context, so newly enabled ones can apply at once
    /// without invalidating the prompt cache.
    fn reload(&self) -> Vec<String> {
        let discovery = match self.store.discover() {
            Ok(discovery) => discovery,
            Err(error) => return vec![error.to_string()],
        };
        let (hooks, mut diagnostics) = registrations(&discovery);
        if let Ok(mut current) = self.hooks.write() {
            *current = Arc::new(hooks);
        }
        diagnostics.splice(0..0, discovery.notices());
        diagnostics
    }

    async fn invoke(
        &self,
        request: &kernel::HookRequest,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> kernel::HookBatch {
        let batch_started = tokio::time::Instant::now();
        let batch_deadline = batch_started + MAX_HOOK_BATCH_DURATION;
        let mut batch = kernel::HookBatch::default();
        let hooks = self.snapshot();
        for hook in hooks.iter().filter(|hook| hook.wants(request)) {
            match self.registration_ready(hook).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(reason) => {
                    apply_hook_failure(
                        &mut batch,
                        hook,
                        request,
                        reason,
                        kernel::HookStatus::Failed,
                        0,
                    );
                    if matches!(batch.directive, kernel::HookDirective::Deny(_)) {
                        break;
                    }
                    continue;
                }
            }
            let now = tokio::time::Instant::now();
            let remaining = batch_deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                apply_hook_failure(
                    &mut batch,
                    hook,
                    request,
                    "hook batch exceeded its 30 second deadline".into(),
                    kernel::HookStatus::TimedOut,
                    batch_started
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                );
                break;
            }
            let started = std::time::Instant::now();
            match self
                .execute(hook, request, hook.timeout.min(remaining), cancel)
                .await
            {
                Ok(result) => {
                    let duration_ms =
                        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    let reason = result_reason(&result);
                    batch.audits.push(kernel::HookAudit {
                        event_id: request.event_id.clone(),
                        plugin_id: hook.plugin_id.clone(),
                        component_id: hook.component_id.clone(),
                        point: request.point,
                        status: kernel::HookStatus::Completed,
                        decision: Some(result.decision),
                        reason: reason.clone(),
                        duration_ms,
                    });
                    let point = request.point.as_str();
                    match result.decision {
                        HookDecision::Deny => {
                            batch.directive = kernel::HookDirective::Deny(
                                reason.unwrap_or_else(|| format!("denied by a {point} hook")),
                            );
                            break;
                        }
                        HookDecision::RequestApproval => {
                            if matches!(batch.directive, kernel::HookDirective::Continue) {
                                batch.directive =
                                    kernel::HookDirective::RequestApproval(reason.unwrap_or_else(
                                        || format!("approval requested by a {point} hook"),
                                    ));
                            }
                        }
                        HookDecision::AddContext => {
                            if let Some(text) = result.context {
                                batch.contexts.push(kernel::HookContext {
                                    plugin_id: hook.plugin_id.clone(),
                                    component_id: hook.component_id.clone(),
                                    text,
                                });
                            }
                        }
                        HookDecision::Annotate => {
                            if let Some(text) = &result.annotation {
                                batch.notices.push(format!(
                                    "{}/{}: {text}",
                                    hook.plugin_id, hook.component_id
                                ));
                            }
                        }
                        // `validate_for` rejects actions until a work engine runs them.
                        HookDecision::Continue | HookDecision::EnqueueAction => {}
                    }
                }
                Err(error) => {
                    let duration_ms =
                        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    apply_hook_failure(
                        &mut batch,
                        hook,
                        request,
                        error.reason,
                        error.status,
                        duration_ms,
                    );
                    if matches!(batch.directive, kernel::HookDirective::Deny(_)) {
                        break;
                    }
                }
            }
        }
        batch
    }
}

struct HookRunFailure {
    status: kernel::HookStatus,
    reason: String,
}

impl HookRunFailure {
    fn failed(reason: impl Into<String>) -> Self {
        Self {
            status: kernel::HookStatus::Failed,
            reason: reason.into(),
        }
    }
}

fn apply_hook_failure(
    batch: &mut kernel::HookBatch,
    hook: &HookRegistration,
    request: &kernel::HookRequest,
    reason: String,
    status: kernel::HookStatus,
    duration_ms: u64,
) {
    batch.audits.push(kernel::HookAudit {
        event_id: request.event_id.clone(),
        plugin_id: hook.plugin_id.clone(),
        component_id: hook.component_id.clone(),
        point: request.point,
        status,
        decision: None,
        reason: Some(reason.clone()),
        duration_ms,
    });
    match hook.failure {
        HookFailureMode::FailClosed => {
            batch.directive = kernel::HookDirective::Deny(format!(
                "required hook {}/{} failed: {reason}",
                hook.plugin_id, hook.component_id
            ));
        }
        HookFailureMode::Warn => batch.notices.push(format!(
            "{}/{}: hook failed and was skipped: {reason}",
            hook.plugin_id, hook.component_id
        )),
        HookFailureMode::Ignore => {}
    }
}

fn result_reason(result: &HookResult) -> Option<String> {
    result
        .reason
        .clone()
        .or_else(|| result.annotation.clone())
        .or_else(|| result.context.clone())
        .or_else(|| result.action.clone())
}

fn workspace_identity(workspace: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(workspace.to_string_lossy().as_bytes());
    format!("sha256:{:x}", hash.finalize())
}

fn hook_environment() -> Vec<(String, String)> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "SystemRoot",
        "PATHEXT",
        "USERPROFILE",
        "USERNAME",
        "COMSPEC",
    ];
    ALLOWED
        .iter()
        .filter_map(|name| {
            std::env::var_os(name)
                .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

#[cfg(all(test, unix))]
#[path = "hooks_tests.rs"]
mod tests;
