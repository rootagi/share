//! Streaming uploads with atomic commit and resumable `.part` retention.
//!
//! The request body is the raw file (no multipart framing), which lets the
//! browser use `XMLHttpRequest`/`fetch` with a `File` directly, gives real
//! progress events, and keeps the server path trivially streaming:
//!
//! ```text
//! POST /api/upload?name=report.pdf&dir=docs      (X-Share-Upload: 1)
//!   body ──▶ BufWriter (1 MiB) ──▶ docs/.share-upload-<pid>-<id>.part
//!                                       │  complete & length verified
//!                                       ▼
//!                      reserve final name (create_new) ─▶ rename() over it
//! ```
//!
//! * Nothing is buffered beyond one write buffer; file size is unlimited.
//! * Data goes to a hidden `.part` file in the destination directory (same
//!   filesystem, so the final `rename` is atomic). The `.part` file is never
//!   listed or served.
//! * If the client opts into resumable uploads (via `X-Share-Offset` or `mtime`),
//!   interrupted `.part` files are retained in [`PartialRegistry`] for up to 15
//!   minutes so a dropped Wi-Fi connection can resume from the last written byte.
//! * Existing files are never overwritten: `name (1).ext`, `name (2).ext`, ...

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};

use super::ClientAddr;
use super::throttle::Throttle;
use crate::error::{Result, ShareError};
use crate::fs::paths::{self, TEMP_PREFIX, TEMP_SUFFIX};
use crate::metrics::{Direction, Shared};

const WRITE_BUFFER: usize = 1024 * 1024;
/// How long an interrupted resumable `.part` file is kept before automatic cleanup.
const PARTIAL_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Deserialize)]
pub struct UploadParams {
    /// File name (a bare name, no directories).
    pub name: String,
    /// Destination directory relative to the share root (ignored with `--upload-dir` unless `mkdir` is true).
    #[serde(default)]
    pub dir: String,
    /// Create missing subdirectories under the target root (used by recursive folder drag-and-drop).
    #[serde(default)]
    pub mkdir: bool,
    /// Total file size in bytes (optional metadata for resumable uploads).
    pub size: Option<u64>,
    /// Last-modified timestamp in ms (optional metadata for resumable uploads).
    pub mtime: Option<u64>,
    /// Byte offset to resume an interrupted upload from (`POST /api/upload?offset=<N>`).
    pub offset: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    /// Final name on disk (may differ from the requested one after a collision).
    pub name: String,
    pub size: u64,
    pub renamed: bool,
}

/// Deletes the temporary file unless disarmed. Runs on error, on client
/// disconnect (when not retained for resume) and on server shutdown.
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PartialKey {
    dir: PathBuf,
    name: String,
    size: u64,
    mtime: u64,
}

#[derive(Debug)]
struct PartialEntry {
    tmp: TempFile,
    updated: Instant,
}

/// Tracks interrupted `.part` files keyed by `(dir, name, size, mtime)` for a short TTL.
/// Dropping the registry at server shutdown automatically removes any leftover `.part` files.
#[derive(Debug, Default)]
pub struct PartialRegistry {
    inner: Mutex<HashMap<PartialKey, PartialEntry>>,
}

impl PartialRegistry {
    pub fn evict_expired(&self) {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, entry| entry.updated.elapsed() < PARTIAL_TTL);
    }

    fn take(&self, key: &PartialKey) -> Option<TempFile> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, entry| entry.updated.elapsed() < PARTIAL_TTL);
        if let Some(entry) = map.remove(key) {
            return Some(entry.tmp);
        }
        // Fallback: if size or mtime was omitted on either the initial or resume request, match by (dir, name).
        let found_key = map
            .keys()
            .find(|k| {
                k.dir == key.dir
                    && k.name == key.name
                    && (key.size == 0 || k.size == 0 || k.size == key.size)
            })
            .cloned();
        if let Some(k) = found_key {
            return map.remove(&k).map(|e| e.tmp);
        }
        None
    }

    fn peek_offset(&self, key: &PartialKey) -> u64 {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, entry| entry.updated.elapsed() < PARTIAL_TTL);
        let entry = map.get(key).or_else(|| {
            map.iter()
                .find(|(k, _)| {
                    k.dir == key.dir
                        && k.name == key.name
                        && (key.size == 0 || k.size == 0 || k.size == key.size)
                })
                .map(|(_, v)| v)
        });
        entry
            .and_then(|e| std::fs::metadata(e.tmp.path()).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn park(&self, key: PartialKey, tmp: TempFile) {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, entry| entry.updated.elapsed() < PARTIAL_TTL);
        map.insert(
            key,
            PartialEntry {
                tmp,
                updated: Instant::now(),
            },
        );
    }
}

