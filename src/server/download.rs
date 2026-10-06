//! Streaming file downloads with HTTP range (resume) support.
//!
//! Data path for a download:
//!
//! ```text
//! tokio::fs::File ──(take(len), 256 KiB reads)──▶ ReaderStream ──▶ FileStream (metrics)
//!                                                                      │
//!                                         axum Body ◀──────────────────┘
//!                                         hyper (HTTP/1.1 or h2) ──▶ [rustls] ──▶ TCP
//! ```
//!
//! Only one chunk per connection is in flight, so memory use is independent of
//! file size, and hyper's back-pressure paces the disk reads to the client.
//! (The kernel's `sendfile(2)` is not used: it cannot be combined with
//! user-space TLS, and hyper does not expose the socket for plain HTTP.)

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::Stream;
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::Sleep;
use tokio_util::io::ReaderStream;

use super::ClientAddr;
use super::response::content_disposition;
use super::throttle::Throttle;
use crate::config::RootKind;
use crate::error::{Result, ShareError};
use crate::fs::{metadata, paths};
use crate::metrics::{Direction, Shared, TransferGuard};

/// Size of each read from disk. Large enough to amortise the `spawn_blocking`
/// hop tokio uses for file I/O, small enough to keep per-connection memory tiny.
pub const FILE_CHUNK: usize = 256 * 1024;

#[derive(Debug, Default, Deserialize)]
pub struct DownloadQuery {
    /// `?inline=1` asks the browser to display the file instead of saving it
    /// (honoured only for types that are safe to render).
    pub inline: Option<String>,
}

pub async fn download_root(
    State(st): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<DownloadQuery>,
) -> Result<Response> {
    serve(st, "", client, method, headers, q).await
}

pub async fn download_path(
    State(st): State<Shared>,
    Path(rel): Path<String>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<DownloadQuery>,
) -> Result<Response> {
    serve(st, &rel, client, method, headers, q).await
}

pub(crate) async fn serve(
    st: Shared,
    rel: &str,
    client: ClientAddr,
    method: Method,
    headers: HeaderMap,
    q: DownloadQuery,
) -> Result<Response> {
    let root = &st.config.root;
    let path = match root.kind {
        RootKind::Dir => paths::resolve(&root.path, rel, st.config.show_hidden).await?,
        RootKind::File => {
            let file_name = root
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let wanted = rel.trim_matches('/');
            if wanted.is_empty() || wanted == file_name {
                root.path.clone()
            } else {
                return Err(ShareError::NotFound);
            }
        }
    };

    let mut file = File::open(&path).await?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return Err(ShareError::NotFound);
    }
    let size = meta.len();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let etag = metadata::etag(&meta);
    let modified = metadata::last_modified(&meta);

    // Content type and disposition.
    let mime = metadata::mime_for(&filename);
    let wants_inline = matches!(q.inline.as_deref(), Some("1" | "true" | "yes"));
    let inline_type = if wants_inline {
        inline_content_type(&filename, &mime)
    } else {
        None
    };
    let content_type = inline_type.clone().unwrap_or_else(|| mime.to_string());

    let mut builder = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(inline_type.is_some(), &filename),
        )
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "no-cache");
    if let Some(t) = modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(t));
    }

    // Conditional GET.
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if etag_matches(inm, &etag) {
            return builder
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())
                .map_err(internal);
        }
    }

    // Range handling. A failed If-Range precondition means "ignore Range, send everything".
    let range = match headers.get(header::RANGE) {
        Some(v) if if_range_allows(&headers, &etag, modified) => {
            let text = v
                .to_str()
                .map_err(|_| ShareError::InvalidRange("header is not valid text".into()))?;
            parse_range(text, size)?
        }
        _ => None,
    };

    let (status, start, len) = match range {
        Some((s, e)) => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{size}"));
            (StatusCode::PARTIAL_CONTENT, s, e - s + 1)
        }
        None => (StatusCode::OK, 0, size),
    };
    builder = builder.status(status).header(header::CONTENT_LENGTH, len);

    if method == Method::HEAD || len == 0 {
        return builder.body(Body::empty()).map_err(internal);
    }

    if start > 0 {
        file.seek(io::SeekFrom::Start(start)).await?;
    }
    let counts_toward_quota = start == 0 && len == size && !wants_inline;
    let guard = st
        .metrics
        .begin_transfer(client.0, filename, Direction::Download, len)
        .with_quota(counts_toward_quota);
    let stream = FileStream {
        inner: ReaderStream::with_capacity(file.take(len), FILE_CHUNK),
        guard: Some(guard),
        throttle: st.throttle.clone(),
        sleep: None,
        pending: None,
        expected: len,
        sent: 0,
    };
    builder.body(Body::from_stream(stream)).map_err(internal)
}

