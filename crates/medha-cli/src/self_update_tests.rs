use super::*;
use std::ffi::OsStr;

#[test]
fn only_a_newer_release_is_offered() {
    assert!(newer("v0.1.13", "0.1.12"));
    assert!(newer("v0.2.0", "0.1.99"));
    assert!(!newer("v0.1.12", "0.1.12"));
    assert!(!newer("v0.1.11", "0.1.12"));
    // A prerelease or a tag that is not a version never replaces a release.
    for tag in ["v0.1.13-rc.1", "latest", "v0.1", "v0.1.13.1", ""] {
        assert!(!newer(tag, "0.1.12"), "{tag}");
    }
}

#[test]
fn only_a_copy_in_an_installers_version_folder_can_update_itself() {
    let root = Path::new("/home/me/.local/lib/medha");
    let install = Install::from_exe(&root.join("versions/v0.1.12/medha")).unwrap();
    assert_eq!(install.root, root);
    assert_eq!(
        install.executable("v0.1.13"),
        root.join("versions")
            .join("v0.1.13")
            .join(if cfg!(windows) { "medha.exe" } else { "medha" })
    );
    for elsewhere in [
        "/home/me/project/target/release/medha",
        "/home/me/.cargo/bin/medha",
        "/home/me/.local/lib/medha/versions/latest/medha",
    ] {
        assert_eq!(Install::from_exe(Path::new(elsewhere)), None, "{elsewhere}");
    }
}

#[test]
fn the_installer_is_pinned_to_the_offered_release_and_the_install_folder() {
    let env = |command: &std::process::Command, key: &str| {
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(key))
            .map(|(_, value)| value.map(OsStr::to_os_string))
    };
    let folder = PathBuf::from("/opt/me/bin");
    let install = Install {
        root: PathBuf::from("/opt/me/lib/medha"),
        install_dir: Some(folder.clone()),
    };
    let command = installer(Path::new("/tmp/x/install.sh"), "v0.1.13", &install);
    assert_eq!(env(&command, "MEDHA_VERSION"), Some(Some("v0.1.13".into())));
    assert_eq!(
        env(&command, "MEDHA_INSTALL_DIR"),
        Some(Some(folder.into()))
    );
    let program = PathBuf::from(command.get_program());
    if cfg!(windows) {
        assert!(program.is_absolute() && program.ends_with("powershell.exe"));
    } else {
        assert_eq!(program, Path::new("/bin/sh"));
    }
    // Without a known folder the installer chooses, and an inherited override is not passed on.
    let install = Install {
        install_dir: None,
        ..install
    };
    let command = installer(Path::new("/tmp/x/install.sh"), "v0.1.13", &install);
    assert_eq!(env(&command, "MEDHA_INSTALL_DIR"), Some(None));
}

#[test]
fn a_skipped_release_is_not_offered_again_but_the_next_one_is() {
    let cache = |latest: &str, skipped: Option<&str>| Cache {
        latest: latest.into(),
        checked_at: 0,
        skipped: skipped.map(str::to_owned),
    };
    assert_eq!(
        offered(&cache("v0.1.13", None), "0.1.12").as_deref(),
        Some("v0.1.13")
    );
    assert_eq!(offered(&cache("v0.1.13", Some("v0.1.13")), "0.1.12"), None);
    assert_eq!(
        offered(&cache("v0.1.14", Some("v0.1.13")), "0.1.12").as_deref(),
        Some("v0.1.14")
    );
    assert_eq!(offered(&cache("v0.1.12", None), "0.1.12"), None);
}

// A throwaway key, never used for a release, and `echo hello` signed with it as install.sh of 0.1.13.
const TEST_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDU1MzM3QUQ5NDU2NUIzMTUKUldRVnMyVkYyWG96VlV4aU40LzUyTDNvdTU2MlZlK3U1TzZhdWZPUDhjc3BmRmNmcFpvYUJSTjMK";
const TEST_SCRIPT: &[u8] = b"echo hello\n";
const TEST_SIGNATURE: &[u8] = b"dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRVnMyVkYyWG96VmNSWmxuUm84TXVIRnRTZTBJbzl1aVBIWGhYMFU1dXEyVTRXb1pEdTR1TE5reUxuaEJ1YWw4ZlZwRkV0OXlWMXlJTTg2Ky84dUh5ejNsRkdieHB4Z1E4PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzMTI5NDI4CWZpbGU6aW5zdGFsbC5zaAl2ZXJzaW9uOjAuMS4xMwptTHVKQkkrcFR2RFRuOGUrN0pXaGxaRkpLWEx1OEtKak5JTWZjN2lZT0drL1lTSVU2Q2U3d0VKOWRNYWY0YXk3UzVVOU84aU1xUzBsUWVBa3BSVitCdz09Cg==";

