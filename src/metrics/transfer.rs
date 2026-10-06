//! Per-transfer tracking.
//!
//! The hot path (one call per streamed chunk) touches only atomics on an
//! `Arc<Transfer>` the handler already owns. The registry's lock is taken when
//! a transfer starts or ends and when the UI takes a snapshot – never while
//! reading a file, writing a file, or waiting for a client.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::state::Metrics;

/// How many finished transfers the registry remembers for the UI.
const FINISHED_KEEP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Server → client.
    Download,
    /// Client → server.
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferStatus {
    Active,
    Completed,
    /// The server hit an error (I/O failure, disk full, ...).
    Failed,
    /// The client went away before the transfer finished.
    Aborted,
}

impl TransferStatus {
    fn code(self) -> u8 {
        self as u8
    }
    fn from_code(c: u8) -> Self {
        match c {
            0 => Self::Active,
            1 => Self::Completed,
            2 => Self::Failed,
            _ => Self::Aborted,
        }
    }
}

/// Live state of one transfer. Cheap to share: everything mutable is atomic.
#[derive(Debug)]
pub struct Transfer {
    pub id: u64,
    pub client: IpAddr,
    pub filename: String,
    pub direction: Direction,
    /// Bytes this transfer is expected to move (0 = unknown).
    pub total: u64,
    pub started: Instant,
    /// Per-transfer cancellation token (fired by the TUI `x` action).
    pub cancel: CancellationToken,
    transferred: AtomicU64,
    /// Bytes counted at the previous sampler tick.
    last_sampled: AtomicU64,
    /// Smoothed speed in bytes/second, written by the sampler.
    speed: AtomicU64,
    peak: AtomicU64,
    status: AtomicU8,
    /// Milliseconds from start to end; 0 while running.
    ended_after_ms: AtomicU64,
}

impl Transfer {
    fn new(id: u64, client: IpAddr, filename: String, direction: Direction, total: u64) -> Self {
        Self {
            id,
            client,
            filename,
            direction,
            total,
            started: Instant::now(),
            cancel: CancellationToken::new(),
            transferred: AtomicU64::new(0),
            last_sampled: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            status: AtomicU8::new(TransferStatus::Active.code()),
            ended_after_ms: AtomicU64::new(0),
        }
    }

    pub fn transferred(&self) -> u64 {
        self.transferred.load(Relaxed)
    }

    pub fn status(&self) -> TransferStatus {
        TransferStatus::from_code(self.status.load(Relaxed))
    }

    pub fn speed(&self) -> u64 {
        self.speed.load(Relaxed)
    }

    /// Called by the sampler. Returns the number of bytes moved since the last tick.
    pub(crate) fn sample(&self, dt: Duration, alpha: f64) -> u64 {
        let now = self.transferred.load(Relaxed);
        let before = self.last_sampled.swap(now, Relaxed);
        let delta = now.saturating_sub(before);
        let instant = delta as f64 / dt.as_secs_f64().max(0.001);
        let previous = self.speed.load(Relaxed) as f64;
        let smoothed = if previous == 0.0 {
            instant
        } else {
            alpha * instant + (1.0 - alpha) * previous
        };
        let smoothed = if smoothed < 1.0 { 0 } else { smoothed as u64 };
        self.speed.store(smoothed, Relaxed);
        self.peak.fetch_max(smoothed, Relaxed);
        delta
    }

    /// A frozen copy for display. Arithmetic only – cheap enough to call every frame.
    pub fn info(&self) -> TransferInfo {
        let status = self.status();
        let ended = self.ended_after_ms.load(Relaxed);
        let elapsed = if ended > 0 {
            Duration::from_millis(ended)
        } else {
            self.started.elapsed()
        };
        let transferred = self.transferred();
        let avg = if elapsed.as_secs_f64() > 0.0 {
            (transferred as f64 / elapsed.as_secs_f64()) as u64
        } else {
            0
        };
        TransferInfo {
            id: self.id,
            client: self.client,
            filename: self.filename.clone(),
            direction: self.direction,
            total: self.total,
            transferred,
            speed: if status == TransferStatus::Active {
                self.speed()
            } else {
                0
            },
            avg_speed: avg,
            peak_speed: self.peak.load(Relaxed),
            elapsed,
            status,
        }
    }
}