fn internal(e: axum::http::Error) -> ShareError {
    ShareError::Internal(format!("building response: {e}"))
}

/// The content type to use when displaying a file inline, or `None` if the
/// file must be downloaded as an attachment (HTML, SVG and other active content).
fn inline_content_type(filename: &str, mime: &mime_guess::Mime) -> Option<String> {
    match (mime.type_().as_str(), mime.subtype().as_str()) {
        ("image", "svg" | "svg+xml") => None,
        ("image" | "video" | "audio", _) => Some(mime.essence_str().to_string()),
        ("application", "pdf") => Some("application/pdf".into()),
        // Text and source code are shown as plain text so they can never run as markup or script.
        ("text", _) => Some("text/plain; charset=utf-8".into()),
        _ if matches!(metadata::kind_for(filename, false), "text" | "code") => {
            Some("text/plain; charset=utf-8".into())
        }
        _ => None,
    }
}

// ---- conditional requests --------------------------------------------------------------

/// `If-None-Match` uses weak comparison: strip `W/` and compare opaque tags.
fn etag_matches(header_value: &str, etag: &str) -> bool {
    header_value
        .split(',')
        .map(|t| t.trim().trim_start_matches("W/"))
        .any(|t| t == "*" || t == etag)
}

/// `If-Range` (RFC 9110 §13.1.5): the range applies only if the validator still matches.
fn if_range_allows(headers: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    let Some(v) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let v = v.trim();
    if v.starts_with('"') {
        return v == etag; // strong comparison
    }
    match (httpdate::parse_http_date(v), modified) {
        (Ok(date), Some(m)) => date == m,
        _ => false,
    }
}

// ---- range parsing ---------------------------------------------------------------------

/// Parse a `Range` header for a representation of `size` bytes.
///
/// * `Ok(None)` – no usable range: other units, or multiple ranges (which RFC 9110
///   lets a server ignore). The full file is sent with `200`.
/// * `Ok(Some((first, last)))` – inclusive byte positions, already clamped to the file.
/// * `Err(RangeNotSatisfiable)` – syntactically valid but outside the file (`416`).
/// * `Err(InvalidRange)` – malformed (`400`).
///
/// All positions are `u64`, so files larger than 4 GiB need no special casing.
pub fn parse_range(value: &str, size: u64) -> Result<Option<(u64, u64)>> {
    let value = value.trim();
    let Some((unit, spec)) = value.split_once('=') else {
        return Err(ShareError::InvalidRange("missing '='".into()));
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Ok(None);
    }
    if spec.contains(',') {
        return Ok(None);
    }
    let (first, last) = spec
        .split_once('-')
        .map(|(a, b)| (a.trim(), b.trim()))
        .ok_or_else(|| ShareError::InvalidRange("missing '-'".into()))?;

    match (first.is_empty(), last.is_empty()) {
        (true, true) => Err(ShareError::InvalidRange("empty range".into())),
        // "-N": the final N bytes.
        (true, false) => {
            let n = parse_position(last)?;
            if n == 0 || size == 0 {
                return Err(ShareError::RangeNotSatisfiable { size });
            }
            let n = n.min(size);
            Ok(Some((size - n, size - 1)))
        }
        // "N-" or "N-M"
        (false, _) => {
            let start = parse_position(first)?;
            let end = if last.is_empty() {
                None
            } else {
                Some(parse_position(last)?)
            };
            if end.is_some_and(|e| e < start) {
                return Err(ShareError::InvalidRange(
                    "last position is before first position".into(),
                ));
            }
            if start >= size {
                return Err(ShareError::RangeNotSatisfiable { size });
            }
            let end = end.map_or(size - 1, |e| e.min(size - 1));
            Ok(Some((start, end)))
        }
    }
}

