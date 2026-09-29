use super::*;

fn scratch(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("medha-browser-{tag}-"))
        .tempdir()
        .unwrap()
}

/// Quit, then kill: a killed browser leaves its temp folders behind.
fn stop_browser(mut child: std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: signals only the browser this test started.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn get(server: &PageServer, path: &str, host: &str) -> String {
    let port = server.host.rsplit_once(':').unwrap().1;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// Checked on the opened handle, so no path or symlink swap can redirect the read.
#[test]
fn only_files_really_inside_the_workspace_are_opened() {
    let base = scratch("serve");
    let root = base.path().join("site");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("index.html"), "home").unwrap();
    std::fs::write(root.join("docs/a b.html"), "doc").unwrap();
    std::fs::write(base.path().join("secret.txt"), "outside").unwrap();
    let root = root.canonicalize().unwrap();

    for served in ["/index.html", "/", "/docs/a%20b.html"] {
        assert!(open_page(&root, served).is_some(), "{served} was refused");
    }
    let outside = base.path().join("secret.txt").canonicalize().unwrap();
    for denied in [
        "/../secret.txt".to_string(),
        "/%2e%2e/secret.txt".into(),
        "/..%2fsecret.txt".into(),
        format!("/{}", outside.display()),
        "/missing.html".into(),
    ] {
        assert!(open_page(&root, &denied).is_none(), "{denied} was served");
    }
    #[cfg(unix)]
    {
        // A folder replaced by a link out, as a racing command would do.
        std::fs::remove_dir_all(root.join("docs")).unwrap();
        std::os::unix::fs::symlink(base.path(), root.join("docs")).unwrap();
        assert!(
            open_page(&root, "/docs/secret.txt").is_none(),
            "a link out of the workspace must not be followed"
        );
        let fifo = root.join("pipe.html");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a plain mode.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (done, opened) = std::sync::mpsc::channel();
        let root = root.clone();
        std::thread::spawn(move || done.send(open_page(&root, "/pipe.html").is_none()));
        assert_eq!(
            opened.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a FIFO must be refused without blocking"
        );
    }
}

#[test]
fn only_the_token_host_is_served_and_nothing_leaks_the_token() {
    let base = scratch("live");
    std::fs::write(base.path().join("index.html"), "<h1>hi</h1>").unwrap();
    let server = PageServer::start(base.path()).unwrap();
    let page = get(&server, "/index.html", &server.host.to_uppercase());
    assert!(
        page.starts_with("HTTP/1.1 200") && page.ends_with("<h1>hi</h1>"),
        "{page}"
    );
    assert!(page.contains("Referrer-Policy: no-referrer"));
    let port = server.host.rsplit_once(':').unwrap().1;
    for other in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        "guess.localhost".into(),
    ] {
        assert!(
            get(&server, "/index.html", &other).starts_with("HTTP/1.1 404"),
            "{other}"
        );
    }
}

#[test]
fn without_a_grant_every_connection_but_the_page_server_is_dead() {
    let host = "tok.localhost:4321";
    let args = browser_args(
        Path::new("/p"),
        Path::new("/o.png"),
        "http://tok.localhost:4321/index.html",
        (800, 600),
        Some(host),
        false,
    );
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    assert!(has("--proxy-server=http://127.0.0.1:9"));
    assert!(has("--proxy-bypass-list=<-loopback>;tok.localhost:4321"));
    assert!(has(
        "--host-resolver-rules=MAP tok.localhost 127.0.0.1, MAP * ~NOTFOUND"
    ));
    assert!(has(
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp"
    ));
    for flag in [
        "--use-mock-keychain",
        "--password-store=basic",
        "--user-data-dir=/p",
    ] {
        assert!(has(flag), "missing {flag}");
    }
    assert!(
        !has("--no-sandbox"),
        "the browser keeps its own renderer sandbox"
    );
    assert_eq!(args.last().unwrap(), "http://tok.localhost:4321/index.html");

    let granted = browser_args(
        Path::new("/p"),
        Path::new("/o.png"),
        "https://x.dev",
        (800, 600),
        None,
        true,
    );
    assert!(!granted.iter().any(|arg| arg.starts_with("--proxy-server")));
    assert!(granted.iter().any(|arg| arg == "--use-mock-keychain"));
}

