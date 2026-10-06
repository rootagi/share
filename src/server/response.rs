//! Small helpers for building HTTP responses.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use percent_encoding::{AsciiSet, CONTROLS, NON_ALPHANUMERIC, utf8_percent_encode};

/// RFC 5987 `attr-char`: everything except letters, digits and `!#$&+-.^_`|~`.
const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// Characters that must be escaped inside one URL path segment.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Percent-encode one path segment (for building URLs shown to the user).
pub fn encode_path_segment(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_SEGMENT).to_string()
}

/// `Content-Disposition` with an ASCII fallback plus an RFC 6266 `filename*` for Unicode names.
pub fn content_disposition(inline: bool, filename: &str) -> HeaderValue {
    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && !matches!(c, '"' | '\\' | '%') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = utf8_percent_encode(filename, ATTR_CHAR);
    let disposition = if inline { "inline" } else { "attachment" };
    let value = format!("{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}");
    // The value is built exclusively from ASCII, so this cannot fail.
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Headers added to every response.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_has_ascii_fallback_and_utf8_name() {
        let v = content_disposition(false, "日本語 \"quoted\".txt");
        let s = v.to_str().unwrap();
        assert!(s.starts_with("attachment; filename=\""));
        assert!(!s.contains("日"), "header must be ASCII");
        assert!(
            s.contains("filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E%20%22quoted%22.txt"),
            "{s}"
        );
    }

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(encode_path_segment("a b#c?.mkv"), "a%20b%23c%3F.mkv");
        assert_eq!(encode_path_segment("日本"), "%E6%97%A5%E6%9C%AC");
    }
}
