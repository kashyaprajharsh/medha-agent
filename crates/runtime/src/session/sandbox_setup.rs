use super::*;

pub(super) struct Built {
    pub workspace: Arc<WorkspaceSandbox>,
    pub live_settings: Arc<crate::project_session::LiveSettings>,
    pub verifier_exec: Arc<dyn sandbox::ExecBackend>,
    pub sandbox_template: agents::SandboxTemplate,
    pub sbx_cfg: sandbox::SandboxConfig,
    pub scratch: Option<sandbox::Scratch>,
}

pub(super) fn build(setup: &Setup<'_>, surface: &Surface) -> Result<Built> {
    let Setup {
        lock,
        options,
        cwd,
        state,
        medha_home,
        notices,
    } = *setup;
    let logs_dir = state.join("logs");
    let lock_path = cwd.join("medha.lock");
    // Machine-local grants never inherit authority from repository config.
    let trust_path = state.join("trust.lock");
    if !lock.permissions.trusted_paths.is_empty() {
        notices.say(&format!(
            "warning: ignoring {} repository-provided permission grant(s) in {}; \
             out-of-workspace paths require explicit path-specific approval",
            lock.permissions.trusted_paths.len(),
            lock_path.display()
        ));
    }
    let audit_path = logs_dir.join("audit.log");
    let mut sbx_cfg = lock.sandbox.to_config();
    // One per chat: its commands, language servers, local MCP servers and hooks.
    sbx_cfg.home = sandbox::HomeScope::private();
    if let Some(backend) = options.sandbox {
        sbx_cfg.backend = backend;
    }
    if sbx_cfg.strict_cleanup && sbx_cfg.backend != sandbox::BackendKind::Container {
        anyhow::bail!(
            "[sandbox].strict_cleanup requires backend=container and a locally available image"
        );
    }
    // A selected container must never silently become weaker native isolation.
    match sbx_cfg.backend {
        sandbox::BackendKind::Container => {
            let runtime = sbx_cfg.runtime.clone().unwrap_or_else(|| "docker".into());
            if sbx_cfg.image.as_deref().unwrap_or("").is_empty() {
                anyhow::bail!(
                    "[sandbox] backend=container requires an image; command execution will not fall back to native"
                );
            } else if !sandbox::program_on_path(&runtime) {
                anyhow::bail!(
                    "container runtime '{runtime}' is not available; command execution will not fall back to native"
                );
            }
        }
        sandbox::BackendKind::Ssh => {
            if sbx_cfg.host.as_deref().unwrap_or("").is_empty() {
                notices.say(
                    "SSH needs a host. Chat is available; commands are unavailable until SSH is configured.",
                );
            } else if !sandbox::program_on_path("ssh") {
                notices.say("SSH is not installed. Chat is available; commands are unavailable until SSH is installed.");
            }
        }
        _ => {}
    }
    if sbx_cfg.backend == sandbox::BackendKind::Native && !sandbox::native_backend_available() {
        if cfg!(target_os = "linux") {
            notices.say("Linux native confinement is unavailable. Chat is available; commands fail closed until the native boundary can be set up.");
        } else if sandbox::native_sandbox_supported() {
            notices.say(
                "warning: the OS sandbox works on this machine but Medha's own profile failed \
                 to apply — this is a Medha bug, please report it. Shell commands run on the \
                 host; the scanner + approval gate still apply.",
            );
        } else {
            notices.say(
                "warning: OS-native sandbox unavailable here (needs macOS Seatbelt, or Linux \
                 Landlock on kernel ≥5.13) — shell commands run on the host; the scanner + \
                 approval gate still apply. Set [sandbox] backend = \"host\" to silence.",
            );
        }
    }
    // Approved roots update native profiles without exposing whole tool homes.
    let approved = sandbox::ApprovedRoots::default();
    // Shared network grant: the exec backend enforces it, the permission manager
    // flips and persists it. One handle so a session/persistent grant reaches both.
    let net_grant = sandbox::NetworkGrant::default();
    let scratch = sandbox::Scratch::create().ok();
    let live_settings = crate::project_session::LiveSettings::new(
        cwd.to_path_buf(),
        options.clone(),
        lock.clone(),
        sbx_cfg.home.clone(),
        approved.clone(),
        net_grant.clone(),
        scratch.as_ref().map(|scratch| scratch.path().to_path_buf()),
    );
    let exec_backend = live_settings.exec();
    let verifier_exec = Arc::clone(&exec_backend);

    // Writers re-root this template in their isolated worktrees.
    let sandbox_template = agents::SandboxTemplate {
        trust: trust_path.clone(),
        audit: audit_path.clone(),
        gate: surface.gate.clone(),
        exec: Arc::clone(&exec_backend),
        snapshots: state.join("snapshots"),
        readable: vec![config::user_skills_dir()?],
        approved: approved.clone(),
        net_grant: net_grant.clone(),
    };
    let mut workspace = WorkspaceSandbox::new_with_state_root(
        cwd,
        trust_path,
        audit_path,
        Some(surface.gate.clone()),
        approved,
        medha_home.to_path_buf(),
    )?
    .with_exec_backend(exec_backend)
    .with_network_grant(net_grant)?
    // Bundled user-skill files are trusted configuration, not workspace data.
    .with_readable_roots(&[config::user_skills_dir()?])
    .with_snapshots_dir(state.join("snapshots"))
    .with_command_snapshots_dir(config::command_snapshots_dir(cwd)?);
    // Held to the end of the run; a temp directory that refuses it just means no scratch.
    if let Some(scratch) = &scratch {
        workspace = workspace.with_scratch(scratch.path());
    }
    let workspace = Arc::new(workspace);
    Ok(Built {
        workspace,
        live_settings,
        verifier_exec,
        sandbox_template,
        sbx_cfg,
        scratch,
    })
}
