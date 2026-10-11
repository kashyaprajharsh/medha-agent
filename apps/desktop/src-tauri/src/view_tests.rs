use super::*;

fn header<'a>(reply: &'a Reply, name: &str) -> &'a str {
    reply
        .headers
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

#[cfg(target_os = "macos")]
#[test]
fn webkit_loads_manifest_assets_and_enforces_preview_permissions() {
    use base64::Engine;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let views = Views::default();
    let parts = [
        ("styles/main.css", "text/css", b"@import './theme.css'; #box{width:321px;border:9px solid red}".as_slice()),
        ("styles/theme.css", "text/css", b"#box{background:rgb(1,2,3)}".as_slice()),
        ("images/logo.svg", "image/svg+xml", b"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect width='16' height='16' fill='red'/></svg>".as_slice()),
        ("scripts/main.js", "text/javascript", b"import {value} from './helper.js'; window.moduleRan=value;".as_slice()),
        ("scripts/helper.js", "text/javascript", b"export const value=32;".as_slice()),
    ];
    let bundle = views
        .bundle(
            parts
                .iter()
                .map(|(path, mime, bytes)| Asset {
                    path: (*path).into(),
                    mime: (*mime).into(),
                    data: bytes.to_vec(),
                })
                .collect(),
        )
        .unwrap();
    let html = "<!doctype html><html><head><link rel='stylesheet' href='./styles/main.css'></head><body><div id='box'>Local CSS</div><img id='logo' src='./images/logo.svg'><img src='https://blocked.invalid/banner'><script>window.authorRan=true;fetch('https://blocked.invalid/probe').catch(()=>{})</script><script type='module' src='./scripts/main.js'></script></body></html>";
    let mut pages = Vec::new();
    let mut replies = serde_json::Map::new();
    for reach in [Reach::Still, Reach::Running] {
        let url = views.put_bundle(html.into(), reach, Some(&bundle)).unwrap();
        let root = path_of(&url).strip_suffix("index.html").unwrap();
        for path in std::iter::once(path_of(&url).to_string())
            .chain(parts.iter().map(|(path, _, _)| format!("{root}{path}")))
        {
            let reply = views.respond(&path, None);
            let headers: std::collections::HashMap<_, _> = reply.headers.into_iter().collect();
            replies.insert(path, serde_json::json!({"status":reply.status,"headers":headers,"body":base64::engine::general_purpose::STANDARD.encode(reply.body)}));
        }
        pages.push(url);
    }
    let folder = std::env::temp_dir().join(format!("medha-webkit-assets-{}", token().unwrap()));
    fs::create_dir_all(&folder).unwrap();
    // Guard owns only this test's newly created directory, including Swift's
    // cache. Never clean a shared compiler cache or a user's running app.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _scratch = Scratch(folder.clone());
    let fixture = folder.join("fixture.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({"pages":pages,"replies":replies})).unwrap(),
    )
    .unwrap();
    let output = folder.join("result.log");
    let file = fs::File::create(&output).unwrap();
    let mut child = Command::new("swift")
        .arg("-module-cache-path")
        .arg(folder.join("swift-cache"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/view-webkit.swift"))
        .arg(fixture)
        .stdin(Stdio::null())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .expect("Swift is required for the macOS WebKit integration check");
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "WebKit asset check timed out: {}",
                fs::read_to_string(&output).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "WebKit asset check failed: {}",
        fs::read_to_string(output).unwrap()
    );
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
        assert!(policy.contains("script-src"));
        assert_eq!(policy.contains("script-src 'unsafe-inline'"), scripts);
        if !scripts {
            assert!(
                policy.contains("script-src 'nonce-"),
                "only the reporting script runs"
            );
        }
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
    for page in 0..(MAX_SCREENS / MAX_SCREEN - 2) {
        let fill = char::from(b'b' + page as u8).to_string();
        views.put(fill.repeat(MAX_SCREEN), Reach::Still).unwrap();
    }
    assert!(views.put("x".repeat(MAX_SCREEN), Reach::Still).is_err());
    assert_eq!(
        views.respond(path_of(&first), None).status,
        200,
        "overload never evicts an active preview"
    );
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
        .contains("script-src 'unsafe-inline'")
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

#[test]
fn local_assets_are_exact_manifest_members_and_survive_bundle_release() {
    let views = Views::default();
    let bundle = views
        .bundle(vec![
            Asset {
                path: "assets/site.css".into(),
                mime: "text/css".into(),
                data: b"body{color:red}".to_vec(),
            },
            Asset {
                path: "assets/with space.svg".into(),
                mime: "image/svg+xml".into(),
                data: b"<svg/>".to_vec(),
            },
        ])
        .unwrap();
    let page = views
        .put_bundle(
            "<html><head><link rel=stylesheet href=assets/site.css></head></html>".into(),
            Reach::Still,
            Some(&bundle),
        )
        .unwrap();
    let root = path_of(&page).strip_suffix("index.html").unwrap();
    views.drop_bundle(&bundle);
    assert!(views.bundles.lock().unwrap().is_empty());
    let css = views.respond(&format!("{root}assets/site.css"), None);
    assert_eq!(css.body, b"body{color:red}");
    assert_eq!(header(&css, "Content-Type"), "text/css");
    assert_eq!(header(&css, "X-Content-Type-Options"), "nosniff");
    assert_eq!(
        views
            .respond(&format!("{root}assets/with%20space.svg"), None)
            .status,
        200
    );
    for path in [
        "assets/other.css",
        "../private",
        "assets%2fsite.css",
        "assets/..%2fsite.css",
        "assets/%2e%2e/site.css",
        "assets/site.css%00",
    ] {
        assert_eq!(
            views.respond(&format!("{root}{path}"), None).status,
            404,
            "{path}"
        );
    }
    let reply = views.respond(path_of(&page), None);
    let policy = header(&reply, "Content-Security-Policy");
    assert!(policy.contains(&format!("style-src {BASE}{root}")));
    assert!(policy.contains("script-src 'nonce-"));
    assert!(!policy.contains("script-src 'unsafe-inline'"));
    assert!(!policy.contains("https:"));
    let html = String::from_utf8(reply.body).unwrap();
    assert!(html.contains("securitypolicyviolation"));
    assert!(html.contains("medha-preview-blocked"));
    assert!(html.contains("<link rel=stylesheet href=assets/site.css>"));
    views.drop_screen(&page);
    assert_eq!(
        views
            .respond(&format!("{root}assets/site.css"), None)
            .status,
        404
    );
}

#[test]
fn asset_manifests_refuse_unsafe_names_types_duplicates_and_expired_handles() {
    let views = Views::default();
    for path in [
        "../private",
        "/absolute",
        "assets/../private",
        "encoded%2fpath",
        "https://remote/file.css",
        "file.css?query",
    ] {
        assert!(
            views
                .bundle(vec![Asset {
                    path: path.into(),
                    mime: "text/css".into(),
                    data: vec![]
                }])
                .is_err()
        );
    }
    assert!(
        views
            .bundle(vec![Asset {
                path: "other.html".into(),
                mime: "text/html".into(),
                data: vec![]
            }])
            .is_err()
    );
    assert!(
        views
            .bundle(
                (0..2)
                    .map(|_| Asset {
                        path: "style.css".into(),
                        mime: "text/css".into(),
                        data: vec![]
                    })
                    .collect()
            )
            .is_err()
    );
    let bundle = views.bundle(vec![]).unwrap();
    views.drop_bundle(&bundle);
    assert!(
        views
            .put_bundle("page".into(), Reach::Still, Some(&bundle))
            .is_err()
    );
}

/// A reloaded window cannot drop what its last page held; without letting go,
/// previews held by a page that is gone would refuse every new one until restart.
#[test]
fn a_reloaded_window_starts_with_room_for_previews() {
    let views = Views::default();
    let held: Vec<_> = (0..32)
        .map(|index| views.bundle(vec![]).map(|_| index).unwrap())
        .collect();
    let pages: Vec<_> = (0..64)
        .map(|index| views.put(format!("page {index}"), Reach::Running).unwrap())
        .collect();
    assert_eq!(held.len(), 32);
    assert!(views.bundle(vec![]).is_err(), "the bundle limit is reached");
    assert!(views.put("one more".into(), Reach::Running).is_err());

    views.forget_all();

    for page in &pages {
        assert_eq!(views.respond(path_of(page), None).status, 404);
    }
    let bundle = views.bundle(vec![]).unwrap();
    assert!(
        views
            .put_bundle("page".into(), Reach::Still, Some(&bundle))
            .is_ok()
    );
    assert!(views.put("one more".into(), Reach::Running).is_ok());
}

#[test]
fn screen_count_is_bounded_without_discarding_live_pages() {
    let views = Views::default();
    let first = views.put("first".into(), Reach::Running).unwrap();
    for index in 1..64 {
        views.put(index.to_string(), Reach::Running).unwrap();
    }
    assert!(views.put("extra".into(), Reach::Running).is_err());
    assert_eq!(views.respond(path_of(&first), None).status, 200);
    views.drop_screen(&first);
    assert!(views.put("extra".into(), Reach::Running).is_ok());
}
