//! Rendering. Pure function of [`App`]; no I/O and no locks beyond what `App` already copied.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Sparkline};

use super::app::{App, View};
use super::widgets::{self, ACCENT, DIM, ERR, OK, WARN, bold, dim, label};
use crate::config::RootKind;
use crate::metrics::state::ServerStatus;
use crate::metrics::{Direction, TransferInfo, TransferStatus};
use crate::util::{format_bytes, format_duration, format_speed, truncate_middle};

const MIN_WIDTH: u16 = 64;
const MIN_HEIGHT: u16 = 20;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small ({}×{}). Resize to at least {MIN_WIDTH}×{MIN_HEIGHT}, or press q to quit.",
            area.width, area.height
        );
        f.render_widget(
            Paragraph::new(msg).wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }

    let (color, status_text) = match app.state.status() {
        ServerStatus::Starting => (WARN, "STARTING"),
        ServerStatus::Running => (OK, "RUNNING"),
        ServerStatus::Stopping => (WARN, "STOPPING"),
        ServerStatus::Stopped => (ERR, "STOPPED"),
    };
    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(Span::styled(" SHARE ", bold().fg(ACCENT))).left_aligned())
        .title(
            Line::from(vec![
                Span::styled(app.state.protocol(), bold()),
                Span::raw(" "),
                Span::styled("●", Style::new().fg(color)),
                Span::styled(format!(" {status_text} "), Style::new().fg(color)),
            ])
            .right_aligned(),
        );
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    match app.view {
        View::Dashboard => draw_dashboard(f, app, inner),
        View::Logs => {
            let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner);
            draw_logs(f, app, rows[0]);
            draw_footer(f, app, rows[1]);
        }
    }

    if app.show_help {
        draw_help(f, app, area);
    } else if app.show_qr {
        draw_qr(f, app, area);
    }
}

fn section(title: &str, borders: Borders) -> Block<'static> {
    Block::new()
        .borders(borders)
        .border_style(dim())
        .title(Span::styled(
            format!(" {title} "),
            label().add_modifier(Modifier::BOLD),
        ))
}

fn draw_dashboard(f: &mut Frame, app: &App, inner: Rect) {
    let info_w = inner.width.saturating_sub(2) as usize;
    let info = info_lines(app, info_w);
    let info_h = (info.len() as u16 + 1).min(inner.height / 2);
    let clients_h = app.snapshot.clients.len().clamp(1, 5) as u16 + 1;
    let rows = Layout::vertical([
        Constraint::Length(info_h),
        Constraint::Min(6),
        Constraint::Length(clients_h),
        Constraint::Length(1),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(info),
        Rect {
            x: rows[0].x + 1,
            width: rows[0].width.saturating_sub(2),
            ..rows[0]
        },
    );

    let cols =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[1]);
    draw_transfers(f, app, cols[0]);
    draw_network(f, app, cols[1]);
    draw_clients(f, app, rows[2]);
    draw_footer(f, app, rows[3]);
}

fn kv(key: &str) -> Span<'static> {
    Span::styled(format!("{key:<9}"), label())
}

