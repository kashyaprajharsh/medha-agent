use super::*;

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
fn sign_in_errors_survive_the_trip_between_processes() {
    let wire: WireError = Error::NeedsAuth("linear".into()).into();
    let back: Error = serde_json::from_value::<WireError>(serde_json::to_value(&wire).unwrap())
        .unwrap()
        .into();
    assert!(matches!(back, Error::NeedsAuth(server) if server == "linear"));
}