/// Immutable snapshot of a [`Transfer`].
#[derive(Debug, Clone, Serialize)]
pub struct TransferInfo {
    pub id: u64,
    pub client: IpAddr,
    pub filename: String,
    pub direction: Direction,
    pub total: u64,
    pub transferred: u64,
    /// Smoothed current speed (bytes/s).
    pub speed: u64,
    /// Whole-transfer average (bytes/s).
    pub avg_speed: u64,
    pub peak_speed: u64,
    #[serde(serialize_with = "serialize_secs")]
    pub elapsed: Duration,
    pub status: TransferStatus,
}

fn serialize_secs<S: serde::Serializer>(
    d: &Duration,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

impl TransferInfo {
    /// 0.0..=1.0, or `None` when the total is unknown.
    pub fn progress(&self) -> Option<f64> {
        (self.total > 0).then(|| (self.transferred as f64 / self.total as f64).clamp(0.0, 1.0))
    }

    /// Estimated time remaining, from the current speed (falling back to the average).
    pub fn eta(&self) -> Option<Duration> {
        if self.total == 0 || self.status != TransferStatus::Active {
            return None;
        }
        let rate = if self.speed > 0 {
            self.speed
        } else {
            self.avg_speed
        };
        if rate == 0 {
            return None;
        }
        let remaining = self.total.saturating_sub(self.transferred);
        Some(Duration::from_secs(remaining / rate))
    }
}

#[derive(Debug, Default)]
pub struct TransferRegistry {
    next_id: AtomicU64,
    active: RwLock<HashMap<u64, Arc<Transfer>>>,
    finished: Mutex<VecDeque<TransferInfo>>,
}

impl TransferRegistry {
    pub fn register(
        &self,
        client: IpAddr,
        filename: String,
        direction: Direction,
        total: u64,
    ) -> Arc<Transfer> {
        let id = self.next_id.fetch_add(1, Relaxed) + 1;
        let transfer = Arc::new(Transfer::new(id, client, filename, direction, total));
        self.active
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, Arc::clone(&transfer));
        transfer
    }

    /// Move a transfer from "active" to the finished history.
    pub(crate) fn finish(&self, transfer: &Arc<Transfer>, status: TransferStatus) -> TransferInfo {
        transfer.ended_after_ms.store(
            (transfer.started.elapsed().as_millis() as u64).max(1),
            Relaxed,
        );
        transfer.status.store(status.code(), Relaxed);
        self.active
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&transfer.id);
        let info = transfer.info();
        let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        finished.push_front(info.clone());
        finished.truncate(FINISHED_KEEP);
        info
    }

    /// Handles to all running transfers (the lock is released before returning).
    pub fn active_handles(&self) -> Vec<Arc<Transfer>> {
        self.active
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub fn active_info(&self) -> Vec<TransferInfo> {
        let mut v: Vec<TransferInfo> = self.active_handles().iter().map(|t| t.info()).collect();
        v.sort_by_key(|t| t.id);
        v
    }

    /// Most recent first.
    pub fn finished_info(&self) -> Vec<TransferInfo> {
        self.finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear_finished(&self) {
        self.finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Cancel an active transfer by ID. Returns its filename if found.
    pub fn cancel(&self, id: u64) -> Option<String> {
        let active = self.active.read().unwrap_or_else(PoisonError::into_inner);
        let t = active.get(&id)?;
        t.cancel.cancel();
        Some(t.filename.clone())
    }
}

/// Ownership token for a running transfer.
///
/// Dropping the guard without calling [`complete`](Self::complete) or
/// [`fail`](Self::fail) records the transfer as *aborted* – which is exactly
/// what happens when a client disconnects and hyper drops the response body.
pub struct TransferGuard {
    transfer: Arc<Transfer>,
    metrics: Arc<Metrics>,
    counts_toward_quota: bool,
    done: bool,
}

impl TransferGuard {
    pub(crate) fn new(transfer: Arc<Transfer>, metrics: Arc<Metrics>) -> Self {
        Self {
            transfer,
            metrics,
            counts_toward_quota: true,
            done: false,
        }
    }

    /// Control whether completing this transfer increments the `--max-downloads` counter.
    pub fn with_quota(mut self, counts_toward_quota: bool) -> Self {
        self.counts_toward_quota = counts_toward_quota;
        self
    }

    pub fn transfer(&self) -> &Transfer {
        &self.transfer
    }

    /// Record `n` more bytes. Two relaxed atomic adds; no locks.
    #[inline]
    pub fn add(&self, n: u64) {
        self.transfer.transferred.fetch_add(n, Relaxed);
        self.metrics
            .counters
            .record_bytes(self.transfer.direction, n);
    }

    pub fn complete(mut self) {
        self.close(TransferStatus::Completed, None);
    }

    pub fn fail(mut self, reason: &str) {
        self.close(TransferStatus::Failed, Some(reason));
    }

    fn close(&mut self, status: TransferStatus, reason: Option<&str>) {
        if !self.done {
            self.done = true;
            self.metrics
                .finish_transfer(&self.transfer, status, reason, self.counts_toward_quota);
        }
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        self.close(TransferStatus::Aborted, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.168.1.9".parse().unwrap()
    }

    #[test]
    fn guard_records_completion_and_bytes() {
        let m = Metrics::new();
        let g = m.begin_transfer(ip(), "a.bin".into(), Direction::Download, 100);
        g.add(60);
        g.add(40);
        assert_eq!(m.registry.active_info().len(), 1);
        g.complete();
        assert!(m.registry.active_info().is_empty());
        let fin = m.registry.finished_info();
        assert_eq!(fin.len(), 1);
        assert_eq!(fin[0].status, TransferStatus::Completed);
        assert_eq!(fin[0].transferred, 100);
        assert_eq!(m.snapshot().bytes_sent, 100);
        assert_eq!(m.snapshot().completed_transfers, 1);
    }

    #[test]
    fn dropped_guard_counts_as_aborted() {
        let m = Metrics::new();
        {
            let g = m.begin_transfer(ip(), "a.bin".into(), Direction::Upload, 0);
            g.add(5);
        }
        let fin = m.registry.finished_info();
        assert_eq!(fin[0].status, TransferStatus::Aborted);
        let s = m.snapshot();
        assert_eq!(s.bytes_received, 5);
        assert_eq!(s.aborted_transfers, 1);
        assert_eq!(s.completed_transfers, 0);
    }

    #[test]
    fn failed_transfers_count_as_errors() {
        let m = Metrics::new();
        m.begin_transfer(ip(), "x".into(), Direction::Download, 10)
            .fail("disk on fire");
        assert_eq!(m.snapshot().errors, 1);
    }

    #[test]
    fn progress_and_eta() {
        let info = TransferInfo {
            id: 1,
            client: ip(),
            filename: "f".into(),
            direction: Direction::Download,
            total: 1000,
            transferred: 250,
            speed: 50,
            avg_speed: 40,
            peak_speed: 60,
            elapsed: Duration::from_secs(5),
            status: TransferStatus::Active,
        };
        assert_eq!(info.progress(), Some(0.25));
        assert_eq!(info.eta(), Some(Duration::from_secs(15)));
        let unknown = TransferInfo { total: 0, ..info };
        assert_eq!(unknown.progress(), None);
        assert_eq!(unknown.eta(), None);
    }

    #[test]
    fn history_is_bounded() {
        let m = Metrics::new();
        for _ in 0..(FINISHED_KEEP + 25) {
            m.begin_transfer(ip(), "f".into(), Direction::Download, 1)
                .complete();
        }
        assert_eq!(m.registry.finished_info().len(), FINISHED_KEEP);
    }
}
