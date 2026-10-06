//! TUI state and input handling (no drawing here).

use std::collections::VecDeque;
use std::io::Write as _;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use base64::Engine;

use super::events::Action;
use crate::config::RootKind;
use crate::logging::LogEntry;
use crate::metrics::state::UrlEntry;
use crate::metrics::{MetricsSnapshot, Shared};
use crate::network::{addresses, interfaces};
use crate::qr;

/// Samples kept for the throughput sparklines (one per 500 ms).
const HISTORY: usize = 120;
const NOTICE_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Logs,
}

pub struct App {
    pub state: Shared,
    pub direct_url: Option<String>,
    pub view: View,
    pub show_help: bool,
    pub show_qr: bool,
    pub should_quit: bool,
    pub snapshot: MetricsSnapshot,
    pub logs: Vec<LogEntry>,
    pub urls: Vec<UrlEntry>,
    pub url_index: usize,
    pub selected_transfer: usize,
    pub down_history: VecDeque<u64>,
    pub up_history: VecDeque<u64>,
    /// Lines scrolled up from the bottom of the log view.
    pub log_scroll: u16,
    /// Lines scrolled down in the transfers panel.
    pub transfer_scroll: u16,
    pub notice: Option<(String, Instant)>,
    pub qr_lines: Vec<String>,
    ticks: u64,
}

impl App {
    pub fn new(state: Shared, direct_url: Option<String>) -> Self {
        let urls = state.network.urls.clone();
        let mut app = Self {
            snapshot: state.metrics.snapshot(),
            logs: Vec::new(),
            show_qr: state.config.show_qr,
            urls,
            state,
            direct_url,
            view: View::Dashboard,
            show_help: false,
            should_quit: false,
            url_index: 0,
            selected_transfer: 0,
            down_history: VecDeque::with_capacity(HISTORY),
            up_history: VecDeque::with_capacity(HISTORY),
            log_scroll: 0,
            transfer_scroll: 0,
            notice: None,
            qr_lines: Vec::new(),
            ticks: 0,
        };
        app.refresh_qr();
        app.logs = app.state.logs.recent(500);
        app
    }

    /// The URL used by `o`, `y`, and the QR code: the direct file link, or the selected base URL.
    pub fn target_url(&self) -> String {
        self.direct_url
            .clone()
            .or_else(|| self.urls.get(self.url_index).map(|u| u.url.clone()))
            .unwrap_or_default()
    }

    /// Called every 100 ms.
    pub fn on_tick(&mut self) {
        self.ticks += 1;
        self.snapshot = self.state.metrics.snapshot();
        if self.snapshot.active.is_empty() {
            self.selected_transfer = 0;
        } else if self.selected_transfer >= self.snapshot.active.len() {
            self.selected_transfer = self.snapshot.active.len() - 1;
        }
        if self.view == View::Logs || self.ticks % 5 == 0 {
            self.logs = self.state.logs.recent(500);
        }
        if self.ticks % 5 == 0 {
            push_capped(&mut self.down_history, self.snapshot.download_speed);
            push_capped(&mut self.up_history, self.snapshot.upload_speed);
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > NOTICE_TTL)
        {
            self.notice = None;
        }
    }

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        // 1. Emit OSC 52 escape sequence (works over SSH and in OSC 52-capable terminals).
        let seq = osc52_sequence(text);
        let _ = std::io::stdout().write_all(seq.as_bytes());
        let _ = std::io::stdout().flush();

        // 2. On Unix (X11 / XWayland), serve CLIPBOARD + PRIMARY in-process advertising both
        // X11 atoms (UTF8_STRING, STRING, TEXT) and Wayland MIME types (text/plain;charset=utf-8).
        #[cfg(unix)]
        x11_set_clipboard(text);

