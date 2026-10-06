//! Global counters, per-client bookkeeping and the shared [`AppState`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::transfer::{Direction, TransferGuard, TransferInfo, TransferRegistry, TransferStatus};
use crate::config::Config;
use crate::logging::LogBuffer;
use crate::network::LanAddr;
use crate::server::throttle::Throttle;
use crate::server::upload::PartialRegistry;

/// Smoothing factor for speeds (0..1, higher = more responsive).
pub const SPEED_ALPHA: f64 = 0.5;

/// Lock-free counters updated from transfer paths.
#[derive(Debug, Default)]
pub struct Counters {
    total_connections: AtomicU64,
    active_connections: AtomicU64,
    completed: AtomicU64,
    completed_downloads: AtomicU64,
    failed: AtomicU64,
    aborted: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    down_speed: AtomicU64,
    up_speed: AtomicU64,
    peak_down: AtomicU64,
    peak_up: AtomicU64,
    last_sent: AtomicU64,
    last_received: AtomicU64,
}

impl Counters {
    #[inline]
    pub fn record_bytes(&self, dir: Direction, n: u64) {
        match dir {
            Direction::Download => self.bytes_sent.fetch_add(n, Relaxed),
            Direction::Upload => self.bytes_received.fetch_add(n, Relaxed),
        };
    }
}

#[derive(Debug)]
struct ClientEntry {
    connections: u32,
}

/// Everything the TUI and `/api/status` need to know, copied out in one go.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub uptime: Duration,
    pub total_connections: u64,
    pub active_connections: u64,
    pub completed_transfers: u64,
    pub aborted_transfers: u64,
    pub errors: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub peak_download_speed: u64,
    pub peak_upload_speed: u64,
    pub active: Vec<TransferInfo>,
    pub finished: Vec<TransferInfo>,
    pub clients: Vec<ClientInfo>,
}

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub ip: IpAddr,
    pub connections: u32,
    pub active_transfers: usize,
    pub speed: u64,
    pub direction: Option<Direction>,
    pub current_file: Option<String>,
}

