use super::*;

/// A fetch that cannot connect says why, not only that the request could not be sent.
#[tokio::test]
async fn a_request_that_cannot_connect_names_its_cause() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let error = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect_err("nothing listens on a port just let go");
    let ToolError::Failed(said) = request_failed(error) else {
        panic!("a failed request is a failure");
    };
    assert!(said.starts_with("error sending request"), "{said}");
    assert!(
        said.to_ascii_lowercase().contains("refused"),
        "the cause was left out: {said}"
    );
}
