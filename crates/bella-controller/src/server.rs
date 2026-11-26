//! The listener: accepting connections, TLS, and serving each one until it
//! ends or the controller shuts down.

use crate::state::App;
use axum::Router;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tower::ServiceExt;
use tracing::{debug, warn};

/// The address a request came from, as seen on the socket. Nothing a client
/// sends can change it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// Accept connections until shutdown, then let open requests finish within
/// the shutdown grace and drop whatever is left.
pub(crate) async fn run(
    app: App,
    listener: TcpListener,
    tls: Option<Arc<ServerConfig>>,
    router: Router,
) {
    let acceptor = tls.map(TlsAcceptor::from);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = app.shutdown.cancelled() => break,
            // Reap finished connections so the set does not grow.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        // Out of descriptors or a connection that died in
                        // the backlog: neither is a reason to stop serving.
                        warn!(%error, "could not accept a connection");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let app = app.clone();
                let router = router.clone();
                let acceptor = acceptor.clone();
                connections.spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            let handshake = tokio::time::timeout(
                                app.tuning.tls_handshake_timeout,
                                acceptor.accept(stream),
                            );
                            match handshake.await {
                                Ok(Ok(stream)) => serve(app, stream, peer, router).await,
                                Ok(Err(error)) => debug!(%peer, %error, "TLS handshake failed"),
                                Err(_) => debug!(%peer, "TLS handshake timed out"),
                            }
                        }
                        None => serve(app, stream, peer, router).await,
                    }
                });
            }
        }
    }
    drop(listener);

    let drained = tokio::time::timeout(app.tuning.shutdown_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        debug!(
            open = connections.len(),
            "connections still open after the shutdown grace; closing them"
        );
        connections.shutdown().await;
    }
}

/// Serve one connection: HTTP/1.1 (with upgrades, for the worker socket) or
/// HTTP/2, whichever the client speaks.
async fn serve<I>(app: App, stream: I, peer: SocketAddr, router: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |mut request: hyper::Request<Incoming>| {
        request.extensions_mut().insert(PeerAddr(peer));
        router.clone().oneshot(request)
    });
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(app.tuning.header_read_timeout);
    builder.http2().timer(TokioTimer::new());
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                debug!(%peer, %error, "connection ended with an error");
            }
        }
        () = app.shutdown.cancelled() => {
            // Finish the request in flight, accept no further ones.
            connection.as_mut().graceful_shutdown();
            if let Err(error) = connection.await {
                debug!(%peer, %error, "connection ended with an error during shutdown");
            }
        }
    }
}
