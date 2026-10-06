//! Small formatting helpers shared by the TUI, logs and CLI output.

use std::time::Duration;

/// `1536` → `"1.5 KiB"`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Bytes per second as `"42.2 MB/s"` (decimal megabytes, matching how networks are rated).
pub fn format_speed(bytes_per_sec: u64) -> String {
    let v = bytes_per_sec as f64;
    if v >= 1e9 {
        format!("{:.2} GB/s", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1} MB/s", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1} kB/s", v / 1e3)
    } else {
        format!("{bytes_per_sec} B/s")
    }
}

/// `3725s` → `"1h 02m 05s"`, `65s` → `"1m 05s"`, `7s` → `"7s"`.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// Shorten `s` to at most `max` characters, keeping both ends: `movie…final.mkv`.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let tail = keep / 2;
    let head = keep - tail;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
        assert_eq!(format_bytes(u64::MAX), "16384 PiB");
    }

    #[test]
    fn speeds() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(42_200_000), "42.2 MB/s");
        assert_eq!(format_speed(1_250_000_000), "1.25 GB/s");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(7)), "7s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m 05s");
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate_middle("short.txt", 20), "short.txt");
        let t = truncate_middle("日本語のとても長いファイル名.mkv", 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.contains('…'));
    }
}
