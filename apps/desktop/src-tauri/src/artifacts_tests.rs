use super::*;

/// A saved page's assets are read as they are now, and only from the page's own
/// folder inside the workspace: no name in a manifest can reach anything else.
#[test]
fn a_saved_page_reads_only_its_own_folder_as_it_is_now() {
    let root = test_support::scratch("desktop-current-assets")
        .canonicalize()
        .unwrap();
    let site = root.join("site");
    std::fs::create_dir_all(site.join("css")).unwrap();
    std::fs::write(site.join("css/style.css"), "h1{color:blue}").unwrap();
    std::fs::write(root.join("secret.txt"), "never").unwrap();
    assert_eq!(
        current(&root, &site, "css/style.css").as_deref(),
        Some(&b"h1{color:blue}"[..])
    );
    assert!(current(&root, &site, "../secret.txt").is_none());
    assert!(
        current(&root, &site, "css").is_none(),
        "a folder is not an asset"
    );
    assert!(current(&root, &site, "gone.css").is_none());
    let outside = test_support::scratch("desktop-current-outside");
    std::fs::write(outside.join("private.css"), "never").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("private.css"), site.join("linked.css")).unwrap();
        assert!(current(&root, &site, "linked.css").is_none());
    }
    assert!(current(&root, &outside, "private.css").is_none());
}
