//! Real TLS handshakes over loopback between a server presenting a fleet
//! identity and clients pinned to a fingerprint.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bella_core::{ManualClock, UnixMillis};
use bella_net::{
    ALPN_H2, ALPN_HTTP11, Fingerprint, KEY_FILE, PinnedCertVerifier, ServerIdentity, pin_mismatch,
    pinned_client_config, server_config,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ProtocolVersion, ServerConfig};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Subject names shared by every identity in these tests. Neither
/// `127.0.0.1` nor `localhost` is among them.
const NAMES: [&str; 2] = ["fleet.builderella.test", "10.1.2.3"];

const REQUEST: &[u8] = b"ping from client";
const REPLY: &[u8] = b"pong from server";

/// Upper bound for any single network step, so a broken handshake fails the
/// test instead of hanging it.
const STEP: Duration = Duration::from_secs(20);

fn identity() -> (TempDir, ServerIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = NAMES.iter().map(|name| (*name).to_owned()).collect();
    let identity = ServerIdentity::load_or_generate(dir.path(), &names).unwrap();
    (dir, identity)
}

/// What the server saw for one accepted TCP connection.
type ServerOutcome = io::Result<Vec<u8>>;

/// Serves `connections` TCP connections: completes the TLS handshake, reads
/// the request, answers, and closes. Returns each connection's outcome.
async fn serve(
    config: Arc<ServerConfig>,
    connections: usize,
) -> (SocketAddr, JoinHandle<Vec<ServerOutcome>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(config);
    let task = tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for _ in 0..connections {
            let (tcp, _) = listener.accept().await.unwrap();
            let outcome = tokio::time::timeout(STEP, async {
                let mut tls = acceptor.accept(tcp).await?;
                let mut request = vec![0u8; REQUEST.len()];
                tls.read_exact(&mut request).await?;
                tls.write_all(REPLY).await?;
                tls.shutdown().await?;
                Ok(request)
            })
            .await
            .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "server step")));
            outcomes.push(outcome);
        }
        outcomes
    });
    (address, task)
}

#[derive(Debug)]
struct Exchange {
    reply: Vec<u8>,
    version: ProtocolVersion,
    alpn: Option<Vec<u8>>,
}

/// Connects, sends the request and reads the reply.
async fn exchange(
    address: SocketAddr,
    config: Arc<ClientConfig>,
    server_name: &str,
) -> io::Result<Exchange> {
    let server_name = ServerName::try_from(server_name.to_owned()).unwrap();
    tokio::time::timeout(STEP, async {
        let tcp = TcpStream::connect(address).await?;
        let mut tls = TlsConnector::from(config).connect(server_name, tcp).await?;
        tls.write_all(REQUEST).await?;
        tls.flush().await?;
        let mut reply = Vec::new();
        tls.read_to_end(&mut reply).await?;
        let (_, connection) = tls.get_ref();
        Ok(Exchange {
            reply,
            version: connection.protocol_version().unwrap(),
            alpn: connection.alpn_protocol().map(<[u8]>::to_vec),
        })
    })
    .await
    .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "client step")))
}

fn rustls_error(error: &io::Error) -> &rustls::Error {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .unwrap_or_else(|| panic!("expected a TLS error, got {error:?}"))
}

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A pinned client restricted to one protocol version.
fn pinned_client_for(
    fingerprint: Fingerprint,
    version: &'static rustls::SupportedProtocolVersion,
) -> Arc<ClientConfig> {
    let config = ClientConfig::builder_with_provider(ring())
        .with_protocol_versions(&[version])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(fingerprint)))
        .with_no_client_auth();
    Arc::new(config)
}

/// Presents one certificate while signing with an unrelated key: a peer
/// that copied the public fleet certificate but does not hold its key.
#[derive(Debug)]
struct MismatchedKey(Arc<CertifiedKey>);

impl ResolvesServerCert for MismatchedKey {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// A server that presents `certificate_of`'s certificate but signs with
/// `key_dir`'s private key.
fn impostor_config(certificate_of: &ServerIdentity, key_dir: &TempDir) -> Arc<ServerConfig> {
    let key = PrivateKeyDer::from_pem_file(key_dir.path().join(KEY_FILE)).unwrap();
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let certified = CertifiedKey::new(vec![certificate_of.cert_der().clone()], signing_key);
    assert!(
        certified.keys_match().is_err(),
        "the impostor must not hold the real key"
    );
    let config = ServerConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(MismatchedKey(Arc::new(certified))));
    Arc::new(config)
}

