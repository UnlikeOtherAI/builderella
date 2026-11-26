//! The transport rule: pinned TLS for `https`, nothing but plain HTTP for
//! `http`, and a different certificate is reported as such.

use crate::support::{Mock, Reply, Server, TOKEN, fast_options, fixtures};
use axum::http::Method;
use bella_client::blocking::BlockingClient;
use bella_client::{Client, ClientError, WorkerSocket};
use bella_core::api::SubmitJobRequest;
use bella_net::{ClientCredentials, ControllerUrl, Fingerprint};

fn other_fingerprint() -> Fingerprint {
    Fingerprint::of_certificate(b"some other certificate")
}

#[tokio::test]
async fn a_changed_certificate_is_a_pin_mismatch_and_nothing_is_sent() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::https(mock.router()).await;
    mock.on(Method::POST, "/api/v1/jobs", Reply::json(&fx.jobs[0]));
    mock.on(Method::GET, "/api/v1/whoami", Reply::json(&fx.whoami));

    let wrong = other_fingerprint();
    let client = Client::with_options(
        &server.url,
        Some(&wrong),
        Some(TOKEN.to_owned()),
        fast_options(),
    )
    .unwrap();

    // A request with a body: neither the body nor the token may leave.
    let job = &fx.jobs[0];
    let submit = SubmitJobRequest {
        project: job.project_name.clone(),
        snapshot_id: job.snapshot_id.clone(),
        spec: job.spec.clone().unwrap(),
        origin_platform: serde_json::from_value(serde_json::json!({
            "os": "linux", "arch": "x86_64"
        }))
        .unwrap(),
        session: None,
    };
    let error = client.submit_job(&submit, "key-1").await.unwrap_err();
    assert!(error.is_pin_mismatch(), "{error:?}");
    let ClientError::Tls {
        pin_mismatch: true,
        message,
    } = &error
    else {
        panic!("expected a pin mismatch, got {error:?}");
    };
    assert!(message.contains("fingerprint changed"), "{message}");
    // Both fingerprints are named, so the user can compare them.
    assert!(message.contains(&wrong.to_hex()), "{message}");
    assert!(
        message.contains(&server.fingerprint.unwrap().to_hex()),
        "{message}"
    );
    // A pin mismatch is not a transient failure and is not retried.
    assert!(!error.is_transient());

    let error = client.whoami().await.unwrap_err();
    assert!(error.is_pin_mismatch());

    // The server never saw an HTTP request, let alone a body.
    assert_eq!(mock.count(), 0);

    // The same server with the right pin works, so the refusals above were
    // about the certificate.
    assert_eq!(server.client().whoami().await.unwrap(), fx.whoami);
    assert_eq!(mock.count(), 1);
}

#[tokio::test]
async fn https_without_a_fingerprint_is_refused_everywhere() {
    let url = ControllerUrl::parse("https://127.0.0.1:7420", false).unwrap();

    let error = Client::new(&url, None, Some(TOKEN.to_owned())).unwrap_err();
    assert!(
        matches!(
            &error,
            ClientError::Tls {
                pin_mismatch: false,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("fingerprint"));

    assert!(matches!(
        BlockingClient::new(&url, None, Some(TOKEN.to_owned())).unwrap_err(),
        ClientError::Tls { .. }
    ));
    assert!(matches!(
        WorkerSocket::connect(&url, None, TOKEN).await.unwrap_err(),
        ClientError::Tls {
            pin_mismatch: false,
            ..
        }
    ));
}

#[tokio::test]
async fn a_fingerprint_on_plain_http_is_refused() {
    let url = ControllerUrl::parse("http://127.0.0.1:7420", false).unwrap();
    let fingerprint = other_fingerprint();
    assert!(matches!(
        Client::new(&url, Some(&fingerprint), Some(TOKEN.to_owned())).unwrap_err(),
        ClientError::Invalid { .. }
    ));
    assert!(matches!(
        WorkerSocket::connect(&url, Some(&fingerprint), TOKEN)
            .await
            .unwrap_err(),
        ClientError::Invalid { .. }
    ));
}

#[tokio::test]
async fn a_plain_http_client_does_not_speak_to_a_tls_server() {
    // The server only speaks TLS; a client configured for plain HTTP on the
    // same address gets no answer it accepts.
    let mock = Mock::new();
    let server = Server::https(mock.router()).await;
    let url = ControllerUrl::parse(&format!("http://{}", server.addr), false).unwrap();
    let mut options = fast_options();
    options.retry = bella_client::RetryPolicy::disabled();
    let client = Client::with_options(&url, None, Some(TOKEN.to_owned()), options).unwrap();
    let error = client.whoami().await.unwrap_err();
    assert!(
        matches!(
            error,
            ClientError::Transport { .. } | ClientError::Http { .. } | ClientError::Timeout
        ),
        "{error:?}"
    );
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn stored_credentials_build_a_pinned_client() {
    let fx = fixtures();
    let mock = Mock::new();
    let server = Server::https(mock.router()).await;
    mock.on(Method::GET, "/api/v1/whoami", Reply::json(&fx.whoami));

    let (token, _hash) = bella_net::Token::generate(bella_net::TokenKind::Developer).unwrap();
    let credentials =
        ClientCredentials::new(server.url.clone(), server.fingerprint, token.clone()).unwrap();
    let client = Client::from_credentials(&credentials).unwrap();
    assert_eq!(client.url(), &server.url);
    assert_eq!(client.fingerprint(), server.fingerprint.as_ref());
    assert!(client.has_token());
    assert_eq!(client.whoami().await.unwrap(), fx.whoami);
    assert_eq!(
        mock.single().header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
}
