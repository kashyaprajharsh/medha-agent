//! Medha keeps folders of its own inside its state folder. The jail closes the
//! state to commands and opens only what the harness itself hands it; these
//! checks run against the default layout and against a named one.

use sandbox::{
    ApprovedRoots, ExecRequest, NetworkGrant, SandboxConfig, StateAccess, select_backend,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Backend = Arc<dyn sandbox::ExecBackend>;

fn backend(state: StateAccess, approved: ApprovedRoots) -> Backend {
    let config = SandboxConfig {
        state,
        ..SandboxConfig::default()
    };
    select_backend(&config, Vec::new(), approved, NetworkGrant::default())
}

/// What a jailed shell command printed, or nothing when the jail refused it.
async fn shell(backend: &Backend, cwd: &Path, script: &str) -> String {
    let request = ExecRequest {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        clear_env: false,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
    };
    match backend.run(request).await {
        Ok(out) if out.status == Some(0) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A fresh folder to act as HOME, already resolved the way the jail sees paths.
pub fn fresh(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("medha-state-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(root.join("home")).unwrap();
    root.canonicalize().unwrap()
}

pub async fn the_state_is_closed_except_what_medha_opens(state: &Path, plain: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let worktree = state.join("projects/p/worktrees/mine");
    let other = state.join("projects/p/worktrees/other");
    let (cache, servers) = (state.join("mcp-cache"), state.join("lsp"));
    let secrets = [
        state.join("credentials.toml"),
        state.join("serve/token"),
        state.join("projects/p/events.db"),
        other.join("file.txt"),
    ];
    for secret in &secrets {
        write(secret, "secret");
    }
    write(&worktree.join("file.txt"), "mine");
    write(&worktree.join(".git/hooks/keep"), "");
    write(&state.join("skills/pdf/SKILL.md"), "a skill");
    let server = servers.join("bin/zz-server");
    write(&server, "#!/bin/sh\necho server-ran\n");
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    for dir in [&cache, plain] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let reads = |paths: &[PathBuf]| {
        let all: Vec<String> = paths
            .iter()
            .map(|p| format!("cat {}", p.display()))
            .collect();
        all.join("; ")
    };

    // An ordinary chat, even one whose person approved reading the whole state.
    let approved = ApprovedRoots::default();
    approved.allow_read(state.to_path_buf());
    approved.allow_read(state.join("skills/pdf"));
    let chat = backend(StateAccess::default(), approved);
    assert_eq!(
        shell(&chat, plain, "echo fine > made.txt && cat made.txt").await,
        "fine"
    );
    assert_eq!(
        shell(&chat, plain, &reads(&secrets)).await,
        "",
        "a command read Medha's state"
    );
    let skill = state.join("skills/pdf/SKILL.md");
    assert_eq!(shell(&chat, plain, &reads(&[skill])).await, "a skill");
    for secret in &secrets[..2] {
        assert!(
            sandbox::is_protected(secret),
            "{} is open to file tools",
            secret.display()
        );
    }

    // A writer agent: its own worktree and nothing else in the state.
    let writer = chat
        .scoped_to(&worktree)
        .expect("a native backend can be scoped");
    let worked = shell(
        &writer,
        &worktree,
        "cat file.txt && echo x > made.txt && echo wrote",
    )
    .await;
    assert_eq!(
        worked, "minewrote",
        "a writer's command could not use its own worktree"
    );
    assert_eq!(shell(&writer, &worktree, &reads(&secrets)).await, "");
    #[cfg(target_os = "macos")]
    assert_eq!(
        shell(
            &writer,
            &worktree,
            "echo x > .git/hooks/pre-commit && echo planted"
        )
        .await,
        ""
    );

    // A local MCP server: its package cache.
    let mcp = backend(
        StateAccess {
            read: Vec::new(),
            write: vec![cache.clone()],
        },
        ApprovedRoots::default(),
    );
    let cached = format!("echo x > {}/pkg && echo cached", cache.display());
    assert_eq!(
        shell(&mcp, plain, &cached).await,
        "cached",
        "the MCP cache is not writable"
    );
    assert_eq!(shell(&mcp, plain, &reads(&secrets)).await, "");

    // A language server Medha installed: runnable, not writable.
    let lsp = backend(
        StateAccess {
            read: vec![servers.clone()],
            write: Vec::new(),
        },
        ApprovedRoots::default(),
    );
    assert_eq!(
        shell(&lsp, plain, &server.display().to_string()).await,
        "server-ran"
    );
    let planted = format!("echo x > {}/bin/planted && echo planted", servers.display());
    assert_eq!(shell(&lsp, plain, &planted).await, "");
    assert_eq!(shell(&lsp, plain, &reads(&secrets)).await, "");

    // The harness itself cannot open the whole state, or a folder holding a credential.
    let careless = backend(
        StateAccess {
            read: vec![state.to_path_buf()],
            write: vec![state.join("serve")],
        },
        ApprovedRoots::default(),
    );
    assert_eq!(shell(&careless, plain, &reads(&secrets[..2])).await, "");
}
