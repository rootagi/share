//! Offline QR code rendering for the terminal (no network, no image support).
//!
//! Two vertically stacked modules are drawn per character cell using the
//! half-block glyphs `▀ ▄ █`, so the code stays roughly square and compact.
//! Colours are fixed (black on white) instead of following the terminal theme,
//! because phone scanners need dark modules on a light background.

use qrcode::{Color, EcLevel, QrCode};

use crate::error::{Result, ShareError};

/// Quiet-zone width in modules (the QR specification asks for 4).
const QUIET: usize = 4;

/// Render `data` as rows of text (each row is one terminal line, no colour codes).
pub fn render_lines(data: &str) -> Result<Vec<String>> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)
        .map_err(|e| ShareError::Internal(format!("cannot encode QR code: {e}")))?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET;
    let dark = |x: usize, y: usize| -> bool {
        if x < QUIET || y < QUIET || x >= QUIET + width || y >= QUIET + width {
            return false;
        }
        colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
    };
    let mut lines = Vec::with_capacity(size.div_ceil(2));
    for y in (0..size).step_by(2) {
        let mut line = String::with_capacity(size);
        for x in 0..size {
            line.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
    }
    Ok(lines)
}

/// The QR code as a string with ANSI colours (black on white), ready to print.
pub fn render_ansi(data: &str) -> Result<String> {
    let mut out = String::new();
    for line in render_lines(data)? {
        out.push_str("\x1b[30;47m");
        out.push_str(&line);
        out.push_str("\x1b[0m\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_compact_square_code() {
        let lines = render_lines("https://192.168.1.15:8080").unwrap();
        let cols = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == cols));
        // Version 2 (25 modules) + quiet zone = 33 columns, 17 rows of half-blocks.
        assert_eq!(cols, 33);
        assert_eq!(lines.len(), 17);
        assert!(lines.iter().any(|l| l.contains('█')));
        // The quiet zone is blank.
        assert!(lines[0].chars().all(|c| c == ' '));
    }

    #[test]
    fn ansi_output_resets_colours() {
        let s = render_ansi("http://10.0.0.1:8080").unwrap();
        assert!(
            s.lines()
                .all(|l| l.starts_with("\x1b[30;47m") && l.ends_with("\x1b[0m"))
        );
    }
}
