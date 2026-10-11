use super::*;
use kernel::write_lanes::{WHOLE, path_key};

struct NoArtifacts;

impl kernel::ArtifactStore for NoArtifacts {
    fn put(&self, _: &[u8]) -> Result<String, String> {
        Err("none".into())
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Err("none".into())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Err("none".into())
    }
}

/// What the kernel is told, by the names the model calls: a reader holds no one
/// up, a build holds only its own folder, and an edit only its own file.
#[test]
fn each_call_says_what_it_must_not_run_beside() {
    use BlastRadius::{IrreversibleLocal, Read, ReversibleLocal};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sbx = Arc::new(WorkspaceSandbox::new_jailed(&root).unwrap());
    let reg = ToolRegistry::with_workspace(sbx, Arc::new(NoArtifacts));
    let ask = |tool: &str, args: Value| {
        let intent = ToolIntent {
            id: "call".into(),
            tool: tool.into(),
            args,
        };
        (
            Executor::call_radius(&reg, &intent),
            Executor::mutation_key(&reg, &intent),
        )
    };
    let tree = Some(path_key(&root));
    let whole = Some(WHOLE.to_string());
    let file = |path: &str| Some(path_key(&root.join(path)));

    assert_eq!(
        ask("git", json!({ "subcommand": "status" })),
        (Some(Read), None)
    );
    assert_eq!(
        ask("git", json!({ "subcommand": "commit", "message": "m" })),
        (Some(ReversibleLocal), tree.clone())
    );
    assert_eq!(
        ask("git", json!({})),
        (Some(ReversibleLocal), tree.clone()),
        "a call that names no reader is not one"
    );
    let shell = |args: Value| ask("shell.exec", args);
    assert_eq!(
        shell(json!({ "command": "rg TODO src | head" })),
        (Some(IrreversibleLocal), None),
        "a reading command skips the lock and keeps its review"
    );
    assert_eq!(shell(json!({ "command": "cargo test" })).1, tree);
    assert_eq!(
        shell(json!({ "command": "ls", "outside_sandbox": true })).1,
        whole,
        "past the sandbox, even a reader runs alone"
    );
    assert_eq!(shell(json!({})).1, whole);

    assert_eq!(
        ask("edit", json!({ "path": "src/a.rs" })).1,
        file("src/a.rs")
    );
    assert_eq!(ask("write", json!({ "path": "b.rs" })).1, file("b.rs"));
    assert_eq!(ask("write", json!({})).1, whole);
    assert_eq!(ask("diagnostics", json!({})).1, tree);
    assert_eq!(ask("mcp__srv__list", json!({})).1, whole);
}
