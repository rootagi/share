//! Plain-text start-up banner for `--no-tui` mode.

use std::fmt::Write;

use crate::config::RootKind;
use crate::metrics::AppState;

pub fn render(state: &AppState, direct_url: Option<&str>) -> String {
    let cfg = &state.config;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "share {}  ({})",
        env!("CARGO_PKG_VERSION"),
        state.protocol()
    );
    let kind = if cfg.root.kind == RootKind::Dir {
        "folder"
    } else {
        "file"
    };
    let _ = writeln!(out, "  sharing {kind}  {}", cfg.root.display);
    let _ = writeln!(out, "  listening     {}", state.network.listen);
    let _ = writeln!(out);
    for u in &state.network.urls {
        let _ = writeln!(
            out,
            "  {:<22} {}",
            format!("{} ({})", u.iface, u.label),
            u.url
        );
    }
    if let Some(d) = direct_url {
        let _ = writeln!(out, "\n  direct download: {d}");
    }
    if cfg.upload.enabled {
        let _ = writeln!(out, "\n  uploads enabled");
    }
    if let Some(fp) = &state.network.tls_fingerprint {
        let _ = writeln!(
            out,
            "\n  Self-signed certificates trigger a browser warning the first time:"
        );
        let _ = writeln!(out, "  choose \"Advanced\" → \"Proceed\". SHA-256: {fp}");
    }
    if let Some(note) = &state.network.tls_note {
        let _ = writeln!(out, "  {note}");
    }
    let _ = writeln!(
        out,
        "\n  LAN TRUST MODE: anyone who can reach this server can read the shared files."
    );
    let _ = writeln!(out, "  Press Ctrl+C to stop.\n");
    out
}
