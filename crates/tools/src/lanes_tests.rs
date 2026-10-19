use super::*;

/// Only a command whose every part is a known reader skips the lock. Anything
/// that writes, or that the shell could turn into something else, does not.
#[test]
fn only_plain_known_readers_count_as_reading() {
    for reads in [
        "ls -la src",
        "cat Cargo.toml | head -20",
        "rg TODO crates && wc -l README.md",
        "git status",
        "git diff --stat HEAD~1",
        "git log --oneline -5; git show HEAD",
        "sed -n 1,20p src/main.rs",
    ] {
        assert!(shell_only_reads(reads), "{reads}");
    }
    for writes in [
        "cargo test",
        "cat a > b",
        "cat a | tee b",
        "ls && rm -rf target",
        "sed -i s/a/b/ src/main.rs",
        "git commit -m x",
        "git -c core.pager=less status",
        "git diff --output=patch.diff",
        "cat $(make-something)",
        "cat `whoami`",
        "ls &",
        "(ls)",
        "/tmp/cat notes",
        "FOO=1 cat notes",
        "cat 'unclosed",
        "",
    ] {
        assert!(!shell_only_reads(writes), "{writes}");
    }
}

/// Two names for one file share a lane; a file not yet made has one too; a path
/// that cannot be placed makes its write run alone.
#[test]
fn a_write_is_placed_at_the_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("src"), root.join("alias")).unwrap();

    let main = path_key(&root.join("src/main.rs"));
    assert_eq!(file(&root, Some("src/main.rs")), main);
    assert_eq!(file(&root, Some("src/../src/main.rs")), main);
    #[cfg(unix)]
    assert_eq!(file(&root, Some("alias/main.rs")), main);
    assert_eq!(
        file(&root, Some("src/new/deep.rs")),
        path_key(&root.join("src/new/deep.rs"))
    );
    for path in ["src/new/../../x", "src/new/../main.rs", "src/main.rs/../x"] {
        assert_eq!(file(&root, Some(path)), WHOLE, "{path}");
    }
    assert_eq!(file(&root, root.join("src/main.rs").to_str()), main);
    assert_eq!(file(&root, None), WHOLE);
    assert_eq!(tree(&root), path_key(&root));
}