/// Digits only; values beyond `u64::MAX` saturate (they are then clamped to the file size).
fn parse_position(s: &str) -> Result<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ShareError::InvalidRange(format!(
            "'{s}' is not a byte position"
        )));
    }
    Ok(s.parse::<u64>().unwrap_or(u64::MAX))
}

// ---- body stream -----------------------------------------------------------------------

/// Wraps the file reader, counting bytes and settling the [`TransferGuard`].
struct FileStream {
    inner: ReaderStream<tokio::io::Take<File>>,
    guard: Option<TransferGuard>,
    throttle: Option<Arc<Throttle>>,
    sleep: Option<Pin<Box<Sleep>>>,
    pending: Option<Bytes>,
    expected: u64,
    sent: u64,
}

impl Stream for FileStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this
            .guard
            .as_ref()
            .is_some_and(|g| g.transfer().cancel.is_cancelled())
        {
            let _ = this.guard.take(); // Drop records Aborted
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transfer cancelled by operator",
            ))));
        }

        if let Some(sleep) = &mut this.sleep {
            match sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.sleep = None;
                    if let Some(chunk) = this.pending.take() {
                        let n = chunk.len() as u64;
                        this.sent += n;
                        if let Some(g) = &this.guard {
                            g.add(n);
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let n = chunk.len() as u64;
                if let Some(delay) = this.throttle.as_ref().and_then(|t| t.acquire_delay(n)) {
                    let mut s = Box::pin(tokio::time::sleep(delay));
                    if s.as_mut().poll(cx).is_pending() {
                        this.sleep = Some(s);
                        this.pending = Some(chunk);
                        return Poll::Pending;
                    }
                }
                this.sent += n;
                if let Some(g) = &this.guard {
                    g.add(n);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                if let Some(g) = this.guard.take() {
                    g.fail(&e.to_string());
                }
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) if this.sent < this.expected => {
                // The file shrank after we promised a Content-Length. Surface an error so
                // hyper aborts the connection instead of silently truncating the download.
                if let Some(g) = this.guard.take() {
                    g.fail("file changed while it was being sent");
                }
                Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file was truncated during transfer",
                ))))
            }
            Poll::Ready(None) => {
                if let Some(g) = this.guard.take() {
                    g.complete();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for FileStream {
    /// hyper stops polling as soon as `Content-Length` bytes were written, so the
    /// stream may be dropped without ever yielding `None`. If everything was handed to
    /// hyper, count the transfer as complete; otherwise the guard records an abort.
    fn drop(&mut self) {
        if let Some(g) = self.guard.take() {
            if self.sent >= self.expected {
                g.complete();
            }
            // else: dropping the guard records the transfer as aborted.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &str, size: u64) -> Result<Option<(u64, u64)>> {
        parse_range(v, size)
    }

    #[test]
    fn closed_and_open_ranges() {
        assert_eq!(r("bytes=0-99", 1000).unwrap(), Some((0, 99)));
        assert_eq!(r("bytes=500-", 1000).unwrap(), Some((500, 999)));
        assert_eq!(r("bytes=0-0", 1000).unwrap(), Some((0, 0)));
        assert_eq!(r("bytes=999-999", 1000).unwrap(), Some((999, 999)));
        assert_eq!(r(" Bytes = 1 - 2 ", 10).unwrap(), Some((1, 2)));
    }

    #[test]
    fn end_is_clamped_to_file_size() {
        assert_eq!(r("bytes=900-5000", 1000).unwrap(), Some((900, 999)));
        assert_eq!(
            r("bytes=0-18446744073709551615", 1000).unwrap(),
            Some((0, 999))
        );
        assert_eq!(
            r("bytes=0-99999999999999999999999", 1000).unwrap(),
            Some((0, 999))
        );
    }

    #[test]
    fn suffix_ranges() {
        assert_eq!(r("bytes=-100", 1000).unwrap(), Some((900, 999)));
        assert_eq!(r("bytes=-5000", 1000).unwrap(), Some((0, 999)));
        assert!(matches!(
            r("bytes=-0", 1000),
            Err(ShareError::RangeNotSatisfiable { size: 1000 })
        ));
    }

    #[test]
    fn unsatisfiable_ranges_are_416() {
        assert!(matches!(
            r("bytes=1000-", 1000),
            Err(ShareError::RangeNotSatisfiable { .. })
        ));
        assert!(matches!(
            r("bytes=2000-3000", 1000),
            Err(ShareError::RangeNotSatisfiable { .. })
        ));
        assert!(matches!(
            r("bytes=0-1", 0),
            Err(ShareError::RangeNotSatisfiable { size: 0 })
        ));
        assert!(matches!(
            r("bytes=-1", 0),
            Err(ShareError::RangeNotSatisfiable { .. })
        ));
    }

    #[test]
    fn malformed_ranges_are_400() {
        for bad in [
            "bytes",
            "bytes=",
            "bytes=-",
            "bytes=abc-",
            "bytes=1-x",
            "bytes=5-2",
            "bytes=1_000-",
            "bytes=+1-2",
            "bytes=1",
        ] {
            assert!(
                matches!(r(bad, 1000), Err(ShareError::InvalidRange(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn unknown_units_and_multi_ranges_fall_back_to_full_response() {
        assert_eq!(r("items=0-5", 1000).unwrap(), None);
        assert_eq!(r("bytes=0-5,10-15", 1000).unwrap(), None);
    }

    #[test]
    fn offsets_beyond_4_gib_work() {
        let size = 100 * 1024 * 1024 * 1024u64; // 100 GiB
        let start = 5_000_000_000u64;
        assert_eq!(
            r(&format!("bytes={start}-"), size).unwrap(),
            Some((start, size - 1))
        );
        assert_eq!(
            r("bytes=-4294967296", size).unwrap(),
            Some((size - 4_294_967_296, size - 1))
        );
    }

    #[test]
    fn etag_matching_is_weak_and_supports_lists() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(etag_matches("W/\"abc\"", "\"abc\""));
        assert!(etag_matches("\"x\", \"abc\"", "\"abc\""));
        assert!(etag_matches("*", "\"abc\""));
        assert!(!etag_matches("\"other\"", "\"abc\""));
    }

    #[test]
    fn if_range_semantics() {
        let mut h = HeaderMap::new();
        assert!(
            if_range_allows(&h, "\"e\"", None),
            "absent If-Range allows the range"
        );
        h.insert(header::IF_RANGE, "\"e\"".parse().unwrap());
        assert!(if_range_allows(&h, "\"e\"", None));
        assert!(!if_range_allows(&h, "\"changed\"", None));
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        h.insert(
            header::IF_RANGE,
            httpdate::fmt_http_date(t).parse().unwrap(),
        );
        assert!(if_range_allows(&h, "\"e\"", Some(t)));
        assert!(!if_range_allows(
            &h,
            "\"e\"",
            Some(t + std::time::Duration::from_secs(5))
        ));
        h.insert(header::IF_RANGE, "W/\"e\"".parse().unwrap());
        assert!(
            !if_range_allows(&h, "\"e\"", None),
            "weak validators never match If-Range"
        );
    }

    #[test]
    fn inline_only_for_safe_types() {
        let ic = |n: &str| inline_content_type(n, &metadata::mime_for(n));
        assert!(ic("a.png").is_some());
        assert!(ic("a.mp4").is_some());
        assert!(ic("a.pdf").is_some());
        assert_eq!(ic("a.html").as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(ic("a.json").as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(ic("a.rs").as_deref(), Some("text/plain; charset=utf-8"));
        assert!(ic("a.svg").is_none());
        assert!(ic("a.zip").is_none());
    }
}