fn info_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let cfg = &app.state.config;
    let mut lines = Vec::new();

    let kind = if cfg.root.kind == RootKind::Dir {
        "folder"
    } else {
        "file"
    };
    let mut meta = format!("  {kind}");
    if app.state.is_upload_enabled() {
        meta.push_str(" · uploads on");
    } else {
        meta.push_str(" · read-only");
    }
    if let Some(rate) = cfg.rate_limit {
        meta.push_str(&format!(" · limit {}/s", format_bytes(rate)));
    }
    let max_path = width.saturating_sub(9 + meta.chars().count()).max(12);
    lines.push(Line::from(vec![
        kv("Sharing"),
        Span::styled(truncate_middle(&cfg.root.display, max_path), bold()),
        Span::styled(meta, dim()),
    ]));

    if app.urls.is_empty() {
        lines.push(Line::from(vec![
            kv("URL"),
            Span::styled("no address detected", Style::new().fg(WARN)),
        ]));
    }
    for (i, u) in app.urls.iter().enumerate().take(4) {
        let selected = i == app.url_index;
        let first = if i == 0 { kv("URL") } else { kv("") };
        let marker = if app.urls.len() > 1 {
            if selected { "▸ " } else { "  " }
        } else {
            ""
        };
        let style = if selected {
            bold().fg(ACCENT)
        } else {
            Style::new().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            first,
            Span::styled(marker, Style::new().fg(ACCENT)),
            Span::styled(u.url.clone(), style),
            Span::styled(format!("   {} · {}", u.iface, u.label), dim()),
        ]));
    }
    if app.urls.len() > 4 {
        lines.push(Line::from(vec![
            kv(""),
            Span::styled(
                format!("(+{} more, press r to refresh)", app.urls.len() - 4),
                dim(),
            ),
        ]));
    }
    if let Some(d) = &app.direct_url {
        lines.push(Line::from(vec![
            kv("Direct"),
            Span::styled(d.clone(), bold().fg(ACCENT)),
        ]));
    }
    if let Some(fp) = &app.state.network.tls_fingerprint {
        lines.push(Line::from(vec![
            kv("TLS"),
            Span::styled(
                format!("self-signed · SHA-256 {}", widgets::short_fingerprint(fp)),
                dim(),
            ),
        ]));
    }
    if let Some(auth) = &cfg.auth {
        lines.push(Line::from(vec![
            kv("Auth"),
            Span::styled(
                format!("protected ({})", auth.display),
                Style::new().fg(OK).add_modifier(Modifier::BOLD),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            kv(""),
            Span::styled(
                "LAN TRUST MODE",
                Style::new().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  anyone who can reach this server can read the shared files",
                Style::new().fg(WARN),
            ),
        ]));
    }
    lines
}

fn direction_arrow(d: Direction) -> &'static str {
    match d {
        Direction::Download => "↓",
        Direction::Upload => "↑",
    }
}

fn transfer_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let s = &app.snapshot;
    if s.active.is_empty() && s.finished.is_empty() {
        return vec![Line::from(Span::styled("Waiting for transfers…", dim()))];
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let bar_w = width.saturating_sub(34).clamp(8, 40);
    for (idx, t) in s.active.iter().enumerate() {
        let marker = if s.active.len() > 1 {
            if idx == app.selected_transfer {
                "▸ "
            } else {
                "  "
            }
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::new().fg(ACCENT)),
            Span::styled(
                format!("{} ", direction_arrow(t.direction)),
                Style::new().fg(ACCENT),
            ),
            Span::styled(
                truncate_middle(&t.filename, width.saturating_sub(4)),
                bold(),
            ),
        ]));
        let mut bar_line = match t.progress() {
            Some(p) => {
                let mut spans = widgets::progress_bar(p, bar_w, ACCENT);
                spans.push(Span::raw(format!(" {:>3.0}%", p * 100.0)));
                spans
            }
            None => vec![Span::styled("receiving…".to_string(), dim())],
        };
        bar_line.push(Span::styled(
            format!("  {}", format_speed(t.speed)),
            Style::new().fg(OK),
        ));
        if let Some(eta) = t.eta() {
            bar_line.push(Span::styled(
                format!("  ETA {}", format_duration(eta)),
                dim(),
            ));
        }
        lines.push(Line::from(bar_line));
        let amount = if t.total > 0 {
            format!(
                "{} / {}",
                format_bytes(t.transferred),
                format_bytes(t.total)
            )
        } else {
            format_bytes(t.transferred)
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{amount}   {}   avg {}   peak {}",
                t.client,
                format_speed(t.avg_speed),
                format_speed(t.peak_speed)
            ),
            dim(),
        )));
        lines.push(Line::raw(""));
    }
    if !s.finished.is_empty() {
        lines.push(Line::from(Span::styled(
            "RECENT",
            dim().add_modifier(Modifier::BOLD),
        )));
        for t in s.finished.iter().take(50) {
            lines.push(finished_line(t, width));
        }
    }
    lines
}

fn finished_line(t: &TransferInfo, width: usize) -> Line<'static> {
    let (mark, color) = match t.status {
        TransferStatus::Completed => ("✓", OK),
        TransferStatus::Failed => ("✗", ERR),
        _ => ("–", WARN),
    };
    let tail = format!(
        "  {}  {}",
        format_bytes(t.transferred),
        format_speed(t.avg_speed)
    );
    let name_w = width.saturating_sub(tail.chars().count() + 3);
    Line::from(vec![
        Span::styled(
            format!("{mark} {} ", direction_arrow(t.direction)),
            Style::new().fg(color),
        ),
        Span::raw(truncate_middle(&t.filename, name_w)),
        Span::styled(tail, dim()),
    ])
}