/// Real browser: the page renders offline and reaches no other address, loopback included.
#[test]
fn a_real_render_shows_the_page_and_reaches_nothing_else() {
    let Some(browser) = find_browser() else {
        return;
    };
    let base = scratch("render");
    let spy = TcpListener::bind("127.0.0.1:0").unwrap();
    spy.set_nonblocking(true).unwrap();
    let spy_addr = spy.local_addr().unwrap();
    std::fs::create_dir_all(base.path().join("assets")).unwrap();
    std::fs::write(
        base.path().join("assets/site.css"),
        "body{background:#00aa77}",
    )
    .unwrap();
    std::fs::write(
        base.path().join("index.html"),
        format!(
            "<link rel=stylesheet href=\"/assets/site.css\"><h1>medha</h1>\
             <img src=\"http://{spy_addr}/leak.png\"><img src=\"https://example.com/x.png\">"
        ),
    )
    .unwrap();
    let server = PageServer::start(base.path()).unwrap();
    let out = base.path().join("shot.png");
    let status = std::process::Command::new(&browser)
        .args(browser_args(
            &base.path().join("profile"),
            &out,
            &server.url(Path::new("index.html")),
            (640, 480),
            Some(&server.host),
            false,
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !out.exists() && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(500));
    stop_browser(status);
    assert!(
        spy.accept().is_err(),
        "a page rendered without network reached another local port"
    );
    let png = std::fs::read(&out).expect("the page must render");
    let shot = image::load_from_memory(&png).unwrap().to_rgb8();
    let corner = shot.get_pixel(4, shot.height() - 4).0;
    assert_eq!(
        corner,
        [0x00, 0xaa, 0x77],
        "a root-relative stylesheet must load"
    );
}

/// With network on, another localhost service a page reaches never sees the token.
#[test]
fn another_local_service_never_receives_the_page_credential() {
    let Some(browser) = find_browser() else {
        return;
    };
    let base = scratch("leak");
    let spy = TcpListener::bind("127.0.0.1:0").unwrap();
    spy.set_nonblocking(true).unwrap();
    let spy_addr = spy.local_addr().unwrap();
    std::fs::write(
        base.path().join("index.html"),
        format!(
            "<img src=\"http://{spy_addr}/a.png\"><img src=\"http://localhost:{}/b.png\">",
            spy_addr.port()
        ),
    )
    .unwrap();
    let server = PageServer::start(base.path()).unwrap();
    let token = server.host.split('.').next().unwrap().to_string();
    let out = base.path().join("shot.png");
    let child = std::process::Command::new(&browser)
        .args(browser_args(
            &base.path().join("profile"),
            &out,
            &server.url(Path::new("index.html")),
            (320, 240),
            Some(&server.host),
            true,
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut seen = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) && seen.len() < 2 {
        if let Ok((mut stream, _)) = spy.accept() {
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap_or(0);
            seen.push(String::from_utf8_lossy(&request[..read]).into_owned());
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    while !out.exists() && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_browser(child);
    assert!(!seen.is_empty(), "the page never reached the other service");
    for request in &seen {
        assert!(
            !request.to_lowercase().contains(&token),
            "token leaked: {request}"
        );
    }
}

#[test]
fn only_urls_need_network_and_other_schemes_are_refused() {
    assert!(needs_network(
        &json!({"path": "https://example.com", "render": true})
    ));
    assert!(needs_network(
        &json!({"path": "HTTP://localhost:3000", "render": true})
    ));
    assert!(
        !needs_network(&json!({"path": "https://example.com"})),
        "only a render opens it"
    );
    assert!(!needs_network(
        &json!({"path": "index.html", "render": true})
    ));
    assert!(!needs_network(&json!({})));

    let base = scratch("targets");
    std::fs::write(base.path().join("index.html"), "x").unwrap();
    std::fs::write(base.path().join("../outside-browser-target.html"), "x").ok();
    let state = scratch("state");
    let sbx = sandbox::WorkspaceSandbox::new(
        base.path(),
        state.path().join("trust.lock"),
        state.path().join("audit.log"),
        None,
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        assert_eq!(
            parse_target("index.html", &sbx).await.unwrap(),
            Target::Workspace(PathBuf::from("index.html"))
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "missing.html",
        ] {
            assert!(parse_target(bad, &sbx).await.is_err(), "{bad} was accepted");
        }
    });
}

#[derive(Default)]
struct MemArtifacts(std::sync::Mutex<Vec<Vec<u8>>>);

impl kernel::ArtifactStore for MemArtifacts {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let mut stored = self.0.lock().unwrap();
        stored.push(bytes.to_vec());
        Ok(format!("h{}", stored.len()))
    }
    fn get(&self, _: &str, _: usize, _: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _: &str) -> Result<usize, String> {
        Ok(0)
    }
}

/// `read` with `render: true` returns the picture; an ungranted URL never starts a browser.
#[tokio::test]
async fn read_render_returns_the_page_as_an_image_and_refuses_ungranted_urls() {
    let Some(browser) = find_browser() else {
        return;
    };
    let base = scratch("read");
    std::fs::write(
        base.path().join("index.html"),
        "<h1 style=color:red>medha</h1>",
    )
    .unwrap();
    let artifacts = Arc::new(MemArtifacts::default());
    let page = BrowserScreenshot {
        sbx: Arc::new(sandbox::WorkspaceSandbox::new_jailed(base.path()).unwrap()),
        artifacts: artifacts.clone(),
        browser,
    };
    let payload = page
        .render(&json!({"path": "index.html", "render": true}))
        .await
        .unwrap();
    let media: Vec<kernel::MediaPart> = serde_json::from_value(payload[MEDIA].clone()).unwrap();
    assert_eq!(media.len(), 1);
    assert!(media[0].mime_type.starts_with("image/"));
    assert_eq!(artifacts.0.lock().unwrap().len(), 1);

    if page.sbx.denies_network() {
        let refused = page
            .render(&json!({"path": "https://example.com", "render": true}))
            .await;
        assert!(refused.is_err(), "an ungranted URL must not be opened");
        assert_eq!(artifacts.0.lock().unwrap().len(), 1);
    }
}
