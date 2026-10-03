use crate::exec::{
    ExecRequest, NetPolicy, SandboxConfig, native_backend_available, select_backend,
};
use permissions::{ApprovedRoots, NetworkGrant};

/// Re-entered inside the jail: prints which sockets the kernel let it create.
#[test]
fn socket_child() {
    if std::env::var_os("MEDHA_SOCKET_CHILD").is_none() {
        return;
    }
    let opens = |family, kind| {
        // SAFETY: plain socket creation; a returned descriptor is closed at once.
        unsafe {
            let fd = libc::socket(family, kind, 0);
            if fd >= 0 {
                libc::close(fd);
            }
            fd >= 0
        }
    };
    let mut pair = [0; 2];
    // SAFETY: `pair` has room for the two descriptors.
    let paired =
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } == 0;
    println!(
        "unix={} tcp={} udp={} pair={paired}",
        opens(libc::AF_UNIX, libc::SOCK_STREAM),
        opens(libc::AF_INET, libc::SOCK_STREAM),
        opens(libc::AF_INET, libc::SOCK_DGRAM),
    );
}

async fn sockets_under(net: NetPolicy) -> String {
    let exe = std::env::current_exe().unwrap();
    let ws = std::env::temp_dir().join(format!("medha-sockets-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&ws).unwrap();
    let backend = select_backend(
        &SandboxConfig {
            net,
            ..SandboxConfig::default()
        },
        Vec::new(),
        ApprovedRoots::default(),
        NetworkGrant::default(),
    );
    assert_eq!(backend.label(), "native");
    let output = backend
        .run(ExecRequest {
            program: exe.to_string_lossy().into_owned(),
            args: [
                "--exact",
                "socket_filter::tests::socket_child",
                "--nocapture",
            ]
            .map(String::from)
            .to_vec(),
            cwd: ws.canonicalize().unwrap(),
            env: vec![("MEDHA_SOCKET_CHILD".into(), "1".into())],
            clear_env: false,
            read_roots: vec![exe.parent().unwrap().to_path_buf()],
            write_roots: Vec::new(),
        })
        .await
        .unwrap();
    std::fs::remove_dir_all(&ws).ok();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A Unix socket reaches container daemons and agents; UDP carries data out past a TCP-only rule.
#[tokio::test]
async fn a_jailed_command_opens_no_unix_socket_and_no_denied_network_socket() {
    if !native_backend_available() {
        return;
    }
    let denied = sockets_under(NetPolicy::Deny).await;
    assert!(
        denied.contains("unix=false tcp=false udp=false pair=true"),
        "{denied}"
    );
    let granted = sockets_under(NetPolicy::Allow).await;
    assert!(
        granted.contains("unix=false tcp=true udp=true pair=true"),
        "{granted}"
    );
}
