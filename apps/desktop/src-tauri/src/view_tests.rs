use super::*;

fn header<'a>(reply: &'a Reply, name: &str) -> &'a str {
    reply
        .headers
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

fn path_of(url: &str) -> &str {
    url.strip_prefix(BASE).unwrap()
}

#[test]
fn a_screen_is_served_locked_down_and_scripts_wait_for_run() {
    let views = Views::default();
    let page = "<!doctype html><script>fetch('https://evil.example')</script>";
    for (reach, scripts) in [
        (Reach::Still, false),
        (Reach::Running, true),
        (Reach::Online, true),
    ] {
        let url = views.put(page.into(), reach.clone()).unwrap();
        let reply = views.respond(path_of(&url), None);
        let policy = header(&reply, "Content-Security-Policy");
        assert_eq!(reply.status, 200);
        assert_eq!(policy.contains("script-src"), scripts);
        assert_eq!(policy.contains("allow-scripts"), scripts);
        // Whatever its reach, a screen never shares an origin with the app, leaves its
        // frame, reaches a plain-http or local address, or is trusted as the app itself.
        assert!(policy.starts_with("sandbox"));
        assert!(policy.contains("default-src 'none'"));
        for wide in [
            "allow-same-origin",
            "allow-top-navigation",
            "allow-popups",
            "allow-forms",
            "http:",
            "*",
            "'self'",
            "medha-view",
            "127.0.0.1",
            "localhost",
        ] {
            assert!(!policy.contains(wide), "{wide} in {policy}");
        }
        // The web is reachable only for the one reach the person grants page by page.
        assert_eq!(policy.contains("https:"), reach == Reach::Online);
        assert_eq!(policy.contains("connect-src"), reach == Reach::Online);
        assert_eq!(header(&reply, "X-Content-Type-Options"), "nosniff");
        views.drop_screen(&url);
        assert_eq!(views.respond(path_of(&url), None).status, 404);
    }
}

#[test]
fn a_server_screen_reaches_only_the_origins_declared_for_it() {
    let text = |list: &[&str]| list.iter().map(|item| item.to_string()).collect();
    let declared = Origins {
        connect: text(&[
            "https://api.draw.example",
            "wss://live.draw.example:8443",
            "https://ok.example; script-src *",
            "http://plain.example",
            "https://*",
            "*",
            "https://127.0.0.1",
            "'self'",
            "medha-view://localhost",
        ]),
        resources: text(&[
            "https://cdn.draw.example",
            "https://*.draw.example",
            "data:",
        ]),
        frames: Vec::new(),
    };
    let policy = policy(&Reach::App(declared));
    let directive = |name: &str| {
        let found = policy.split("; ").find(|part| part.starts_with(name));
        found
            .unwrap_or_else(|| panic!("{name} in {policy}"))
            .to_string()
    };
    assert_eq!(
        directive("connect-src"),
        "connect-src https://api.draw.example wss://live.draw.example:8443"
    );
    assert_eq!(
        directive("script-src"),
        "script-src 'unsafe-inline' https://cdn.draw.example https://*.draw.example"
    );
    assert_eq!(directive("frame-src"), "frame-src 'none'");
    assert!(policy.starts_with("sandbox allow-scripts; default-src 'none'"));
    for wide in [
        "allow-same-origin",
        "http:",
        "'self'",
        "medha-view",
        "127.0.0.1",
        " *",
        "https://*;",
    ] {
        assert!(!policy.contains(wide), "{wide} in {policy}");
    }
    // A screen that declares nothing reaches nothing.
    assert!(super::policy(&Reach::App(Origins::default())).contains("connect-src 'none'"));
}

#[test]
fn only_a_linked_media_file_is_served_and_only_by_its_own_link() {
    let folder = std::env::temp_dir().join(format!("medha-view-{}", std::process::id()));
    fs::create_dir_all(&folder).unwrap();
    let clip = folder.join("clip.mp4");
    fs::write(&clip, b"0123456789").unwrap();
    let secret = folder.join("secret.txt");
    fs::write(&secret, b"key").unwrap();
    let views = Views::default();

    assert!(views.link(secret).is_err());
    let url = views.link(clip.clone()).unwrap();
    assert_eq!(views.link(clip).unwrap(), url);
    let path = path_of(&url);

    let whole = views.respond(path, None);
    assert_eq!(
        (whole.status, whole.body.as_slice()),
        (200, b"0123456789".as_slice())
    );
    let part = views.respond(path, Some("bytes=2-5"));
    assert_eq!(
        (part.status, part.body.as_slice()),
        (206, b"2345".as_slice())
    );
    assert_eq!(header(&part, "Content-Range"), "bytes 2-5/10");
    assert_eq!(views.respond(path, Some("bytes=-3")).body, b"789");
    assert_eq!(views.respond(path, Some("bytes=8-")).body, b"89");
    assert_eq!(views.respond(path, Some("bytes=4-99")).body, b"456789");
    assert_eq!(views.respond(path, Some("bytes=10-")).status, 416);
    assert_eq!(views.respond(path, Some("bytes=5-2")).status, 416);

    for guess in [
        "/file/",
        "/file/0",
        "/file/../secret.txt",
        "/secret.txt",
        "/screen/x",
        "/",
    ] {
        assert_eq!(views.respond(guess, None).status, 404, "{guess}");
    }
    fs::remove_dir_all(folder).unwrap();
}

#[test]
fn stored_screens_stay_bounded() {
    let views = Views::default();
    assert!(views.put("x".repeat(MAX_SCREEN + 1), Reach::Still).is_err());
    let first = views.put("a".repeat(MAX_SCREEN), Reach::Still).unwrap();
    for page in 0..(MAX_SCREENS / MAX_SCREEN) {
        let fill = char::from(b'b' + page as u8).to_string();
        views.put(fill.repeat(MAX_SCREEN), Reach::Still).unwrap();
    }
    assert_eq!(views.respond(path_of(&first), None).status, 404);
    let held: usize = views
        .screens
        .lock()
        .unwrap()
        .iter()
        .map(|screen| screen.html.len())
        .sum();
    assert!(held <= MAX_SCREENS);
}

#[test]
fn a_page_shown_in_several_frames_is_kept_once_and_goes_with_the_last() {
    let views = Views::default();
    let page = "<p>drawing</p>".to_string();
    let chat = views.put(page.clone(), Reach::Running).unwrap();
    let beside = views.put(page.clone(), Reach::Running).unwrap();
    assert_eq!(chat, beside);
    assert_eq!(views.screens.lock().unwrap().len(), 1);

    // The same page under another policy is not the same thing to serve.
    let still = views.put(page.clone(), Reach::Still).unwrap();
    assert_ne!(still, chat);
    assert!(
        !header(
            &views.respond(path_of(&still), None),
            "Content-Security-Policy"
        )
        .contains("allow-scripts")
    );

    views.drop_screen(&chat);
    assert_eq!(
        views.respond(path_of(&beside), None).status,
        200,
        "one frame still shows it"
    );
    views.drop_screen(&beside);
    assert_eq!(views.respond(path_of(&beside), None).status, 404);
    // A link that is already gone takes nothing else with it.
    views.drop_screen(&beside);
    assert_eq!(views.respond(path_of(&still), None).status, 200);
}