pub(crate) enum Failure {
    /// The client stopped sending: not a server error.
    ClientGone(String),
    Server(ShareError),
}

async fn resolve_upload_target_dir(st: &Shared, params: &UploadParams) -> Result<PathBuf> {
    match &st.config.upload.dir {
        Some(fixed) => {
            if params.mkdir && !params.dir.is_empty() {
                paths::resolve_or_create_dir(fixed, &params.dir, st.config.show_hidden).await
            } else {
                Ok(fixed.clone())
            }
        }
        None => {
            if params.mkdir {
                paths::resolve_or_create_dir(
                    &st.config.root.path,
                    &params.dir,
                    st.config.show_hidden,
                )
                .await
            } else {
                paths::resolve_dir(&st.config.root.path, &params.dir, st.config.show_hidden).await
            }
        }
    }
}

/// `GET /api/upload/status` and `HEAD /api/upload` query the byte offset of an interrupted resumable upload.
pub async fn upload_status(
    State(st): State<Shared>,
    method: Method,
    Query(params): Query<UploadParams>,
) -> Result<Response> {
    if !st.is_upload_enabled() {
        return Err(ShareError::UploadsDisabled);
    }
    let name = paths::sanitize_upload_name(&params.name)?;
    let dir = resolve_upload_target_dir(&st, &params).await?;
    let key = PartialKey {
        dir,
        name,
        size: params.size.unwrap_or(0),
        mtime: params.mtime.unwrap_or(0),
    };
    let offset = st.partial_uploads.peek_offset(&key);
    if method == Method::HEAD {
        Ok((
            StatusCode::OK,
            [("upload-offset", offset.to_string())],
            Body::empty(),
        )
            .into_response())
    } else {
        Ok((
            StatusCode::OK,
            [("upload-offset", offset.to_string())],
            Json(serde_json::json!({ "offset": offset })),
        )
            .into_response())
    }
}

