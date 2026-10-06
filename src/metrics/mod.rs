//! Metrics and shared state.
//!
//! ```text
//! HTTP handlers ──(atomics)──▶ Metrics ◀──(snapshot)── TUI / /api/status
//!                                 ▲
//!                         sampler task (500 ms)
//! ```

pub mod state;
pub mod transfer;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use state::{AppState, Metrics, MetricsSnapshot, Shared};
pub use transfer::{Direction, TransferGuard, TransferInfo, TransferStatus};

/// How often speeds are recomputed.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn the background task that turns byte counters into speeds.
pub fn spawn_sampler(metrics: Arc<Metrics>, token: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = Instant::now();
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tick.tick() => {
                    let now = Instant::now();
                    metrics.sample(now - last);
                    last = now;
                }
            }
        }
    })
}
