//! A chat's private HOME is its own; only the default scope is the process's.

use super::*;

async fn shell(scope: &HomeScope, cwd: &Path, script: &str) -> (Option<i32>, String) {
    let backend = select_backend(
        &SandboxConfig {
            home: scope.clone(),
            ..SandboxConfig::default()
        },
        Vec::new(),
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    let out = backend
        .run(ExecRequest {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: cwd.to_path_buf(),
            env: Vec::new(),
            clear_env: false,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
        })
        .await
        .unwrap();
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn a_private_scope_has_its_own_home_and_the_default_is_the_process_one() {
    let (a, b) = (HomeScope::private(), HomeScope::private());
    assert_ne!(a.home().path, b.home().path);
    assert_eq!(a.clone().home().path, a.home().path);
    assert_eq!(
        HomeScope::default().home().path,
        IsolatedHome::shared().path
    );
    assert_ne!(a.home().path, IsolatedHome::shared().path);
}

#[test]
fn a_private_home_lasts_until_its_last_holder_goes() {
    let scope = HomeScope::private();
    let path = scope.home().path.clone();
    let held = scope.clone();
    drop(scope);
    assert!(path.is_dir());
    drop(held);
    assert!(!path.exists());
}

#[tokio::test]
async fn commands_of_two_chats_do_not_share_a_home() {
    if !native_backend_available() {
        return;
    }
    let cwd = std::env::temp_dir().join(format!("medha-home-scope-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&cwd).unwrap();
    let (a, b) = (HomeScope::private(), HomeScope::private());

    let (status, home_a) = shell(
        &a,
        &cwd,
        r#"echo secret > "$HOME/note" && printf %s "$HOME""#,
    )
    .await;
    assert_eq!(status, Some(0));
    let (_, seen) = shell(
        &b,
        &cwd,
        r#"cat "$HOME/note" 2>/dev/null; printf %s "$HOME""#,
    )
    .await;
    assert!(
        !seen.contains("secret"),
        "the second chat read the first chat's HOME"
    );
    assert_ne!(seen, home_a);

    let (status, direct) = shell(&b, &cwd, &format!("cat {home_a}/note")).await;
    assert_ne!(
        status,
        Some(0),
        "the other chat's HOME was readable by its path"
    );
    assert!(!direct.contains("secret"));
    std::fs::remove_dir_all(&cwd).ok();
}
