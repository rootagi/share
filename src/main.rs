use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, Parser};

use share::cli::Cli;
use share::config::Config;
use share::error::{Result, ShareError};
use share::logging::{self, LogBuffer};
use share::{app, banner, qr, signals, tui, util};

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "share", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }

    let config = match Config::from_cli(cli) {
        Ok(c) => c,
        Err(e) => return report(&e),
    };

    // The TUI needs a real terminal on both ends; otherwise fall back to plain output.
    let use_tui =
        !config.no_tui && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let logs = LogBuffer::new(1000);
    logging::init(config.log_level, !use_tui, logs.clone());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().thread_name("share-worker");
    if let Some(n) = config.workers {
        builder.worker_threads(n);
    }
    let runtime = match builder.build() {
        Ok(rt) => rt,
        Err(e) => return report(&ShareError::io("starting the async runtime", e)),
    };

    let result = runtime.block_on(run(config, logs, use_tui));
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => report(&e),
    }
}

async fn run(config: Config, logs: std::sync::Arc<LogBuffer>, use_tui: bool) -> Result<()> {
    let show_qr = config.show_qr;
    let open_browser = config.open_browser;
    let running = app::start(config, logs).await?;
    signals::spawn(running.token.clone());

    let first_url = running.state.network.urls.first().map(|u| u.url.clone());
    if open_browser {
        if let Some(url) = &first_url {
            if let Err(e) = open::that_detached(url) {
                tracing::warn!("could not open a browser: {e}");
            }
        }
    }

    if use_tui {
        let outcome = tui::run(
            running.state.clone(),
            running.token.clone(),
            running.direct_url(),
        )
        .await;
        running.token.cancel();
        let active = running.state.metrics.snapshot().active.len();
        if active > 0 {
            eprintln!(
                "shutting down, waiting for {active} active transfer(s)… (Ctrl+C again to force)"
            );
        }
        let state = running.state.clone();
        running.shutdown().await;
        print_summary(&state);
        outcome?;
    } else {
        let mut out = std::io::stdout().lock();
        let _ = out
            .write_all(banner::render(&running.state, running.direct_url().as_deref()).as_bytes());
        if show_qr {
            if let Some(url) = running.direct_url().or(first_url) {
                if let Ok(code) = qr::render_ansi(&url) {
                    let _ = writeln!(out, "  Scan to open {url}\n");
                    let _ = out.write_all(code.as_bytes());
                }
            }
        }
        let _ = out.flush();
        drop(out);
        running.token.cancelled().await;
        let state = running.state.clone();
        running.shutdown().await;
        print_summary(&state);
    }
    Ok(())
}

fn print_summary(state: &share::metrics::AppState) {
    let s = state.metrics.snapshot();
    eprintln!(
        "served {} sent, {} received, {} transfer(s) completed, {} error(s), uptime {}",
        util::format_bytes(s.bytes_sent),
        util::format_bytes(s.bytes_received),
        s.completed_transfers,
        s.errors,
        util::format_duration(s.uptime)
    );
}

fn report(err: &ShareError) -> ExitCode {
    eprintln!("error: {err}");
    if let Some(hint) = err.hint() {
        eprintln!("hint: {hint}");
    }
    ExitCode::from(1)
}