#[test]
fn only_the_exact_script_signed_for_this_file_and_release_may_run() {
    verify(
        TEST_SCRIPT,
        TEST_SIGNATURE,
        TEST_KEY,
        "install.sh",
        "v0.1.13",
    )
    .unwrap();
    // One changed byte, from either host, is refused.
    assert!(
        verify(
            b"echo hellp\n",
            TEST_SIGNATURE,
            TEST_KEY,
            "install.sh",
            "v0.1.13"
        )
        .is_err()
    );
    // A genuine script cannot pass for another file or an older or newer release.
    assert!(
        verify(
            TEST_SCRIPT,
            TEST_SIGNATURE,
            TEST_KEY,
            "install.ps1",
            "v0.1.13"
        )
        .is_err()
    );
    assert!(
        verify(
            TEST_SCRIPT,
            TEST_SIGNATURE,
            TEST_KEY,
            "install.sh",
            "v0.1.14"
        )
        .is_err()
    );
    // Signed by any other key, including the real one here, it does not run.
    assert!(
        verify(
            TEST_SCRIPT,
            TEST_SIGNATURE,
            UPDATE_KEY,
            "install.sh",
            "v0.1.13"
        )
        .is_err()
    );
    assert!(
        verify(
            TEST_SCRIPT,
            b"not a signature",
            TEST_KEY,
            "install.sh",
            "v0.1.13"
        )
        .is_err()
    );
}

#[test]
fn the_cli_checks_installers_with_the_key_built_into_the_desktop_app() {
    let config: serde_json::Value = serde_json::from_str(include_str!(
        "../../../apps/desktop/src-tauri/tauri.conf.json"
    ))
    .unwrap();
    assert_eq!(config["plugins"]["updater"]["pubkey"], UPDATE_KEY);
}

/// Serves `files` by path over plain HTTP until dropped; anything else is a 404.
async fn serve(
    files: Vec<(&'static str, &'static [u8])>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let files = files.clone();
            tokio::spawn(async move {
                let mut request = vec![0; 4096];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let reply = match files.iter().find(|(name, _)| *name == path) {
                    Some((_, body)) => [
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes(),
                        body.to_vec(),
                    ]
                    .concat(),
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&reply).await;
            });
        }
    });
    (address, task)
}

#[tokio::test]
async fn an_unreachable_or_incomplete_source_falls_through_to_the_next() {
    let (mirror, _mirror) = serve(vec![("/v0.1.13/install.sh", TEST_SCRIPT)]).await;
    let (github, _github) = serve(vec![
        ("/v0.1.13/install.sh", TEST_SCRIPT),
        ("/v0.1.13/install.sh.minisig", TEST_SIGNATURE),
    ])
    .await;
    let client = client(Duration::from_secs(5), Duration::from_secs(10)).unwrap();
    let sources = [
        // Nothing listens on port 1.
        "http://127.0.0.1:1/v0.1.13/install.sh".to_owned(),
        // The script without its signature is not enough.
        format!("http://{mirror}/v0.1.13/install.sh"),
        format!("http://{github}/v0.1.13/install.sh"),
    ];
    let (script, signature) = download_installer(&client, &sources).await.unwrap();
    assert_eq!(
        (script.as_slice(), signature.as_slice()),
        (TEST_SCRIPT, TEST_SIGNATURE)
    );
    let none = download_installer(&client, &sources[..2])
        .await
        .unwrap_err();
    assert!(
        format!("{none:#}").contains("could not be downloaded"),
        "{none:#}"
    );
}

#[tokio::test]
async fn an_address_that_never_answers_costs_only_its_share_of_the_connect_limit() {
    let (live, _live) = serve(vec![("/latest/latest.json", b"{\"version\":\"0.1.13\"}")]).await;
    // A non-routable private address: connecting to it hangs, as the dead GitHub address did.
    let dead: std::net::SocketAddr = format!("10.255.255.1:{}", live.port()).parse().unwrap();
    let client = client_builder(Duration::from_secs(4), Duration::from_secs(30))
        .resolve_to_addrs("mirror.test", &[dead, live])
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    let tag = latest_mirrored(&client, &format!("http://mirror.test:{}", live.port()))
        .await
        .unwrap();
    assert_eq!(tag, "v0.1.13");
    // Two addresses share 4 s, so the dead one costs at most 2 s, never the 30 s limit or the system's.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_fork_downloads_from_its_own_releases_and_never_from_the_mirror() {
    let mirrored = sources("v0.1.13", "install.sh");
    if std::env::var_os("MEDHA_REPO").is_none() {
        assert_eq!(
            mirrored,
            [
                "https://download.manojavamlabs.com/v0.1.13/install.sh",
                "https://github.com/kashyaprajharsh/medha-agent/releases/download/v0.1.13/install.sh",
            ]
        );
    } else {
        assert_eq!(mirrored.len(), 1, "a fork has no mirror: {mirrored:?}");
    }
}