        // 3. Also try platform clipboard utilities when available so clipboard contents
        // persist even after the TUI exits.
        try_external_clipboard(text);
    }

    pub fn handle(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::Open => {
                let url = self.target_url();
                match open::that_detached(&url) {
                    Ok(()) => self.notify(format!("opened {url}")),
                    Err(e) => self.notify(format!("could not open a browser: {e}")),
                }
            }
            Action::CopyUrl => {
                if self.view == View::Logs && !self.logs.is_empty() {
                    let text = self
                        .logs
                        .iter()
                        .map(|e| {
                            format!(
                                "{:>6} {:<5} {}",
                                crate::util::format_duration(e.elapsed),
                                e.level.as_str(),
                                e.message
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let count = self.logs.len();
                    self.copy_to_clipboard(&text);
                    self.notify(format!("copied {count} log line(s) to clipboard"));
                } else {
                    let url = self.target_url();
                    if url.is_empty() {
                        self.notify("no URL available to copy");
                    } else {
                        self.copy_to_clipboard(&url);
                        self.notify(format!("copied {url} to clipboard"));
                    }
                }
            }
            Action::ToggleUploads => {
                let cfg = &self.state.config;
                if cfg.root.kind == RootKind::File && cfg.upload.dir.is_none() {
                    self.notify("uploads need --upload-dir when sharing a single file");
                } else {
                    let next = !self.state.is_upload_enabled();
                    self.state.set_upload_enabled(next);
                    if next {
                        tracing::info!("uploads enabled via TUI");
                        self.notify("uploads enabled");
                    } else {
                        tracing::info!("uploads disabled (read-only) via TUI");
                        self.notify("uploads disabled (read-only)");
                    }
                }
            }
            Action::KillTransfer => {
                if let Some(t) = self.snapshot.active.get(self.selected_transfer) {
                    let id = t.id;
                    if let Some(name) = self.state.metrics.registry.cancel(id) {
                        tracing::info!("operator cancelled transfer #{id} ({name})");
                        self.notify(format!("cancelled transfer: {name}"));
                    }
                } else {
                    self.notify("no active transfer to cancel");
                }
            }
            Action::Refresh => self.refresh_network(),
            Action::ToggleLogs => {
                self.view = if self.view == View::Logs {
                    View::Dashboard
                } else {
                    View::Logs
                };
                self.log_scroll = 0;
                self.logs = self.state.logs.recent(500);
            }
            Action::ClearCompleted => {
                self.state.metrics.registry.clear_finished();
                self.snapshot = self.state.metrics.snapshot();
                self.transfer_scroll = 0;
                self.notify("cleared finished transfers");
            }
            Action::ToggleHelp => {
                self.show_help = !self.show_help;
                self.show_qr = false;
            }
            Action::ToggleQr => {
                self.show_qr = !self.show_qr;
                self.show_help = false;
                self.refresh_qr();
            }
            Action::NextUrl => {
                if !self.urls.is_empty() {
                    self.url_index = (self.url_index + 1) % self.urls.len();
                    self.refresh_qr();
                }
            }
            Action::ScrollUp => self.scroll(-1),
            Action::ScrollDown => self.scroll(1),
            Action::PageUp => self.scroll(-10),
            Action::PageDown => self.scroll(10),
            Action::Close => {
                self.show_help = false;
                self.show_qr = false;
            }
            Action::None => {}
        }
    }

    fn scroll(&mut self, delta: i32) {
        match self.view {
            // In the log view "up" means towards older lines.
            View::Logs => {
                let max = self.logs.len().min(u16::MAX as usize) as i32;
                self.log_scroll = (self.log_scroll as i32 - delta).clamp(0, max) as u16;
            }
            View::Dashboard => {
                if !self.snapshot.active.is_empty() {
                    let max_idx = (self.snapshot.active.len() - 1) as i32;
                    let step = delta.signum();
                    self.selected_transfer =
                        (self.selected_transfer as i32 + step).clamp(0, max_idx) as usize;
                }
                self.transfer_scroll = (self.transfer_scroll as i32 + delta).clamp(0, 1000) as u16;
            }
        }
    }

    fn refresh_qr(&mut self) {
        self.qr_lines = qr::render_lines(&self.target_url()).unwrap_or_default();
    }

    /// Re-detect the machine's addresses (e.g. after switching Wi-Fi networks).
    /// The TLS certificate is not regenerated while running; new addresses may show a name mismatch.
    fn refresh_network(&mut self) {
        let cfg = &self.state.config;
        let port = self.state.network.listen.port();
        let addrs = addresses::advertised(cfg.bind, &interfaces::list());
        self.urls = addrs
            .iter()
            .map(|a| UrlEntry {
                iface: a.iface.clone(),
                label: a.label,
                url: cfg.format_base_url(SocketAddr::new(a.ip, port)),
            })
            .collect();
        self.url_index = 0;
        self.refresh_qr();
        self.notify(format!(
            "network refreshed: {} address(es)",
            self.urls.len()
        ));
    }
}

/// Format an OSC 52 terminal escape sequence that copies `text` to the system clipboard.
pub fn osc52_sequence(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

fn try_external_clipboard(text: &str) {
    use std::process::{Command, Stdio};

    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip.exe", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };

    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let payload = text.as_bytes().to_vec();
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&payload);
                    drop(stdin);
                    let _ = child.wait();
                });
                return;
            }
        }
    }

    // Fallback on Linux desktops (e.g. Ubuntu GNOME Wayland) when wl-copy/xclip/xsel are not installed.
    #[cfg(target_os = "linux")]
    {
        let script = "import sys, gi; gi.require_version('Gtk', '3.0'); from gi.repository import Gtk, Gdk, GLib; \
                      t = sys.stdin.read(); \
                      cb = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD); cb.set_text(t, -1); cb.store(); \
                      pr = Gtk.Clipboard.get(Gdk.SELECTION_PRIMARY); pr.set_text(t, -1); \
                      GLib.timeout_add(400, Gtk.main_quit); Gtk.main()";
        if let Ok(mut child) = Command::new("python3")
            .args(["-c", script])
            .env("GDK_BACKEND", "x11")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let payload = text.as_bytes().to_vec();
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&payload);
                    drop(stdin);
                    let _ = child.wait();
                });
            }
        }
    }
}

