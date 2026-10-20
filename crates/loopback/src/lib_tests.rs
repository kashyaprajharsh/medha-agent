use super::*;
use tokio::io::AsyncReadExt;

async fn ask(port: u16, target: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut page = String::new();
    stream.read_to_string(&mut page).await.unwrap();
    page
}

#[test]
fn callback_query_is_decoded() {
    let pairs = query_pairs("/callback?code=a%2Fb&state=xyz&iss=https%3A%2F%2Fissuer");
    assert_eq!(pairs[0], ("code".into(), "a/b".into()));
    assert_eq!(pairs[1], ("state".into(), "xyz".into()));
    assert_eq!(pairs[2], ("iss".into(), "https://issuer".into()));
    assert!(query_pairs("/favicon.ico").is_empty());
}

#[tokio::test]
async fn the_listener_waits_past_other_requests_for_the_grant() {
    let listener = bind(None).await.unwrap();
    let port = listener.port();
    let caught = tokio::spawn(listener.redirect());
    assert!(ask(port, "/favicon.ico").await.contains("Waiting"));
    assert!(
        ask(port, "/auth/callback?code=c&state=s&client_id=oaiapp_1")
            .await
            .contains("Signed in")
    );
    let redirect = caught.await.unwrap().unwrap();
    assert_eq!(
        (redirect.get("code"), redirect.get("client_id")),
        (Some("c"), Some("oaiapp_1"))
    );
}

#[tokio::test]
async fn a_busy_preferred_port_falls_back_to_a_free_one() {
    let taken = bind(None).await.unwrap();
    let other = bind(Some(taken.port())).await.unwrap();
    assert_ne!(other.port(), taken.port());
}
