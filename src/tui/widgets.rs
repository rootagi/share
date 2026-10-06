//! Reusable drawing helpers.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tracing::Level;

pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERR: Color = Color::Red;
pub const DIM: Color = Color::DarkGray;

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn label() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// A fixed-width progress bar: `█████░░░░░`.
pub fn progress_bar(ratio: f64, width: usize, fill: Color) -> Vec<Span<'static>> {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(fill)),
        Span::styled("░".repeat(width - filled), dim()),
    ]
}

/// A rectangle of at most `w`×`h` centred in `area`.
pub fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub fn level_style(level: Level) -> Style {
    match level {
        Level::ERROR => Style::new().fg(ERR).add_modifier(Modifier::BOLD),
        Level::WARN => Style::new().fg(WARN),
        Level::INFO => Style::new(),
        _ => dim(),
    }
}

/// `AB:CD:EF:01:…` – enough to eyeball against the browser's certificate dialog.
pub fn short_fingerprint(fp: &str) -> String {
    let parts: Vec<&str> = fp.split(':').take(8).collect();
    format!("{}…", parts.join(":"))
}

/// `HH:MM:SS` since start-up, for log lines.
pub fn clock(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_width_is_constant() {
        for r in [0.0, 0.01, 0.5, 0.999, 1.0, 7.0, -1.0] {
            let total: usize = progress_bar(r, 20, OK)
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(total, 20);
        }
    }

    #[test]
    fn centering_never_overflows() {
        let r = centered(100, 100, Rect::new(0, 0, 40, 10));
        assert_eq!((r.width, r.height), (40, 10));
        let r = centered(10, 4, Rect::new(0, 0, 40, 10));
        assert_eq!((r.x, r.y), (15, 3));
    }
}
