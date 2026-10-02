use super::*;

fn shelf(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("medha-kept-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_page_is_kept_once_per_screen_and_handed_back_only_as_itself() {
    let dir = shelf("one");
    assert_eq!(kept(&dir, "draw", "ui://draw/view"), None);
    let page = json!({ "html": "<html>v1</html>", "connect": ["https://esm.sh"] });
    keep(&dir, "draw", "ui://draw/view", &page).unwrap();
    keep(
        &dir,
        "draw",
        "ui://draw/view",
        &json!({ "html": "<html>v2</html>" }),
    )
    .unwrap();
    assert_eq!(
        kept(&dir, "draw", "ui://draw/view").unwrap()["html"],
        "<html>v2</html>"
    );
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        1,
        "a newer page replaces the older"
    );
    assert_eq!(kept(&dir, "other", "ui://draw/view"), None);

    // A file under this screen's name that holds another's page is not believed.
    let forged = json!({ "server": "evil", "uri": "ui://draw/view", "page": { "html": "x" } });
    fs::write(file(&dir, "draw", "ui://draw/view"), forged.to_string()).unwrap();
    assert_eq!(kept(&dir, "draw", "ui://draw/view"), None);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_shelf_never_grows_past_its_size() {
    let dir = shelf("many");
    for index in 0..(MAX_PAGES + 9) {
        let uri = format!("ui://draw/{index}");
        keep(&dir, "draw", &uri, &json!({ "html": "<html></html>" })).unwrap();
    }
    assert!(fs::read_dir(&dir).unwrap().count() <= MAX_PAGES);
    let newest = format!("ui://draw/{}", MAX_PAGES + 8);
    assert!(
        kept(&dir, "draw", &newest).is_some(),
        "the newest page stays"
    );

    let huge = json!({ "html": "x".repeat(MAX_PAGE) });
    keep(&dir, "draw", "ui://draw/huge", &huge).unwrap();
    assert_eq!(
        kept(&dir, "draw", "ui://draw/huge"),
        None,
        "an oversized page is not kept"
    );
    fs::remove_dir_all(dir).unwrap();
}