fn draw_transfers(f: &mut Frame, app: &App, area: Rect) {
    let block = section("TRANSFERS", Borders::TOP);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pad = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let lines = transfer_lines(app, pad.width as usize);
    let max_scroll = lines.len().saturating_sub(pad.height as usize) as u16;
    f.render_widget(
        Paragraph::new(lines).scroll((app.transfer_scroll.min(max_scroll), 0)),
        pad,
    );
}

fn draw_network(f: &mut Frame, app: &App, area: Rect) {
    let block = section("NETWORK", Borders::TOP | Borders::LEFT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pad = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let s = &app.snapshot;
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(pad);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("↓ ", Style::new().fg(ACCENT)),
                Span::styled(format!("{:>11}", format_speed(s.download_speed)), bold()),
            ]),
            Line::from(vec![
                Span::styled("↑ ", Style::new().fg(Color::Magenta)),
                Span::styled(format!("{:>11}", format_speed(s.upload_speed)), bold()),
            ]),
        ]),
        parts[0],
    );

    let spark_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(parts[1]);
    for (row, history, color) in [
        (spark_rows[0], &app.down_history, ACCENT),
        (spark_rows[1], &app.up_history, Color::Magenta),
    ] {
        let w = row.width as usize;
        let skip = history.len().saturating_sub(w);
        let data: Vec<u64> = history.iter().skip(skip).copied().collect();
        f.render_widget(
            Sparkline::default()
                .data(data)
                .style(Style::new().fg(color)),
            row,
        );
    }

    let clients = s.clients.len();
    let stats = vec![
        Line::from(vec![
            Span::styled("Clients   ", label()),
            Span::raw(format!("{clients}  ({} conn)", s.active_connections)),
        ]),
        Line::from(vec![
            Span::styled("Peak      ", label()),
            Span::raw(format!(
                "↓ {}  ↑ {}",
                format_speed(s.peak_download_speed),
                format_speed(s.peak_upload_speed)
            )),
        ]),
        Line::from(vec![
            Span::styled("Sent      ", label()),
            Span::raw(format_bytes(s.bytes_sent)),
        ]),
        Line::from(vec![
            Span::styled("Received  ", label()),
            Span::raw(format_bytes(s.bytes_received)),
        ]),
        Line::from(vec![
            Span::styled("Done      ", label()),
            Span::raw(format!("{}  ", s.completed_transfers)),
            Span::styled(format!("aborted {}  ", s.aborted_transfers), dim()),
            Span::styled(
                format!("errors {}", s.errors),
                if s.errors > 0 {
                    Style::new().fg(ERR)
                } else {
                    dim()
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("Uptime    ", label()),
            Span::raw(format_duration(s.uptime)),
        ]),
    ];
    f.render_widget(Paragraph::new(stats), parts[2]);
}

fn draw_clients(f: &mut Frame, app: &App, area: Rect) {
    let block = section("CLIENTS", Borders::TOP);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pad = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let clients = &app.snapshot.clients;
    if clients.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("No clients connected", dim())),
            pad,
        );
        return;
    }
    let file_w = (pad.width as usize).saturating_sub(16 + 3 + 3 + 12);
    let lines: Vec<Line<'static>> = clients
        .iter()
        .take(pad.height as usize)
        .map(|c| {
            let (arrow, name, speed) = match (&c.direction, &c.current_file) {
                (Some(d), Some(n)) => (
                    direction_arrow(*d),
                    truncate_middle(n, file_w),
                    format_speed(c.speed),
                ),
                _ => ("·", "idle".to_string(), String::new()),
            };
            Line::from(vec![
                Span::raw(format!("{:<16}", c.ip.to_string())),
                Span::styled(format!(" {arrow} "), Style::new().fg(ACCENT)),
                Span::styled(
                    format!("{name:<file_w$}"),
                    if c.current_file.is_some() {
                        Style::new()
                    } else {
                        dim()
                    },
                ),
                Span::styled(format!(" {speed:>11}"), Style::new().fg(OK)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), pad);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if let Some((msg, _)) = &app.notice {
        spans.push(Span::styled(format!("● {msg}  │  "), Style::new().fg(WARN)));
    }
    for (key, text) in [
        ("O", "Open"),
        ("Y", "Copy"),
        ("U", "Uploads"),
        ("X", "Kill"),
        ("R", "Refresh"),
        (
            "L",
            if app.view == View::Logs {
                "Dashboard"
            } else {
                "Logs"
            },
        ),
        ("P", "QR"),
        ("C", "Clear"),
        ("?", "Help"),
        ("Q", "Quit"),
    ] {
        spans.push(Span::styled(format!("[{key}]"), bold().fg(ACCENT)));
        spans.push(Span::styled(format!(" {text} "), label()));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        },
    );
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let block = section(&format!("LOGS ({})", app.logs.len()), Borders::TOP);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let pad = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let lines: Vec<Line<'static>> = app
        .logs
        .iter()
        .map(|e| {
            let text: String = e
                .message
                .chars()
                .take((pad.width as usize).saturating_sub(16))
                .collect();
            Line::from(vec![
                Span::styled(format!("{} ", widgets::clock(e.elapsed)), dim()),
                Span::styled(format!("{:<5} ", e.level), widgets::level_style(e.level)),
                Span::styled(text, widgets::level_style(e.level)),
            ])
        })
        .collect();
    let h = pad.height as usize;
    let bottom = lines.len().saturating_sub(h);
    let offset = bottom.saturating_sub(app.log_scroll as usize);
    f.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), pad);
}