pub async fn upload(
    State(st): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<UploadResponse>)> {
    if !st.is_upload_enabled() {
        return Err(ShareError::UploadsDisabled);
    }
    // Cross-site `POST` in a browser can be sent as a CORS "simple request" without preflight,
    // so `POST` requires `X-Share-Upload: 1`. `PUT` (e.g. `curl -T file.bin`) is never a CORS
    // simple method and always triggers a browser preflight, so CLI `PUT` works directly.
    let is_cross_site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("cross-site"));
    if is_cross_site || (method != Method::PUT && !headers.contains_key("x-share-upload")) {
        return Err(ShareError::Forbidden(
            "missing X-Share-Upload header".into(),
        ));
    }
    let name = paths::sanitize_upload_name(&params.name)?;
    let dir = resolve_upload_target_dir(&st, &params).await?;

    let offset_header = headers
        .get("x-share-offset")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.trim()
                .parse::<u64>()
                .map_err(|_| ShareError::BadRequest("invalid X-Share-Offset header".into()))
        })
        .transpose()?;
    let offset = offset_header.or(params.offset).unwrap_or(0);
    let resumable = offset_header.is_some()
        || params.offset.is_some()
        || params.mtime.is_some()
        || params.size.is_some();

    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let total_expected = params
        .size
        .or_else(|| content_length.map(|cl| offset + cl))
        .unwrap_or(0);

    let key = PartialKey {
        dir: dir.clone(),
        name: name.clone(),
        size: params.size.unwrap_or(0),
        mtime: params.mtime.unwrap_or(0),
    };

    let guard =
        st.metrics
            .begin_transfer(client.0, name.clone(), Direction::Upload, total_expected);

    let (mut tmp, file) = if offset > 0 {
        let Some(existing_tmp) = st.partial_uploads.take(&key) else {
            let err = ShareError::BadRequest(format!(
                "no partial upload found to resume at offset {offset}"
            ));
            guard.fail(&err.to_string());
            return Err(err);
        };
        let existing_size = tokio::fs::metadata(existing_tmp.path())
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if existing_size != offset {
            st.partial_uploads.park(key, existing_tmp);
            let err = ShareError::BadRequest(format!(
                "offset {offset} does not match partial file size {existing_size}"
            ));
            guard.fail(&err.to_string());
            return Err(err);
        }
        let f = match OpenOptions::new()
            .write(true)
            .append(true)
            .open(existing_tmp.path())
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let err = ShareError::from(e);
                guard.fail(&err.to_string());
                return Err(err);
            }
        };
        guard.add(offset);
        (existing_tmp, f)
    } else {
        // Starting from 0: discard any stale partial for the same key.
        let _ = st.partial_uploads.take(&key);
        let tmp_path = dir.join(format!(
            "{TEMP_PREFIX}{}-{}{TEMP_SUFFIX}",
            std::process::id(),
            guard.transfer().id
        ));
        let mut tmp = TempFile {
            path: tmp_path.clone(),
            armed: false,
        };
        let f = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let err = ShareError::from(e);
                guard.fail(&err.to_string());
                return Err(err);
            }
        };
        tmp.armed = true;
        (tmp, f)
    };

    match receive(body, file, &guard, content_length, st.throttle.clone()).await {
        Ok(received) => {
            let final_size = offset + received;
            match commit(&dir, tmp.path(), &name).await {
                Ok(final_name) => {
                    tmp.disarm(); // the temp file no longer exists: it was renamed
                    guard.complete();
                    let renamed = final_name != name;
                    if renamed {
                        tracing::info!("upload saved as '{final_name}' ('{name}' already existed)");
                    }
                    Ok((
                        StatusCode::CREATED,
                        Json(UploadResponse {
                            name: final_name,
                            size: final_size,
                            renamed,
                        }),
                    ))
                }
                Err(e) => {
                    guard.fail(&e.to_string());
                    Err(e)
                }
            }
        }
        Err(Failure::ClientGone(why)) => {
            tracing::debug!("upload of '{name}' interrupted: {why}");
            if resumable && !guard.transfer().cancel.is_cancelled() {
                if let Ok(m) = tokio::fs::metadata(tmp.path()).await {
                    if m.len() > 0 {
                        st.partial_uploads.park(key, tmp);
                    }
                }
            }
            drop(guard); // recorded as aborted
            Err(ShareError::BadRequest(format!("upload interrupted: {why}")))
        }
        Err(Failure::Server(e)) => {
            guard.fail(&e.to_string());
            Err(e)
        }
    }
}

pub(crate) async fn receive(
    body: Body,
    file: tokio::fs::File,
    guard: &crate::metrics::TransferGuard,
    expected: Option<u64>,
    throttle: Option<Arc<Throttle>>,
) -> std::result::Result<u64, Failure> {
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER, file);
    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    let cancel = guard.transfer().cancel.clone();

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = writer.flush().await;
                return Err(Failure::ClientGone("cancelled by operator".into()));
            }
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = writer.flush().await;
                return Err(Failure::ClientGone(e.to_string()));
            }
        };
        let n = chunk.len() as u64;
        if let Some(t) = &throttle {
            t.acquire(n).await;
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| Failure::Server(ShareError::from(e)))?;
        received += n;
        guard.add(n);
    }
    writer
        .flush()
        .await
        .map_err(|e| Failure::Server(ShareError::from(e)))?;
    if let Some(expected) = expected {
        if received != expected {
            return Err(Failure::ClientGone(format!(
                "received {received} of {expected} bytes"
            )));
        }
    }
    Ok(received)
}

/// Move the finished temp file to its final name without ever overwriting a file.
///
/// The destination name is *reserved* with `create_new` (atomic, works on every
/// filesystem including FAT/exFAT where hard links do not), then the temp file is
/// renamed over the empty reservation.
async fn commit(dir: &std::path::Path, tmp: &std::path::Path, name: &str) -> Result<String> {
    for candidate in paths::collision_candidates(name) {
        let dest = dir.join(&candidate);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .await
        {
            Ok(reserved) => {
                drop(reserved);
                if let Err(e) = tokio::fs::rename(tmp, &dest).await {
                    let _ = tokio::fs::remove_file(&dest).await;
                    return Err(e.into());
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ShareError::Internal(
        "too many files with the same name".into(),
    ))
}
