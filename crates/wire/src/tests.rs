use super::*;
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf, duplex, split};

const ROLES: Roles = Roles {
    host: "host",
    guest: "chat",
};

type Side = (BufReader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>);

fn channel() -> (Side, Side) {
    let (a, b) = duplex(64 * 1024);
    let ((ar, aw), (br, bw)) = (split(a), split(b));
    ((BufReader::new(ar), aw), (BufReader::new(br), bw))
}

#[tokio::test]
async fn a_peer_cannot_make_the_host_buffer_an_endless_line() {
    let mut oversized = vec![b'x'; MAX_FRAME + 10];
    oversized.push(b'\n');
    let mut reader = BufReader::new(&oversized[..]);
    assert!(read_frame(&mut reader).await.is_none());

    let mut normal = BufReader::new(&b"{\"id\":1}\n"[..]);
    assert_eq!(read_frame(&mut normal).await, Some(json!({"id": 1})));
}

#[test]
fn only_the_exact_token_opens_the_channel() {
    assert!(same_secret("0f3a9c", "0f3a9c"));
    assert!(!same_secret("0f3a9c", "0f3a9d"));
    assert!(!same_secret("0f3a9c", "0f3a9"));
    assert!(!same_secret("", "0f3a9c"));
}

#[test]
fn the_proof_is_standard_hmac_and_bound_to_its_role() {
    // RFC 4231, test case 2.
    assert_eq!(
        mac("Jefe", "what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_ne!(proof("t0ken", "host", "n1"), proof("t0ken", "chat", "n1"));
    assert_ne!(proof("t0ken", "host", "n1"), proof("t0ken", "host", "n2"));
}

#[tokio::test]
async fn two_sides_holding_the_token_meet_and_it_never_crosses() {
    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let host = tokio::spawn(async move {
        let id = admit(&mut hr, &mut hw, "s3cret-token", ROLES).await?;
        write_frame(&mut hw, &json!({"id": id, "result": "welcome"})).await;
        Some(())
    });
    let reply = greet(&mut gr, &mut gw, "s3cret-token", ROLES).await;
    assert_eq!(reply.unwrap()["result"], "welcome");
    assert!(host.await.unwrap().is_some());

    let (tap, mut far) = duplex(64 * 1024);
    let (tr, mut tw) = split(tap);
    let mut tr = BufReader::new(tr);
    let guest = tokio::spawn(async move { greet(&mut tr, &mut tw, "s3cret-token", ROLES).await });
    let mut seen = [0u8; 512];
    let read = far.read(&mut seen).await.unwrap();
    assert!(!String::from_utf8_lossy(&seen[..read]).contains("s3cret-token"));
    drop(far);
    assert!(guest.await.unwrap().is_err());
}

#[tokio::test]
async fn a_guest_without_the_token_is_not_admitted() {
    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let host = tokio::spawn(async move { admit(&mut hr, &mut hw, "right", ROLES).await });
    assert_eq!(
        greet(&mut gr, &mut gw, "wrong", ROLES).await,
        Err(Refusal::Unproven)
    );
    drop((gr, gw));
    assert_eq!(host.await.unwrap(), None);

    // One that skips checking the host and guesses a proof gets no further.
    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let host = tokio::spawn(async move { admit(&mut hr, &mut hw, "right", ROLES).await });
    let hello = json!({"id": 0, "method": "hello", "params": {"nonce": nonce().unwrap()}});
    write_frame(&mut gw, &hello).await;
    let challenge = read_frame(&mut gr).await.unwrap();
    let theirs = challenge["result"]["nonce"].as_str().unwrap();
    let guess = json!({"id": 1, "method": "prove",
        "params": {"proof": proof("wrong", ROLES.guest, theirs)}});
    write_frame(&mut gw, &guess).await;
    assert_eq!(host.await.unwrap(), None);
}

#[tokio::test]
async fn a_proof_seen_on_one_connection_does_not_open_another() {
    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let host = tokio::spawn(async move { admit(&mut hr, &mut hw, "right", ROLES).await });
    let hello = json!({"id": 0, "method": "hello", "params": {"nonce": nonce().unwrap()}});
    write_frame(&mut gw, &hello).await;
    let first = read_frame(&mut gr).await.unwrap();
    let overheard = proof(
        "right",
        ROLES.guest,
        first["result"]["nonce"].as_str().unwrap(),
    );
    let prove = json!({"id": 1, "method": "prove", "params": {"proof": overheard}});
    write_frame(&mut gw, &prove).await;
    assert_eq!(host.await.unwrap(), Some(json!(1)));

    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let host = tokio::spawn(async move { admit(&mut hr, &mut hw, "right", ROLES).await });
    write_frame(&mut gw, &hello).await;
    read_frame(&mut gr).await.unwrap();
    write_frame(&mut gw, &prove).await;
    assert_eq!(host.await.unwrap(), None, "a replayed proof was accepted");
}

#[cfg(unix)]
fn scratch(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("wire-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

/// A socket may have to live under a folder others can write in, so nothing already there is trusted.
#[cfg(unix)]
#[tokio::test]
async fn listening_touches_nothing_that_is_not_this_users_own_folder_and_socket() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("planted");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    // A link planted where the socket's folder should be leads to someone's own folder.
    let theirs = root.join("theirs");
    std::fs::create_dir(&theirs).unwrap();
    std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(theirs.join("s.sock"), "their file").unwrap();
    std::os::unix::fs::symlink(&theirs, root.join("link")).unwrap();
    let through_link = root.join("link").join("s.sock");
    assert!(bind(through_link.to_str().unwrap()).is_err());
    assert_eq!(
        mode(&theirs),
        0o755,
        "a folder reached through a link was changed"
    );
    assert_eq!(
        std::fs::read_to_string(theirs.join("s.sock")).unwrap(),
        "their file"
    );

    // A file that is not a socket, where the socket should be, is left alone.
    let own = root.join("own");
    std::fs::create_dir(&own).unwrap();
    std::fs::write(own.join("s.sock"), "a file").unwrap();
    assert!(bind(own.join("s.sock").to_str().unwrap()).is_err());
    assert_eq!(
        std::fs::read_to_string(own.join("s.sock")).unwrap(),
        "a file"
    );

    // This user's own folder is made private, and its own stale socket gives way.
    let fresh = root.join("fresh").join("s.sock");
    drop(bind(fresh.to_str().unwrap()).unwrap());
    assert_eq!(mode(fresh.parent().unwrap()), 0o700);
    drop(bind(fresh.to_str().unwrap()).expect("a stale socket of its own was in the way"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_host_that_cannot_prove_itself_is_sent_no_proof() {
    let ((mut gr, mut gw), (mut hr, mut hw)) = channel();
    let guest = tokio::spawn(async move { greet(&mut gr, &mut gw, "right", ROLES).await });
    let hello = read_frame(&mut hr).await.unwrap();
    let forged = json!({"id": hello["id"],
        "result": {"proof": proof("wrong", ROLES.host, hello["params"]["nonce"].as_str().unwrap()),
                   "nonce": nonce().unwrap()}});
    write_frame(&mut hw, &forged).await;
    assert_eq!(guest.await.unwrap(), Err(Refusal::Unproven));
    assert_eq!(
        read_frame(&mut hr).await,
        None,
        "the guest answered a forged host"
    );
}