#[tokio::test]
async fn pinned_client_connects_and_exchanges_bytes() {
    let (_dir, identity) = identity();
    let (address, server) = serve(server_config(&identity).unwrap(), 1).await;

    let client = pinned_client_config(identity.fingerprint()).unwrap();
    let result = exchange(address, client, "127.0.0.1").await.unwrap();
    assert_eq!(result.reply, REPLY);
    assert_eq!(result.version, ProtocolVersion::TLSv1_3);
    assert_eq!(result.alpn, None, "the client offered no ALPN protocol");

    let outcomes = server.await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].as_ref().unwrap(), REQUEST);
}

#[tokio::test]
async fn the_pin_survives_a_restart_of_the_server() {
    let (dir, identity) = identity();
    let fingerprint = identity.fingerprint();
    drop(identity);

    // A second start loads the stored identity; the old pin still works.
    let reloaded = ServerIdentity::load_or_generate(dir.path(), &[]).unwrap();
    assert!(!reloaded.was_generated());
    let (address, server) = serve(server_config(&reloaded).unwrap(), 1).await;
    let client = pinned_client_config(fingerprint).unwrap();
    assert_eq!(
        exchange(address, client, "127.0.0.1").await.unwrap().reply,
        REPLY
    );
    assert!(server.await.unwrap()[0].is_ok());
}

#[tokio::test]
async fn a_client_pinned_to_a_different_fingerprint_is_refused() {
    let (_dir, identity) = identity();
    let (address, server) = serve(server_config(&identity).unwrap(), 1).await;

    let wrong = Fingerprint::of_certificate(b"some other certificate");
    let client = pinned_client_config(wrong).unwrap();
    let error = exchange(address, client, "127.0.0.1")
        .await
        .expect_err("the handshake must fail");
    let mismatch = pin_mismatch(rustls_error(&error)).expect("a pin mismatch");
    assert_eq!(mismatch.expected, wrong);
    assert_eq!(mismatch.presented, identity.fingerprint());

    // The server never received application data.
    let outcomes = server.await.unwrap();
    assert!(outcomes[0].is_err());
}

#[tokio::test]
async fn a_server_with_the_same_names_but_another_certificate_is_refused() {
    let (_dir_a, a) = identity();
    let (_dir_b, b) = identity();
    assert_ne!(a.fingerprint(), b.fingerprint());

    // Every name in B's certificate equals A's, and the client connects
    // under one of those names. Only the pin tells them apart.
    let (address, server) = serve(server_config(&b).unwrap(), 2).await;
    let pinned_to_a = pinned_client_config(a.fingerprint()).unwrap();
    let error = exchange(address, pinned_to_a, NAMES[0])
        .await
        .expect_err("the handshake must fail");
    let mismatch = pin_mismatch(rustls_error(&error)).expect("a pin mismatch");
    assert_eq!(mismatch.expected, a.fingerprint());
    assert_eq!(mismatch.presented, b.fingerprint());

    // The same server is fine for a client that pinned it.
    let pinned_to_b = pinned_client_config(b.fingerprint()).unwrap();
    assert_eq!(
        exchange(address, pinned_to_b, NAMES[0])
            .await
            .unwrap()
            .reply,
        REPLY
    );

    let outcomes = server.await.unwrap();
    assert!(outcomes[0].is_err());
    assert!(outcomes[1].is_ok());
}

#[tokio::test]
async fn the_host_name_is_irrelevant() {
    let (_dir, identity) = identity();
    // None of these appears in the certificate, except the third.
    let server_names = [
        "127.0.0.1",
        "localhost",
        NAMES[0],
        "unrelated.example",
        "::1",
    ];
    let (address, server) = serve(server_config(&identity).unwrap(), server_names.len()).await;

    for server_name in server_names {
        let client = pinned_client_config(identity.fingerprint()).unwrap();
        let result = exchange(address, client, server_name)
            .await
            .unwrap_or_else(|error| panic!("{server_name}: {error}"));
        assert_eq!(result.reply, REPLY, "{server_name}");
    }
    assert!(server.await.unwrap().iter().all(Result::is_ok));
}

