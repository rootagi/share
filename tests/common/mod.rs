//! Shared test harness: starts the real server in-process on an ephemeral port.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use share::app::{self, Running};
use share::cli::Cli;
use share::config::Config;
use share::logging::LogBuffer;
use share::metrics::Shared;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Size of `big.bin` – deliberately not a multiple of the 256 KiB read chunk.
pub const BIG_LEN: usize = 3 * 1024 * 1024 + 17;

pub fn big_bytes() -> Vec<u8> {
    (0..BIG_LEN).map(|i| (i % 251) as u8).collect()
}

pub struct TestServer {
    pub base: String,
    pub addr: SocketAddr,
    /// The shared directory.
    pub root: PathBuf,
    /// Parent of `root`; contains `secret.txt`, which must never be reachable.
    pub outer: PathBuf,
    pub state: Shared,
    pub running: Running,
    _guard: tempfile::TempDir,
}

pub const SECRET: &str = "TOP-SECRET-OUTSIDE-THE-SHARE";

impl TestServer {
    /// Start a server over a standard fixture with extra CLI arguments (`--http` is implied
    /// unless `--cert` is given).
    pub async fn start(extra: &[&str]) -> Self {
        let guard = tempfile::tempdir().unwrap();
        let outer = std::fs::canonicalize(guard.path()).unwrap();
        let root = outer.join("share");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();
        std::fs::write(outer.join("secret.txt"), SECRET).unwrap();
        std::fs::write(root.join("hello.txt"), "hello world").unwrap();
        std::fs::write(root.join("empty.txt"), "").unwrap();
        std::fs::write(root.join("big.bin"), big_bytes()).unwrap();
        std::fs::write(root.join("sub/nested.txt"), "nested").unwrap();
        std::fs::write(root.join("日本語 file.txt"), "unicode").unwrap();
        std::fs::write(root.join(".secret"), "hidden").unwrap();
        Self::start_at(&root, outer, extra, Some(guard)).await
    }

    async fn start_at(
        path: &Path,
        outer: PathBuf,
        extra: &[&str],
        guard: Option<tempfile::TempDir>,
    ) -> Self {
        let mut args = vec![
            "share",
            path.to_str().unwrap(),
            "--port",
            "0",
            "--bind",
            "127.0.0.1",
            "--shutdown-timeout",
            "1",
        ];
        if !extra.contains(&"--cert") {
            args.push("--http");
        }
        args.extend_from_slice(extra);
        let cli = Cli::try_parse_from(args).unwrap();
        let config = Config::from_cli_with(cli, &[]).unwrap();
        let running = app::start(config, LogBuffer::new(200)).await.unwrap();
        let addr = running.local_addr;
        let scheme = if running.state.config.is_tls() {
            "https"
        } else {
            "http"
        };
        TestServer {
            base: format!("{scheme}://{addr}"),
            addr,
            root: path.to_path_buf(),
            outer,
            state: running.state.clone(),
            running,
            _guard: guard.unwrap_or_else(|| tempfile::tempdir().unwrap()),
        }
    }

    /// Share an arbitrary path (e.g. a single file) instead of the fixture.
    pub async fn start_path(path: &Path, extra: &[&str], keep: tempfile::TempDir) -> Self {
        Self::start_at(path, keep.path().to_path_buf(), extra, Some(keep)).await
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn stop(self) {
        self.running.shutdown().await;
    }

    /// Poll until `cond` holds (transfer bookkeeping settles a moment after the client finishes).
    pub async fn eventually(
        &self,
        what: &str,
        cond: impl Fn(&share::metrics::MetricsSnapshot) -> bool,
    ) {
        for _ in 0..100 {
            if cond(&self.state.metrics.snapshot()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!(
            "timed out waiting for: {what}; snapshot = {:?}",
            self.state.metrics.snapshot()
        );
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

/// Send a raw HTTP/1.1 request (so `..` and odd encodings reach the server untouched).
pub async fn raw_request(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

pub async fn raw_get(addr: SocketAddr, target: &str) -> (u16, String) {
    raw_request(
        addr,
        &format!("GET {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"),
    )
    .await
}
