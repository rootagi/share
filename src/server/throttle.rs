//! Global token-bucket bandwidth limiter (`--rate-limit`).
//!
//! `Throttle::acquire_delay` computes the exact wait needed to pace `n` bytes
//! under a shared token bucket. Because the bucket balance is allowed to go
//! negative (representing future time slices already reserved by concurrent
//! transfers), callers never need to loop on an async lock: they reserve their
//! slice in O(1) under a fast `std::sync::Mutex` and sleep outside the lock.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::server::download::FILE_CHUNK;

#[derive(Debug)]
pub struct Throttle {
    rate: u64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Available byte tokens (may be negative when future capacity is reserved).
    available: f64,
    last_refill: Instant,
}

impl Throttle {
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1);
        // Start with up to one chunk of burst so the very first read starts
        // immediately while subsequent reads are paced to `rate`.
        let initial = (rate as f64 * 0.1).min(FILE_CHUNK as f64);
        Self {
            rate,
            bucket: Mutex::new(Bucket {
                available: initial,
                last_refill: Instant::now(),
            }),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Reserve `bytes` from the token bucket and return how long the caller
    /// should wait before sending/processing the chunk (`None` if enough
    /// capacity is immediately available).
    pub fn acquire_delay(&self, bytes: u64) -> Option<Duration> {
        if bytes == 0 {
            return None;
        }
        let mut b = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.duration_since(b.last_refill).as_secs_f64();
        b.last_refill = now;

        let rate = self.rate as f64;
        let max_burst = (rate * 0.25).max(FILE_CHUNK as f64);
        b.available = (b.available + elapsed * rate).min(max_burst);
        b.available -= bytes as f64;

        if b.available >= 0.0 {
            None
        } else {
            let secs = (-b.available) / rate;
            Some(Duration::from_secs_f64(secs))
        }
    }

    /// Async convenience wrapper around [`Self::acquire_delay`].
    pub async fn acquire(&self, bytes: u64) {
        if let Some(delay) = self.acquire_delay(bytes) {
            tokio::time::sleep(delay).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_and_paces_tokens() {
        let t = Throttle::new(1_000_000); // 1 MB/s -> initial burst 100 KB
        assert!(t.acquire_delay(50_000).is_none());
        let wait = t.acquire_delay(550_000).expect("should require waiting");
        // 50k + 550k - 100k initial = 500k deficit -> ~500ms at 1 MB/s
        assert!(wait >= Duration::from_millis(450) && wait <= Duration::from_millis(550));
    }
}