#[cfg(unix)]
struct X11Atoms {
    clipboard: u32,
    primary: u32,
    targets: u32,
    utf8_string: u32,
    string: u32,
    text: u32,
    mime_utf8: u32,
    mime_plain: u32,
}

#[cfg(unix)]
struct X11Clipboard {
    conn: std::sync::Arc<x11rb::rust_connection::RustConnection>,
    window: u32,
    atoms: X11Atoms,
    data: std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
}

#[cfg(unix)]
impl X11Clipboard {
    fn new() -> Option<Self> {
        use std::sync::{Arc, RwLock};
        use x11rb::connection::Connection;
        use x11rb::protocol::Event;
        use x11rb::protocol::xproto::{
            AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
            SelectionNotifyEvent, WindowClass,
        };
        use x11rb::rust_connection::RustConnection;
        use x11rb::wrapper::ConnectionExt as _;

        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let conn = Arc::new(conn);
        let screen = conn.setup().roots.get(screen_num)?;
        let window = conn.generate_id().ok()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )
        .ok()?
        .check()
        .ok()?;

        let intern = |name: &[u8]| -> Option<u32> {
            Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
        };
        let atoms = X11Atoms {
            clipboard: intern(b"CLIPBOARD")?,
            primary: u32::from(AtomEnum::PRIMARY),
            targets: intern(b"TARGETS")?,
            utf8_string: intern(b"UTF8_STRING")?,
            string: u32::from(AtomEnum::STRING),
            text: intern(b"TEXT")?,
            mime_utf8: intern(b"text/plain;charset=utf-8")?,
            mime_plain: intern(b"text/plain")?,
        };

        let data = Arc::new(RwLock::new(Vec::<u8>::new()));
        let conn_bg = Arc::clone(&conn);
        let data_bg = Arc::clone(&data);
        let supported = [
            atoms.targets,
            atoms.utf8_string,
            atoms.string,
            atoms.text,
            atoms.mime_utf8,
            atoms.mime_plain,
        ];
        let targets_atom = atoms.targets;
        let text_targets = [
            atoms.utf8_string,
            atoms.string,
            atoms.text,
            atoms.mime_utf8,
            atoms.mime_plain,
        ];

        std::thread::spawn(move || {
            while let Ok(event) = conn_bg.wait_for_event() {
                if let Event::SelectionRequest(ev) = event {
                    let prop = if ev.property == u32::from(AtomEnum::NONE) {
                        ev.target
                    } else {
                        ev.property
                    };
                    let handled = if ev.target == targets_atom {
                        conn_bg
                            .change_property32(
                                PropMode::REPLACE,
                                ev.requestor,
                                prop,
                                AtomEnum::ATOM,
                                &supported,
                            )
                            .is_ok()
                    } else if text_targets.contains(&ev.target) {
                        let bytes = data_bg.read().map(|g| g.clone()).unwrap_or_default();
                        conn_bg
                            .change_property8(
                                PropMode::REPLACE,
                                ev.requestor,
                                prop,
                                ev.target,
                                &bytes,
                            )
                            .is_ok()
                    } else {
                        false
                    };

                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: ev.time,
                        requestor: ev.requestor,
                        selection: ev.selection,
                        target: ev.target,
                        property: if handled {
                            prop
                        } else {
                            u32::from(AtomEnum::NONE)
                        },
                    };
                    let _ = conn_bg.send_event(false, ev.requestor, EventMask::NO_EVENT, notify);
                    let _ = conn_bg.flush();
                }
            }
        });

        Some(Self {
            conn,
            window,
            atoms,
            data,
        })
    }

    fn store(&self, text: &str) {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        if let Ok(mut guard) = self.data.write() {
            *guard = text.as_bytes().to_vec();
        }
        let _ =
            self.conn
                .set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME);
        let _ = self
            .conn
            .set_selection_owner(self.window, self.atoms.primary, x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }
}

#[cfg(unix)]
fn x11_set_clipboard(text: &str) {
    use std::sync::OnceLock;
    static INSTANCE: OnceLock<Option<X11Clipboard>> = OnceLock::new();
    if let Some(cb) = INSTANCE.get_or_init(X11Clipboard::new) {
        cb.store(text);
    }
}

fn push_capped(q: &mut VecDeque<u64>, v: u64) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}
