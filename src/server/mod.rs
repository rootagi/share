//! HTTP(S) server: accept loop, connection handling and graceful shutdown.
//!
//! The server owns no application state of its own. It holds a [`Shared`]
//! handle and reports through [`crate::metrics`]; the TUI reads the same state.

pub mod archive;
pub mod auth;
pub mod dav;
pub mod download;
pub mod handlers;
pub mod response;
pub mod routes;
pub mod throttle;
pub mod upload;

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::config::Config;
use crate::error::{Result, ShareError};
use crate::metrics::Shared;
use crate::metrics::state::ServerStatus;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const LISTEN_BACKLOG: u32 = 1024;

/// Peer address, attached to every request as an extension.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub IpAddr);

/// Bind the listening socket, translating failures into friendly errors.
pub fn bind(addr: SocketAddr) -> Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .map_err(|e| ShareError::io("creating socket", e))?;
    // Lets the server restart immediately on Unix without waiting for TIME_WAIT sockets.
    // (On Windows, SO_REUSEADDR allows multiple active listeners to steal the same port.)
    #[cfg(not(windows))]
    socket
        .set_reuseaddr(true)
        .map_err(|e| ShareError::io("configuring socket", e))?;
    socket.bind(addr).map_err(|e| map_bind_error(addr, e))?;
    socket
        .listen(LISTEN_BACKLOG)
        .map_err(|e| map_bind_error(addr, e))
}

fn map_bind_error(addr: SocketAddr, e: std::io::Error) -> ShareError {
    match e.kind() {
        std::io::ErrorKind::AddrInUse => ShareError::AddrInUse(addr),
        _ => ShareError::Bind { addr, source: e },
    }
}

fn connection_builder(config: &Config) -> auto::Builder<TokioExecutor> {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    if config.http2 {
        // Generous flow-control windows so HTTP/2 does not cap single-stream throughput.
        builder
            .http2()
            .timer(TokioTimer::new())
            .initial_stream_window_size(8 * 1024 * 1024)
            .initial_connection_window_size(32 * 1024 * 1024)
            .max_frame_size(256 * 1024);
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT);
        builder
    } else {
        let mut builder = builder.http1_only();
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT);
        builder
    }
}

/// Accept connections until `shutdown` is cancelled, then drain gracefully.
///
/// Shutdown order: stop accepting → ask every connection to finish its in-flight
/// response → wait up to `shutdown_timeout` → return (remaining tasks are dropped
/// with the runtime, which also cleans up partial uploads).
pub async fn serve(
    state: Shared,
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    shutdown: CancellationToken,
) {
    let router = routes::build(Arc::clone(&state));
    let builder = Arc::new(connection_builder(&state.config));
    let limiter = Arc::new(Semaphore::new(state.config.max_connections));
    let tracker = TaskTracker::new();
    state.set_status(ServerStatus::Running);

    loop {
        // Take a permit *before* accepting: at the connection limit we simply stop
        // accepting and the kernel backlog absorbs bursts.
        let permit = tokio::select! {
            _ = shutdown.cancelled() => break,
            p = Arc::clone(&limiter).acquire_owned() => match p { Ok(p) => p, Err(_) => break },
        };
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    // EMFILE and friends: back off instead of spinning.
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let _ = stream.set_nodelay(true);

        let (router, builder, state, tls, shutdown) = (
            router.clone(),
            Arc::clone(&builder),
            Arc::clone(&state),
            tls.clone(),
            shutdown.clone(),
        );
        tracker.spawn(async move {
            let _permit = permit;
            match tls {
                Some(acceptor) => {
                    let mut first = [0u8; 1];
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut first)).await {
                        Ok(Ok(1)) if first[0] == 0x16 => {
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
                                .await
                            {
                                Ok(Ok(tls_stream)) => {
                                    serve_connection(
                                        TokioIo::new(tls_stream),
                                        peer,
                                        router,
                                        builder,
                                        state,
                                        shutdown,
                                    )
                                    .await
                                }
                                Ok(Err(e)) => {
                                    tracing::debug!("TLS handshake with {peer} failed: {e}")
                                }
                                Err(_) => tracing::debug!("TLS handshake with {peer} timed out"),
                            }
                        }
                        Ok(Ok(1)) => {
                            // Plain HTTP/WebDAV client (e.g. `dav://host:8080` in GNOME Files)
                            // connecting to the TLS port: serve HTTP directly instead of sending
                            // a binary TLS alert that breaks HTTP parsers.
                            serve_connection(
                                TokioIo::new(stream),
                                peer,
                                router,
                                builder,
                                state,
                                shutdown,
                            )
                            .await
                        }
                        _ => {}
                    }
                }
                None => {
                    serve_connection(TokioIo::new(stream), peer, router, builder, state, shutdown)
                        .await
                }
            }
        });
    }

    state.set_status(ServerStatus::Stopping);
    drop(listener); // stop accepting immediately
    tracker.close();
    let grace = state.config.shutdown_timeout;
    if tokio::time::timeout(grace, tracker.wait()).await.is_err() {
        tracing::warn!(
            "{} connection(s) still active after {}s; closing them",
            tracker.len(),
            grace.as_secs()
        );
    }
    state.set_status(ServerStatus::Stopped);
}

async fn serve_connection<I>(
    io: TokioIo<I>,
    peer: SocketAddr,
    router: Router,
    builder: Arc<auto::Builder<TokioExecutor>>,
    state: Shared,
    shutdown: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let _connection = state.metrics.connection_opened(peer.ip());
    let client = ClientAddr(peer.ip());
    let service = service_fn(move |mut req: hyper::Request<Incoming>| {
        req.extensions_mut().insert(client);
        let router = router.clone();
        async move { Ok::<_, Infallible>(router.oneshot(req).await.unwrap_or_else(|e| match e {})) }
    });

    let conn = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(conn);
    tokio::select! {
        res = conn.as_mut() => log_connection_result(peer, res),
        _ = shutdown.cancelled() => {
            conn.as_mut().graceful_shutdown();
            log_connection_result(peer, conn.await);
        }
    }
}

fn log_connection_result(
    peer: SocketAddr,
    res: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>,
) {
    if let Err(e) = res {
        // Resets and broken pipes are normal when a client navigates away or cancels a download.
        tracing::debug!("connection from {peer} ended: {e}");
    }
}
