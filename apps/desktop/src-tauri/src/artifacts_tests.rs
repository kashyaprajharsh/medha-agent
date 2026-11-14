use super::*;

/// A page shows the files it loads as they are now, even ones written after it, and a root path means its own folder.
#[test]
fn a_page_shows_the_files_it_loads_even_when_they_were_written_after_it() {
    let root = dunce::canonicalize(test_support::scratch("desktop-assets-now")).unwrap();
    let site = root.join("site");
    std::fs::create_dir_all(&site).unwrap();
    let html = "<link rel=stylesheet href=css/style.css><script src=js/app.js></script>\
                <img src=/img/logo.png><img src=../secret.png><img src=gone.png>";
    std::fs::write(site.join("index.html"), html).unwrap();
    std::fs::create_dir_all(site.join("css")).unwrap();
    std::fs::create_dir_all(site.join("js")).unwrap();
    std::fs::create_dir_all(site.join("img")).unwrap();
    std::fs::write(
        site.join("css/style.css"),
        "body{background:url(../img/bg.png)}",
    )
    .unwrap();
    std::fs::write(site.join("img/bg.png"), b"bg").unwrap();
    std::fs::write(site.join("js/app.js"), "alert(1)").unwrap();
    std::fs::write(site.join("img/logo.png"), b"logo").unwrap();
    std::fs::write(root.join("secret.png"), b"never").unwrap();

    let (assets, missing) = assets_now(&root, &site.join("index.html"), html, Vec::new());
    let mut names: Vec<&str> = assets.iter().map(|asset| asset.path.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["css/style.css", "img/bg.png", "img/logo.png", "js/app.js"]
    );
    assert!(assets.iter().all(|asset| asset.data != b"never"));
    assert_eq!(missing, ["../secret.png", "gone.png"]);
}

/// A saved page's assets are read as they are now, and only from the page's own
/// folder inside the workspace: no name in a manifest can reach anything else.
#[test]
fn a_saved_page_reads_only_its_own_folder_as_it_is_now() {
    let root = dunce::canonicalize(test_support::scratch("desktop-current-assets")).unwrap();
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