#[derive(Debug)]
pub struct Metrics {
    pub counters: Counters,
    pub registry: TransferRegistry,
    clients: Mutex<HashMap<IpAddr, ClientEntry>>,
    download_notify: Notify,
    started: Instant,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            counters: Counters::default(),
            registry: TransferRegistry::default(),
            clients: Mutex::new(HashMap::new()),
            download_notify: Notify::new(),
            started: Instant::now(),
        })
    }

    pub fn completed_downloads(&self) -> u64 {
        self.counters.completed_downloads.load(Relaxed)
    }

    pub async fn notified_download(&self) {
        self.download_notify.notified().await;
    }

    /// Register a new transfer and get the guard the streaming code holds on to.
    pub fn begin_transfer(
        self: &Arc<Self>,
        client: IpAddr,
        filename: String,
        direction: Direction,
        total: u64,
    ) -> TransferGuard {
        let label = filename.clone();
        let transfer = self.registry.register(client, filename, direction, total);
        match direction {
            Direction::Download => tracing::info!("download started: {label} → {client}"),
            Direction::Upload => tracing::info!("upload started: {label} ← {client}"),
        }
        TransferGuard::new(transfer, Arc::clone(self))
    }

    pub(crate) fn finish_transfer(
        &self,
        transfer: &Arc<super::transfer::Transfer>,
        status: TransferStatus,
        reason: Option<&str>,
        counts_toward_quota: bool,
    ) {
        let info = self.registry.finish(transfer, status);
        let verb = match info.direction {
            Direction::Download => "download",
            Direction::Upload => "upload",
        };
        match status {
            TransferStatus::Completed => {
                self.counters.completed.fetch_add(1, Relaxed);
                if info.direction == Direction::Download && counts_toward_quota {
                    self.counters.completed_downloads.fetch_add(1, Relaxed);
                    self.download_notify.notify_waiters();
                }
                tracing::info!(
                    "{verb} completed: {} ({} in {:.1}s)",
                    info.filename,
                    crate::util::format_bytes(info.transferred),
                    info.elapsed.as_secs_f64()
                );
            }
            TransferStatus::Failed => {
                self.counters.failed.fetch_add(1, Relaxed);
                tracing::warn!(
                    "{verb} failed: {}: {}",
                    info.filename,
                    reason.unwrap_or("error")
                );
            }
            _ => {
                self.counters.aborted.fetch_add(1, Relaxed);
                tracing::info!(
                    "{verb} aborted: {} (client {} disconnected after {})",
                    info.filename,
                    info.client,
                    crate::util::format_bytes(info.transferred)
                );
            }
        }
    }

    /// Track an open TCP connection; the guard decrements the counters on drop.
    pub fn connection_opened(self: &Arc<Self>, ip: IpAddr) -> ConnectionGuard {
        self.counters.total_connections.fetch_add(1, Relaxed);
        self.counters.active_connections.fetch_add(1, Relaxed);
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        let is_new = !clients.contains_key(&ip);
        clients
            .entry(ip)
            .or_insert(ClientEntry { connections: 0 })
            .connections += 1;
        drop(clients);
        if is_new {
            tracing::info!("client connected: {ip}");
        }
        ConnectionGuard {
            metrics: Arc::clone(self),
            ip,
        }
    }

    /// Recompute speeds. Called by the sampler task every ~500 ms – this is where the
    /// (small) per-tick arithmetic lives, so rendering never has to do it.
    pub fn sample(&self, dt: Duration) {
        for t in self.registry.active_handles() {
            t.sample(dt, SPEED_ALPHA);
        }
        let secs = dt.as_secs_f64().max(0.001);
        let c = &self.counters;
        for (total, last, speed, peak) in [
            (&c.bytes_sent, &c.last_sent, &c.down_speed, &c.peak_down),
            (&c.bytes_received, &c.last_received, &c.up_speed, &c.peak_up),
        ] {
            let now = total.load(Relaxed);
            let delta = now.saturating_sub(last.swap(now, Relaxed));
            let instant = delta as f64 / secs;
            let prev = speed.load(Relaxed) as f64;
            let smoothed = if prev == 0.0 {
                instant
            } else {
                SPEED_ALPHA * instant + (1.0 - SPEED_ALPHA) * prev
            };
            let smoothed = if smoothed < 1.0 { 0 } else { smoothed as u64 };
            speed.store(smoothed, Relaxed);
            peak.fetch_max(smoothed, Relaxed);
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let active = self.registry.active_info();
        let finished = self.registry.finished_info();
        let c = &self.counters;

        let mut clients: Vec<ClientInfo> = self
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(ip, e)| {
                let mine: Vec<&TransferInfo> = active.iter().filter(|t| t.client == *ip).collect();
                ClientInfo {
                    ip: *ip,
                    connections: e.connections,
                    active_transfers: mine.len(),
                    speed: mine.iter().map(|t| t.speed).sum(),
                    direction: mine.first().map(|t| t.direction),
                    current_file: mine.first().map(|t| t.filename.clone()),
                }
            })
            .collect();
        clients.sort_by(|a, b| {
            b.active_transfers
                .cmp(&a.active_transfers)
                .then(a.ip.cmp(&b.ip))
        });

        MetricsSnapshot {
            uptime: self.started.elapsed(),
            total_connections: c.total_connections.load(Relaxed),
            active_connections: c.active_connections.load(Relaxed),
            completed_transfers: c.completed.load(Relaxed),
            aborted_transfers: c.aborted.load(Relaxed),
            errors: c.failed.load(Relaxed),
            bytes_sent: c.bytes_sent.load(Relaxed),
            bytes_received: c.bytes_received.load(Relaxed),
            download_speed: c.down_speed.load(Relaxed),
            upload_speed: c.up_speed.load(Relaxed),
            peak_download_speed: c.peak_down.load(Relaxed),
            peak_upload_speed: c.peak_up.load(Relaxed),
            active,
            finished,
            clients,
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

pub struct ConnectionGuard {
    metrics: Arc<Metrics>,
    ip: IpAddr,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.metrics
            .counters
            .active_connections
            .fetch_sub(1, Relaxed);
        let mut clients = self
            .metrics
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = clients.get_mut(&self.ip) {
            entry.connections = entry.connections.saturating_sub(1);
            if entry.connections == 0 {
                clients.remove(&self.ip);
                drop(clients);
                tracing::debug!("client disconnected: {}", self.ip);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// A URL a client can open, with the interface it belongs to.
#[derive(Debug, Clone)]
pub struct UrlEntry {
    pub iface: String,
    pub label: &'static str,
    pub url: String,
}

/// Facts about how the server is reachable. Immutable after start-up.
#[derive(Debug, Clone)]
pub struct NetworkInfo {
    pub listen: SocketAddr,
    pub urls: Vec<UrlEntry>,
    pub addrs: Vec<LanAddr>,
    /// SHA-256 fingerprint of the served certificate (TLS only).
    pub tls_fingerprint: Option<String>,
    /// Where the auto-generated certificate lives, or a note about it.
    pub tls_note: Option<String>,
}

/// Everything shared between the HTTP server, the TUI and the sampler.
///
/// The server and the TUI each hold an `Arc<AppState>`; neither owns the other.
/// Mutable data lives in [`Metrics`] (atomics) and [`LogBuffer`].
pub struct AppState {
    pub config: Config,
    pub metrics: Arc<Metrics>,
    pub network: NetworkInfo,
    pub logs: Arc<LogBuffer>,
    pub throttle: Option<Arc<Throttle>>,
    pub partial_uploads: PartialRegistry,
    upload_enabled: AtomicBool,
    status: AtomicU8,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn new(config: Config, network: NetworkInfo, logs: Arc<LogBuffer>) -> Shared {
        let upload_enabled = AtomicBool::new(config.upload.enabled);
        let throttle = config.rate_limit.map(|r| Arc::new(Throttle::new(r)));
        Arc::new(Self {
            config,
            metrics: Metrics::new(),
            network,
            logs,
            throttle,
            partial_uploads: PartialRegistry::default(),
            upload_enabled,
            status: AtomicU8::new(ServerStatus::Starting as u8),
        })
    }

    pub fn is_upload_enabled(&self) -> bool {
        self.upload_enabled.load(Relaxed)
    }

    pub fn set_upload_enabled(&self, enabled: bool) {
        self.upload_enabled.store(enabled, Relaxed);
    }

    pub fn status(&self) -> ServerStatus {
        match self.status.load(Relaxed) {
            0 => ServerStatus::Starting,
            1 => ServerStatus::Running,
            2 => ServerStatus::Stopping,
            _ => ServerStatus::Stopped,
        }
    }

    pub fn set_status(&self, s: ServerStatus) {
        self.status.store(s as u8, Relaxed);
    }

    /// `"HTTPS"` or `"HTTP"`.
    pub fn protocol(&self) -> &'static str {
        if self.config.is_tls() {
            "HTTPS"
        } else {
            "HTTP"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_computes_speed_and_peak() {
        let m = Metrics::new();
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        let g = m.begin_transfer(ip, "f".into(), Direction::Download, 10_000_000);
        g.add(1_000_000);
        m.sample(Duration::from_millis(500));
        let s = m.snapshot();
        assert_eq!(s.download_speed, 2_000_000);
        assert_eq!(s.active[0].speed, 2_000_000);
        assert_eq!(s.peak_download_speed, 2_000_000);
        // Idle tick: speed decays, peak is retained.
        m.sample(Duration::from_millis(500));
        let s = m.snapshot();
        assert_eq!(s.download_speed, 1_000_000);
        assert_eq!(s.peak_download_speed, 2_000_000);
        drop(g);
    }

    #[test]
    fn connection_guard_tracks_clients() {
        let m = Metrics::new();
        let ip: IpAddr = "10.0.0.3".parse().unwrap();
        let a = m.connection_opened(ip);
        let b = m.connection_opened(ip);
        let s = m.snapshot();
        assert_eq!((s.active_connections, s.total_connections), (2, 2));
        assert_eq!(s.clients.len(), 1);
        assert_eq!(s.clients[0].connections, 2);
        drop(a);
        drop(b);
        let s = m.snapshot();
        assert_eq!(s.active_connections, 0);
        assert_eq!(s.total_connections, 2);
        assert!(s.clients.is_empty());
    }

    #[test]
    fn clients_show_their_active_transfer() {
        let m = Metrics::new();
        let ip: IpAddr = "10.0.0.4".parse().unwrap();
        let _c = m.connection_opened(ip);
        let _g = m.begin_transfer(ip, "movie.mkv".into(), Direction::Download, 100);
        let s = m.snapshot();
        assert_eq!(s.clients[0].current_file.as_deref(), Some("movie.mkv"));
        assert_eq!(s.clients[0].active_transfers, 1);
    }
}
