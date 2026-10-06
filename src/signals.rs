//! Process signal handling.
//!
//! First SIGINT/SIGTERM/SIGHUP: begin graceful shutdown. A second one while
//! shutting down forces an immediate exit (after restoring the terminal), so a
//! 100 GB download can never trap the user in the program.

use tokio_util::sync::CancellationToken;

pub fn spawn(token: CancellationToken) {
    tokio::spawn(async move {
        let mut waiter = SignalWaiter::new();
        waiter.wait().await;
        tracing::info!("shutdown requested");
        token.cancel();
        waiter.wait().await;
        crate::tui::restore_terminal();
        eprintln!("forced exit");
        std::process::exit(130);
    });
}

#[cfg(unix)]
struct SignalWaiter {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl SignalWaiter {
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let make = |kind| signal(kind).expect("failed to install signal handler");
        Self {
            int: make(SignalKind::interrupt()),
            term: make(SignalKind::terminate()),
            hup: make(SignalKind::hangup()),
        }
    }
    async fn wait(&mut self) {
        tokio::select! {
            _ = self.int.recv() => {},
            _ = self.term.recv() => {},
            _ = self.hup.recv() => {},
        }
    }
}

#[cfg(not(unix))]
struct SignalWaiter;

#[cfg(not(unix))]
impl SignalWaiter {
    fn new() -> Self {
        Self
    }
    async fn wait(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
