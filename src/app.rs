//! Application orchestration: wires configuration, network, TLS, the HTTP
//! server, the metrics sampler and shared state together.
//!
//! ```text
//! Config ─▶ network discovery ─▶ TLS setup ─▶ bind ─▶ AppState
//!                                                       │
//!                      ┌────────────────────────────────┼───────────────┐
//!                      ▼                                ▼               ▼
//!                server::serve                  metrics sampler     TUI / banner
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, RootKind};
use crate::error::Result;
use crate::logging::LogBuffer;
use crate::metrics::state::{NetworkInfo, UrlEntry};
use crate::metrics::{self, AppState, Shared};
use crate::network::{self, addresses};
use crate::server::{self, response::encode_path_segment};
use crate::tls;

/// A started server. Dropping it without calling [`shutdown`](Self::shutdown) leaves the tasks running.
pub struct Running {
    pub state: Shared,
    pub token: CancellationToken,
    pub local_addr: SocketAddr,
    server: JoinHandle<()>,
    sampler: JoinHandle<()>,
}

/// Bind, set up TLS and start serving. Everything that can fail does so here,
/// *before* any terminal UI starts, so errors print normally.
pub async fn start(config: Config, logs: Arc<LogBuffer>) -> Result<Running> {
    let ifaces = network::interfaces::list();
    let addrs = addresses::advertised(config.bind, &ifaces);
    let ips: Vec<_> = addrs.iter().map(|a| a.ip).collect();

    let tls = tls::setup(&config, &ips)?;
    let listener = server::bind(config.listen_addr())?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| crate::error::ShareError::io("reading local address", e))?;

    let urls = addrs
        .iter()
        .map(|a| UrlEntry {
            iface: a.iface.clone(),
            label: a.label,
            url: config.format_base_url(SocketAddr::new(a.ip, local_addr.port())),
        })
        .collect();

    let network = NetworkInfo {
        listen: local_addr,
        urls,
        addrs,
        tls_fingerprint: tls.as_ref().map(|t| t.fingerprint.clone()),
        tls_note: tls.as_ref().map(|t| t.note.clone()),
    };

    tracing::info!("server starting");
    tracing::info!("sharing {}", config.root.path.display());
    let state = AppState::new(config, network, logs);
    tracing::info!("{} listening on {local_addr}", state.protocol());
    if state.is_upload_enabled() {
        tracing::info!("uploads enabled");
    }

    let token = CancellationToken::new();
    let sampler = metrics::spawn_sampler(Arc::clone(&state.metrics), token.clone());
    let server = tokio::spawn(server::serve(
        Arc::clone(&state),
        listener,
        tls.map(|t| t.acceptor),
        token.clone(),
    ));

    if let Some(max) = state.config.max_downloads {
        let st = Arc::clone(&state);
        let tok = token.clone();
        tokio::spawn(async move {
            loop {
                if st.metrics.completed_downloads() >= max {
                    tracing::info!("download limit ({max}) reached, shutting down");
                    tok.cancel();
                    break;
                }
                tokio::select! {
                    _ = tok.cancelled() => break,
                    _ = st.metrics.notified_download() => {}
                }
            }
        });
    }

    if let Some(dur) = state.config.expire_after {
        let tok = token.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tok.cancelled() => {}
                _ = tokio::time::sleep(dur) => {
                    tracing::info!(
                        "share expired after {}, shutting down",
                        crate::util::format_duration(dur)
                    );
                    tok.cancel();
                }
            }
        });
    }

    Ok(Running {
        state,
        token,
        local_addr,
        server,
        sampler,
    })
}

impl Running {
    /// Direct download URL when a single file is shared.
    pub fn direct_url(&self) -> Option<String> {
        let cfg = &self.state.config;
        if cfg.root.kind != RootKind::File {
            return None;
        }
        let base = &self.state.network.urls.first()?.url;
        let name = cfg.root.path.file_name()?.to_string_lossy();
        Some(format!("{base}/download/{}", encode_path_segment(&name)))
    }

    /// Stop accepting, drain in-flight transfers (bounded by `--shutdown-timeout`) and stop all tasks.
    pub async fn shutdown(self) {
        self.token.cancel();
        let wait = self.state.config.shutdown_timeout + Duration::from_secs(2);
        if tokio::time::timeout(wait, self.server).await.is_err() {
            tracing::warn!("server did not stop in time");
        }
        let _ = self.sampler.await;
        tracing::info!("server stopped");
    }
}
