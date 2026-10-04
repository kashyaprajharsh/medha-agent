use super::*;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(dir.path().join("Screen Shot 1.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/logo.JPG"), b"\xff\xd8\xff").unwrap();
    dir
}

#[test]
fn a_dropped_path_with_spaces_attaches_however_the_terminal_quoted_it() {
    let dir = fixture();
    let root = dir.path();
    let quoted = format!(
        "'{}' explain this",
        root.join("Screen Shot 1.png").display()
    );
    assert_eq!(scan(&quoted, root), vec![root.join("Screen Shot 1.png")]);
    let escaped = format!(
        "{} explain this",
        root.join("Screen Shot 1.png")
            .display()
            .to_string()
            .replace(' ', "\\ ")
    );
    assert_eq!(scan(&escaped, root), vec![root.join("Screen Shot 1.png")]);
}

#[test]
fn relative_and_uppercase_extensions_resolve_under_the_workspace() {
    let dir = fixture();
    assert_eq!(
        scan("compare shot.png with assets/logo.JPG please", dir.path()),
        vec![
            dir.path().join("shot.png"),
            dir.path().join("assets/logo.JPG")
        ]
    );
}

#[test]
fn only_files_that_exist_become_attachments() {
    let dir = fixture();
    assert!(scan("look at missing.png and notes.txt", dir.path()).is_empty());
}

#[test]
fn trailing_punctuation_is_prose_not_part_of_the_name() {
    let dir = fixture();
    assert_eq!(
        scan("see shot.png.", dir.path()),
        vec![dir.path().join("shot.png")]
    );
    assert_eq!(
        scan("(shot.png)", dir.path()),
        vec![dir.path().join("shot.png")]
    );
}

#[test]
fn a_path_inside_backticks_stays_quoted_text() {
    let dir = fixture();
    assert!(scan("the file `shot.png` is missing", dir.path()).is_empty());
    assert!(scan("```\nshot.png\n```", dir.path()).is_empty());
    assert_eq!(
        scan("`unclosed shot.png", dir.path()),
        Vec::<std::path::PathBuf>::new()
    );
}

#[test]
fn the_same_path_twice_attaches_once() {
    let dir = fixture();
    assert_eq!(
        scan("shot.png and shot.png again", dir.path()),
        vec![dir.path().join("shot.png")]
    );
}

#[test]
fn expand_handles_quotes_home_and_absolute_paths() {
    let root = Path::new("/work/project");
    assert_eq!(
        expand(root, " \"assets/shot.png\" "),
        PathBuf::from("/work/project/assets/shot.png")
    );
    assert_eq!(
        expand(root, "/tmp/shot.png"),
        PathBuf::from("/tmp/shot.png")
    );
    if let Some(home) = dirs::home_dir() {
        assert_eq!(expand(root, "~/shot.png"), home.join("shot.png"));
    }
}

#[test]
fn unsupported_image_formats_are_still_recognised_as_attachments() {
    assert!(has_image_extension("photo.HEIC"));
    assert!(has_image_extension("frame.avif"));
    assert!(!has_image_extension("notes.txt"));
    assert!(!has_image_extension("archive.png.zip"));
}

/// The macOS screenshot case: the terminal writes a quoted path under a
/// temporary folder that is deleted moments later. Once attached, that path is
/// not information — it is a missing file for the model to chase.
#[test]
fn a_dropped_screenshot_leaves_no_path_behind() {
    let outside = std::env::temp_dir().join("NSIRD_screencaptureui/Screen Shot.png");
    let root = Path::new("/work/project");

    assert_eq!(
        strip_unreachable(
            &format!("'{}' what is on my screen", outside.display()),
            std::slice::from_ref(&outside),
            root
        ),
        "what is on my screen"
    );
    assert_eq!(
        strip_unreachable(
            &outside.display().to_string(),
            std::slice::from_ref(&outside),
            root
        ),
        ""
    );
    assert_eq!(
        strip_unreachable(
            &outside.display().to_string().replace(' ', "\\ "),
            &[outside],
            root
        ),
        ""
    );
}

/// A workspace file keeps its name: the model may need it to write back.
#[test]
fn a_workspace_path_survives_because_tools_can_act_on_it() {
    let dir = fixture();
    let inside = dir.path().join("shot.png");
    let line = format!("resize {} to 800px", inside.display());

    assert_eq!(strip_unreachable(&line, &[inside], dir.path()), line);
}
