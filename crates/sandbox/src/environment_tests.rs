use super::*;

fn request() -> ExecRequest {
    ExecRequest {
        program: "true".into(),
        args: Vec::new(),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        clear_env: true,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
    }
}

#[test]
fn denied_network_never_inherits_transport_values_and_container_argv_contains_only_names() {
    let proxy = "https://operator:proxy-password-918275@proxy.example.invalid:443";
    let request = prepare_with(&request(), true, |name| {
        (name == "HTTPS_PROXY").then(|| proxy.into())
    })
    .unwrap();
    let argv = container_arguments(&request, true);
    assert_eq!(argv, ["--env", "HTTPS_PROXY"]);
    assert!(!argv.join(" ").contains(proxy));
    assert!(!redact(proxy).contains(proxy));
    assert!(!redact("proxy-password-918275").contains("proxy-password-918275"));
    let denied = prepare_with(&request, false, |_| {
        panic!("network-denied execution must not capture ambient transport")
    })
    .unwrap();
    assert!(!denied.env.iter().any(|(name, _)| is_transport(name)));
    assert!(container_arguments(&request, false).is_empty());
}

#[test]
fn an_explicit_transport_value_wins_and_ca_access_is_one_readonly_file() {
    let scratch = test_support::scratch("command-ca");
    let ca = scratch.join("cert.pem");
    std::fs::write(&ca, "certificate").unwrap();
    let mut request = request();
    request.env = vec![
        ("HTTP_PROXY".into(), "http://explicit.invalid".into()),
        ("SSL_CERT_FILE".into(), ca.to_string_lossy().into_owned()),
    ];
    let prepared = prepare_with(&request, true, |name| {
        (name == "HTTP_PROXY").then(|| "http://ambient.invalid".into())
    })
    .unwrap();
    assert!(
        prepared
            .env
            .contains(&("HTTP_PROXY".into(), "http://explicit.invalid".into()))
    );
    assert_eq!(prepared.read_roots, [ca.canonicalize().unwrap()]);
    assert!(prepared.write_roots.is_empty());
    assert!(
        container_arguments(&prepared, true)
            .iter()
            .any(|arg| arg.ends_with(",readonly"))
    );
}