fn popup(f: &mut Frame, area: Rect, w: u16, h: u16, title: &str) -> Rect {
    let r = widgets::centered(w, h, area);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), bold().fg(ACCENT)));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let inner = popup(f, area, 74, 23, "HELP");
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!(" {k:<14}"), bold().fg(ACCENT)),
            Span::raw(d.to_string()),
        ])
    };
    let mut lines = vec![
        key("q  Ctrl+C", "quit and shut the server down gracefully"),
        key("o", "open the share URL in a browser on this machine"),
        key("y", "copy the share URL to clipboard (OSC 52)"),
        key("u", "toggle uploads on/off at runtime"),
        key("x", "kill / cancel the selected active transfer"),
        key("p", "show a QR code for the URL (scan with a phone)"),
        key("Tab", "select the next address when several are listed"),
        key("r", "re-detect network addresses"),
        key("l", "switch between dashboard and log view"),
        key("c", "clear finished transfers"),
        key("↑ ↓ PgUp PgDn", "select / scroll transfers and logs"),
        key("Esc", "close this window"),
        Line::raw(""),
    ];
    if let Some(fp) = &app.state.network.tls_fingerprint {
        lines.push(Line::from(Span::styled(
            " Self-signed certificate: browsers warn on first visit.",
            Style::new().fg(WARN),
        )));
        lines.push(Line::from(Span::styled(
            " Choose Advanced → Proceed. To verify, compare the SHA-256:",
            dim(),
        )));
        lines.push(Line::from(Span::raw(format!(
            " {}",
            &fp[..fp.len().min(47)]
        ))));
        lines.push(Line::from(Span::raw(format!(
            " {}",
            fp.get(48..).unwrap_or("")
        ))));
        if let Some(note) = &app.state.network.tls_note {
            lines.push(Line::from(Span::styled(format!(" {note}"), dim())));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " Plain HTTP: traffic is not encrypted.",
            Style::new().fg(WARN),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_qr(f: &mut Frame, app: &App, area: Rect) {
    if app.qr_lines.is_empty() {
        return;
    }
    let w = app.qr_lines[0].chars().count() as u16;
    let h = app.qr_lines.len() as u16;
    if area.width < w + 4 || area.height < h + 5 {
        let inner = popup(f, area, 40, 4, "QR");
        f.render_widget(
            Paragraph::new("Terminal too small for the QR code.\nEnlarge it or press Esc."),
            inner,
        );
        return;
    }
    let inner = popup(f, area, (w + 2).max(40), h + 4, "SCAN TO OPEN");
    let qr_style = Style::new().fg(Color::Black).bg(Color::White);
    let mut lines: Vec<Line<'static>> = app
        .qr_lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), qr_style)).centered())
        .collect();
    lines.push(
        Line::from(Span::styled(
            truncate_middle(&app.target_url(), inner.width as usize),
            bold(),
        ))
        .centered(),
    );
    lines.push(Line::from(Span::styled("Esc to close", Style::new().fg(DIM))).centered());
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::config::Config;
    use crate::logging::LogBuffer;
    use crate::metrics::state::{AppState, NetworkInfo, UrlEntry};
    use clap::Parser;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cli =
            Cli::try_parse_from(["share", dir.path().to_str().unwrap(), "--http", "--upload"])
                .unwrap();
        let config = Config::from_cli_with(cli, &[]).unwrap();
        let network = NetworkInfo {
            listen: "0.0.0.0:8080".parse().unwrap(),
            urls: vec![UrlEntry {
                iface: "wlan0".into(),
                label: "Wi-Fi",
                url: "http://192.168.1.15:8080".into(),
            }],
            addrs: vec![],
            tls_fingerprint: None,
            tls_note: None,
        };
        let state = AppState::new(config, network, LogBuffer::new(100));
        (App::new(state, None), dir)
    }

    #[test]
    fn dashboard_shows_the_key_facts() {
        let (mut app, _d) = app();
        let ip = "192.168.1.21".parse().unwrap();
        let _c = app.state.metrics.connection_opened(ip);
        let g = app
            .state
            .metrics
            .begin_transfer(ip, "movie.mkv".into(), Direction::Download, 1000);
        g.add(500);
        app.on_tick();
        let screen = render(&app, 100, 30);
        for needle in [
            "SHARE",
            "HTTP",
            "http://192.168.1.15:8080",
            "TRANSFERS",
            "NETWORK",
            "CLIENTS",
            "movie.mkv",
            "50%",
            "192.168.1.21",
            "LAN TRUST MODE",
            "uploads on",
            "[Q] Quit",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in:\n{screen}");
        }
        drop(g);
    }

    #[test]
    fn power_controls_toggle_uploads_kill_transfer_and_format_osc52() {
        let (mut app, _d) = app();
        assert!(app.state.is_upload_enabled());
        app.handle(super::super::events::Action::ToggleUploads);
        assert!(!app.state.is_upload_enabled());
        assert!(render(&app, 100, 30).contains("read-only"));
        app.handle(super::super::events::Action::ToggleUploads);
        assert!(app.state.is_upload_enabled());

        let ip = "192.168.1.21".parse().unwrap();
        let g = app
            .state
            .metrics
            .begin_transfer(ip, "slow.iso".into(), Direction::Download, 5000);
        app.on_tick();
        assert!(!g.transfer().cancel.is_cancelled());
        app.handle(super::super::events::Action::KillTransfer);
        assert!(g.transfer().cancel.is_cancelled());
        drop(g);

        let seq = super::super::app::osc52_sequence("http://192.168.1.15:8080");
        assert_eq!(seq, "\x1b]52;c;aHR0cDovLzE5Mi4xNjguMS4xNTo4MDgw\x07");
        app.handle(super::super::events::Action::CopyUrl);
        assert!(render(&app, 100, 30).contains("copied http://192.168.1.15:8080"));
    }

    #[test]
    fn log_view_and_overlays_render() {
        let (mut app, _d) = app();
        tracing::info!("hello from the test"); // may or may not reach the buffer; push directly too
        app.state
            .logs
            .push(tracing::Level::WARN, "disk is nearly full".into());
        app.handle(super::super::events::Action::ToggleLogs);
        assert!(render(&app, 100, 30).contains("disk is nearly full"));
        app.handle(super::super::events::Action::ToggleLogs);
        app.handle(super::super::events::Action::ToggleHelp);
        assert!(render(&app, 100, 30).contains("HELP"));
        app.handle(super::super::events::Action::ToggleQr);
        assert!(render(&app, 100, 36).contains("SCAN TO OPEN"));
    }

    #[test]
    fn tiny_terminals_get_a_message_instead_of_a_panic() {
        let (app, _d) = app();
        assert!(render(&app, 40, 10).contains("too small"));
        // Every size from 1x1 upward must be drawable.
        for (w, h) in [(1, 1), (64, 20), (80, 24), (200, 60)] {
            render(&app, w, h);
        }
    }
}
