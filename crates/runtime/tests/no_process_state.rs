//! One backend runs many folders and chats, so the runtime may not lean on the process.

use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push((
                path.display().to_string(),
                std::fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

#[test]
fn the_runtime_never_uses_the_process_directory_and_a_chat_never_reads_the_environment() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(files.len() > 10, "the runtime sources were found");

    let directory: Vec<_> = files
        .iter()
        .filter(|(_, text)| text.contains("current_dir("))
        .map(|(path, _)| path)
        .collect();
    assert!(
        directory.is_empty(),
        "process directory used in {directory:?}"
    );

    // The path that builds a chat; whoever starts the chat reads the environment for it.
    let chat_path = [
        "session.rs",
        "model.rs",
        "workspace.rs",
        "vision.rs",
        "approvals.rs",
    ];
    let environment: Vec<_> = files
        .iter()
        .filter(|(path, _)| chat_path.iter().any(|name| path.ends_with(name)))
        .filter(|(_, text)| text.contains("env::var"))
        .map(|(path, _)| path)
        .collect();
    assert!(
        environment.is_empty(),
        "environment read in {environment:?}"
    );
}
