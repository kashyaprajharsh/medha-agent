//! A server whose certificate no public authority signed, over real TLS.
//! The keys in `test_tls/` exist only for these tests and guard nothing.

use crate::{AuthKind, ProviderClient, ProviderProfile};
use futures::StreamExt;
use kernel::{CompiledContext, Message, Provider, ProviderError};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ServerConfig, crypto::ring};

const AUTHORITY: &[u8] = include_bytes!("test_tls/ca.pem");
const SIGNED: (&[u8], &[u8]) = (
    include_bytes!("test_tls/leaf.pem"),
    include_bytes!("test_tls/leaf.key"),
);
const SELF_SIGNED: (&[u8], &[u8]) = (
    include_bytes!("test_tls/own.pem"),
    include_bytes!("test_tls/own.key"),
);
const ANSWER: &str =
    r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

/// An https server on this computer showing `certificate`; its address.
async fn serve((certificate, key): (&'static [u8], &'static [u8])) -> String {
    let shown = vec![CertificateDer::from_pem_slice(certificate).unwrap()];
    let key = PrivateKeyDer::from_pem_slice(key).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(shown, key)
        .unwrap();
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let tls = tls.clone();
            tokio::spawn(async move {
                // A client that does not trust the certificate ends it here.
                let Ok(mut open) = tls.accept(socket).await else {
                    return;
                };
                let mut request = [0u8; 16 * 1024];
                let _ = open.read(&mut request).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{ANSWER}",
                    ANSWER.len()
                );
                let _ = open.write_all(reply.as_bytes()).await;
                let _ = open.shutdown().await;
            });
        }
    });
    format!("https://127.0.0.1:{port}/v1")
}

async fn ask(profile: ProviderProfile) -> Result<(), ProviderError> {
    let client = ProviderClient::from_profile(profile, "").unwrap();
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::user("hello")],
        ordered: None,
        tools: Vec::new(),
    };
    let request = client.prepare_request(&context)?;
    let mut stream = client.stream_prepared(&request).await?;
    while stream.next().await.is_some() {}
    Ok(())
}

/// Saves `certificates` to a file and returns the folder that keeps it and its path.
fn saved(certificates: &[u8]) -> (tempfile::TempDir, String) {
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("trusted.pem");
    std::fs::write(&file, certificates).unwrap();
    let path = file.to_string_lossy().into_owned();
    (folder, path)
}

/// A server behind a private authority, as behind a company proxy, is refused
/// and named as a certificate fault, never retried as a server that is away.
/// With that authority's file saved on the model, the same server answers.
#[tokio::test]
async fn a_private_certificate_is_refused_until_its_file_is_saved_with_the_model() {
    let server = serve(SIGNED).await;
    let plain = ProviderProfile::openai_chat(&server, "m", AuthKind::None);
    let error = ask(plain.clone()).await.unwrap_err();
    assert!(matches!(error, ProviderError::Certificate(_)), "{error:?}");
    assert!(!error.is_retryable(), "it would fail the same way again");

    let (_kept, file) = saved(AUTHORITY);
    let trusting = ProviderProfile {
        ca_cert_file: Some(file),
        ..plain.clone()
    };
    ask(trusting).await.unwrap();

    // A file that is not there is said plainly, and nothing is sent without it.
    let missing = ProviderProfile {
        ca_cert_file: Some("/no/such/folder/trusted.pem".into()),
        ..plain
    };
    let error = ask(missing).await.unwrap_err();
    assert!(error.to_string().contains("could not be read"), "{error}");
}

/// A server that signed its own certificate, as a local one may: refused, and
/// then trusted once that very certificate is the file saved with the model.
#[tokio::test]
async fn a_self_signed_server_is_trusted_by_its_own_certificate() {
    let server = serve(SELF_SIGNED).await;
    let plain = ProviderProfile::openai_chat(&server, "m", AuthKind::None);
    let error = ask(plain.clone()).await.unwrap_err();
    assert!(matches!(error, ProviderError::Certificate(_)), "{error:?}");

    let (_kept, file) = saved(SELF_SIGNED.0);
    let trusting = ProviderProfile {
        ca_cert_file: Some(file),
        ..plain
    };
    ask(trusting).await.unwrap();
}
