//! Handlers for the browser UI, status and file-listing API.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::config::RootKind;
use crate::error::{Result, ShareError};
use crate::fs::browser::{self, Entry, ListOptions, SortKey, SortOrder};
use crate::fs::paths;
use crate::metrics::{Shared, TransferInfo};
use crate::web::assets;

const CSP: &str = "default-src 'self'; img-src 'self' data:; media-src 'self'; style-src 'self'; \
                   script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

/// The single-page browser UI. Served for `/` and every `/browse/...` URL so deep
/// links and the back button work; the page itself asks `/api/files` what to show.
pub async fn index() -> Response {
    let mut res = (StatusCode::OK, assets::INDEX_HTML).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

pub async fn asset(Path(name): Path<String>) -> Result<Response> {
    let asset = assets::find(&name).ok_or(ShareError::NotFound)?;
    let mut res = (StatusCode::OK, asset.body).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    // Assets are compiled into the binary; revalidate cheaply instead of caching blindly.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&format!(
        "\"{}-{}\"",
        env!("CARGO_PKG_VERSION"),
        asset.body.len()
    )) {
        h.insert(header::ETAG, v);
    }
    Ok(res)
}

pub async fn favicon() -> Response {
    let mut res = (StatusCode::OK, assets::FAVICON_SVG).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    res
}

pub async fn not_found() -> ShareError {
    ShareError::NotFound
}

// ---- /api/status --------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatusResponse {
    name: String,
    version: &'static str,
    protocol: &'static str,
    /// `"dir"` or `"file"`.
    kind: &'static str,
    upload_enabled: bool,
    /// True when uploads go to one fixed directory rather than the folder being browsed.
    upload_fixed_dir: bool,
    recursive_search: bool,
    show_hidden: bool,
    uptime_secs: u64,
    active_connections: u64,
    total_connections: u64,
    completed_transfers: u64,
    errors: u64,
    bytes_sent: u64,
    bytes_received: u64,
    download_speed: u64,
    upload_speed: u64,
    peak_download_speed: u64,
    peak_upload_speed: u64,
    transfers: Vec<TransferInfo>,
}

pub async fn status(State(st): State<Shared>) -> Json<StatusResponse> {
    let s = st.metrics.snapshot();
    let cfg = &st.config;
    Json(StatusResponse {
        name: cfg.root.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        protocol: st.protocol(),
        kind: if cfg.root.kind == RootKind::Dir {
            "dir"
        } else {
            "file"
        },
        upload_enabled: st.is_upload_enabled(),
        upload_fixed_dir: cfg.upload.dir.is_some(),
        recursive_search: cfg.recursive_search,
        show_hidden: cfg.show_hidden,
        uptime_secs: s.uptime.as_secs(),
        active_connections: s.active_connections,
        total_connections: s.total_connections,
        completed_transfers: s.completed_transfers,
        errors: s.errors,
        bytes_sent: s.bytes_sent,
        bytes_received: s.bytes_received,
        download_speed: s.download_speed,
        upload_speed: s.upload_speed,
        peak_download_speed: s.peak_download_speed,
        peak_upload_speed: s.peak_upload_speed,
        transfers: s.active,
    })
}

// ---- /api/files ---------------------------------------------------------------------

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct FilesParams {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub sort: SortKey,
    #[serde(default)]
    pub order: SortOrder,
    pub q: Option<String>,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    /// Search sub-directories too (needs `--recursive` on the server).
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Serialize)]
pub struct FilesResponse {
    share_name: String,
    kind: &'static str,
    upload_enabled: bool,
    /// Directory being listed, relative to the share root.
    path: String,
    entries: Vec<Entry>,
    total: usize,
    offset: usize,
    limit: usize,
    truncated: bool,
}

pub async fn files(
    State(st): State<Shared>,
    Query(p): Query<FilesParams>,
) -> Result<Json<FilesResponse>> {
    let cfg = &st.config;
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let (kind, path, listing) = match cfg.root.kind {
        RootKind::File => {
            if !paths::normalize_rel(&p.path).is_empty() {
                return Err(ShareError::NotFound);
            }
            let name = cfg
                .root
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (
                "file",
                String::new(),
                browser::single_file(&cfg.root.path, &name)?,
            )
        }
        RootKind::Dir => {
            let rel = paths::normalize_rel(&p.path);
            let dir = paths::resolve_dir(&cfg.root.path, &rel, cfg.show_hidden).await?;
            let opts = ListOptions {
                sort: p.sort,
                order: p.order,
                query: p.q,
                offset: p.offset,
                limit,
                recursive: p.recursive && cfg.recursive_search,
                show_hidden: cfg.show_hidden,
            };
            (
                "dir",
                rel.clone(),
                browser::list(cfg.root.path.clone(), dir, rel, opts).await?,
            )
        }
    };

    Ok(Json(FilesResponse {
        share_name: cfg.root.name.clone(),
        kind,
        upload_enabled: st.is_upload_enabled(),
        path,
        entries: listing.entries,
        total: listing.total,
        offset: p.offset,
        limit,
        truncated: listing.truncated,
    }))
}
