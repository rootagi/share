//! Terminal UI (Ratatui + Crossterm).
//!
//! ```text
//! crossterm EventStream ─┐
//!                        ├─▶ tui::run ─▶ App::handle / App::on_tick ─▶ ui::draw
//! 100 ms interval ───────┘                    ▲
//!                                  Metrics::snapshot() + LogBuffer::recent()
//! ```
//!
//! The TUI only *reads* shared state. Speeds are computed by the metrics sampler,
//! so a frame costs one cheap snapshot copy and a diff-based redraw.

pub mod app;
pub mod events;
pub mod ui;
pub mod widgets;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, ShareError};
use crate::metrics::Shared;
use app::App;

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Leave raw mode and the alternate screen. Idempotent and safe to call from
/// anywhere (signal handler, panic path, normal exit).
pub fn restore_terminal() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        ratatui::restore();
    }
}

/// Restores the terminal when dropped, so every exit path (including `?`) cleans up.
struct TerminalGuard(DefaultTerminal);

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        // `try_init` also installs a panic hook that restores the terminal first.
        let terminal = ratatui::try_init()?;
        ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self(terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Run the interactive UI until the user quits or `token` is cancelled.
/// On quit the token is cancelled so the server shuts down too.
pub async fn run(
    state: Shared,
    token: CancellationToken,
    direct_url: Option<String>,
) -> Result<()> {
    let mut guard =
        TerminalGuard::new().map_err(|e| ShareError::io("initialising the terminal", e))?;
    let mut app = App::new(state, direct_url);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result: io::Result<()> = loop {
        if let Err(e) = guard.0.draw(|f| ui::draw(f, &app)) {
            break Err(e);
        }
        tokio::select! {
            _ = token.cancelled() => break Ok(()),
            _ = tick.tick() => app.on_tick(),
            ev = events.next() => match ev {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    app.handle(events::map_key(key));
                }
                Some(Ok(_)) => {} // resize and mouse events just trigger a redraw
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
        }
        if app.should_quit {
            break Ok(());
        }
    };
    token.cancel();
    drop(guard);
    result.map_err(|e| ShareError::io("terminal I/O", e))
}