#[tokio::test]
async fn connecting_by_the_name_localhost_works_with_the_same_pin() {
    let (_dir, identity) = identity();
    let (address, server) = serve(server_config(&identity).unwrap(), 1).await;

    // Resolve `localhost` for real and keep the IPv4 result, which is where
    // the test server listens.
    let resolved = tokio::net::lookup_host(("localhost", address.port()))
        .await
        .unwrap()
        .find(SocketAddr::is_ipv4)
        .expect("localhost resolves to an IPv4 loopback address");
    assert!(resolved.ip().is_loopback());

    let client = pinned_client_config(identity.fingerprint()).unwrap();
    let result = exchange(resolved, client, "localhost").await.unwrap();
    assert_eq!(result.reply, REPLY);
    assert!(server.await.unwrap()[0].is_ok());
}

#[tokio::test]
async fn tls12_handshakes_are_pinned_and_signature_checked() {
    let (_dir, identity) = identity();
    let (address, server) = serve(server_config(&identity).unwrap(), 2).await;

    let client = pinned_client_for(identity.fingerprint(), &rustls::version::TLS12);
    let result = exchange(address, client, "127.0.0.1").await.unwrap();
    assert_eq!(result.reply, REPLY);
    assert_eq!(result.version, ProtocolVersion::TLSv1_2);

    let wrong = Fingerprint::of_certificate(b"some other certificate");
    let client = pinned_client_for(wrong, &rustls::version::TLS12);
    let error = exchange(address, client, "127.0.0.1")
        .await
        .expect_err("the handshake must fail");
    assert!(pin_mismatch(rustls_error(&error)).is_some());

    let outcomes = server.await.unwrap();
    assert!(outcomes[0].is_ok());
    assert!(outcomes[1].is_err());
}

/// The fleet certificate is public. Presenting it without the private key
/// must not be enough, in either protocol version.
#[tokio::test]
async fn the_pinned_certificate_without_its_key_is_refused() {
    let (_dir_real, real) = identity();
    let (dir_other, _other) = identity();

    for version in [&rustls::version::TLS13, &rustls::version::TLS12] {
        let (address, server) = serve(impostor_config(&real, &dir_other), 1).await;
        let client = pinned_client_for(real.fingerprint(), version);
        let error = exchange(address, client, "127.0.0.1")
            .await
            .expect_err("the handshake must fail");
        let tls_error = rustls_error(&error);
        // The pin matched; the handshake signature did not.
        assert!(
            pin_mismatch(tls_error).is_none(),
            "{version:?}: {tls_error}"
        );
        assert!(
            matches!(
                tls_error,
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature)
            ),
            "{version:?}: {tls_error:?}"
        );
        assert!(server.await.unwrap()[0].is_err());
    }
}

#[tokio::test]
async fn the_server_selects_h2_or_http11_by_alpn() {
    let (_dir, identity) = identity();
    let (address, server) = serve(server_config(&identity).unwrap(), 3).await;

    for (offered, selected) in [
        (vec![ALPN_HTTP11.to_vec()], ALPN_HTTP11),
        (vec![ALPN_H2.to_vec()], ALPN_H2),
        // The server's preference decides when the client offers both.
        (vec![ALPN_HTTP11.to_vec(), ALPN_H2.to_vec()], ALPN_H2),
    ] {
        let mut config = (*pinned_client_config(identity.fingerprint()).unwrap()).clone();
        config.alpn_protocols = offered;
        let result = exchange(address, Arc::new(config), "127.0.0.1")
            .await
            .unwrap();
        assert_eq!(result.alpn.as_deref(), Some(selected));
        assert_eq!(result.reply, REPLY);
    }
    assert!(server.await.unwrap().iter().all(Result::is_ok));
}

/// The pin fixes the exact certificate, so its dates are not consulted. A
/// fleet does not stop working the day its certificate would expire.
#[tokio::test]
async fn an_expired_certificate_is_still_accepted_by_its_pin() {
    let dir = tempfile::tempdir().unwrap();
    // 2001-09-09: a certificate generated then expired in 2011.
    let clock = ManualClock::at(UnixMillis(1_000_000_000_000));
    let identity = ServerIdentity::load_or_generate_at(dir.path(), &[], &clock).unwrap();
    let (address, server) = serve(server_config(&identity).unwrap(), 1).await;

    let client = pinned_client_config(identity.fingerprint()).unwrap();
    assert_eq!(
        exchange(address, client, "127.0.0.1").await.unwrap().reply,
        REPLY
    );
    assert!(server.await.unwrap()[0].is_ok());
}
