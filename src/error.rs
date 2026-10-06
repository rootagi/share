//! Central error type for `share`.
//!
//! One enum covers both start-up failures (bad flags, port in use, invalid
//! certificate) and request-time failures (404, 416, ...). Request-time
//! variants implement [`IntoResponse`], so handlers can simply use `?`.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// Convenience alias used throughout the crate.
pub type Result<T, E = ShareError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum ShareError {
    // ---- start-up / configuration -------------------------------------------------
    #[error("{0}")]
    Config(String),
    #[error("'{}' does not exist", .0.display())]
    PathNotFound(PathBuf),
    #[error("permission denied: '{}'", .0.display())]
    PermissionDenied(PathBuf),
    #[error("port {} is already in use on {}", .0.port(), .0.ip())]
    AddrInUse(SocketAddr),
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: io::Error },
    #[error("network interface '{0}' was not found or has no usable IPv4 address")]
    InterfaceUnavailable(String),
    #[error("invalid TLS certificate: {0}")]
    InvalidCertificate(String),
    #[error("invalid TLS private key: {0}")]
    InvalidKey(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },

    // ---- request time -------------------------------------------------------------
    #[error("not found")]
    NotFound,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("invalid Range header: {0}")]
    InvalidRange(String),
    #[error("requested range not satisfiable")]
    RangeNotSatisfiable { size: u64 },
    #[error("uploads are disabled on this server")]
    UploadsDisabled,
    #[error("the server ran out of disk space")]
    DiskFull,
    #[error("upload incomplete: received {received} of {expected} bytes")]
    UploadIncomplete { received: u64, expected: u64 },
    #[error("internal error: {0}")]
    Internal(String),
}

impl ShareError {
    /// Wrap an I/O error with a human-readable context (start-up code).
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        ShareError::Io {
            context: context.into(),
            source,
        }
    }

    /// HTTP status that corresponds to this error.
    pub fn status(&self) -> StatusCode {
        match self {
            ShareError::NotFound | ShareError::PathNotFound(_) => StatusCode::NOT_FOUND,
            ShareError::Forbidden(_) | ShareError::PermissionDenied(_) => StatusCode::FORBIDDEN,
            ShareError::UploadsDisabled => StatusCode::FORBIDDEN,
            ShareError::BadRequest(_)
            | ShareError::InvalidRange(_)
            | ShareError::UploadIncomplete { .. } => StatusCode::BAD_REQUEST,
            ShareError::RangeNotSatisfiable { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
            ShareError::DiskFull => StatusCode::INSUFFICIENT_STORAGE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// A short, actionable hint printed below start-up errors.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            ShareError::AddrInUse(_) => {
                Some("choose another port with --port, or stop the other program")
            }
            ShareError::Bind { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
                Some("ports below 1024 need elevated privileges; try --port 8080")
            }
            ShareError::Bind { .. } => {
                Some("check that --bind/--interface is an address of this machine")
            }
            ShareError::InterfaceUnavailable(_) => {
                Some("run `ip -br addr` to list interface names, or use --bind <ADDRESS>")
            }
            ShareError::InvalidCertificate(_) | ShareError::InvalidKey(_) => {
                Some("expected PEM files; omit --cert/--key to use an auto-generated certificate")
            }
            ShareError::PermissionDenied(_) => {
                Some("check the file permissions of the shared path")
            }
            _ => None,
        }
    }
}

/// Request-time mapping of raw I/O errors. Start-up code uses [`ShareError::io`]
/// instead so the message carries context.
impl From<io::Error> for ShareError {
    fn from(err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => ShareError::NotFound,
            io::ErrorKind::PermissionDenied => ShareError::Forbidden("permission denied".into()),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => ShareError::DiskFull,
            io::ErrorKind::IsADirectory => ShareError::BadRequest("path is a directory".into()),
            _ => ShareError::Internal(err.to_string()),
        }
    }
}

impl IntoResponse for ShareError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Never leak internal details to clients; they are logged instead.
        let message = match &self {
            ShareError::Internal(detail)
            | ShareError::Io {
                context: detail, ..
            } => {
                tracing::error!("internal error: {detail}");
                "internal server error".to_string()
            }
            other => other.to_string(),
        };
        let body = serde_json::json!({ "error": message, "status": status.as_u16() });
        let mut response = (status, Json(body)).into_response();
        if let ShareError::RangeNotSatisfiable { size } = self {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, v);
            }
        }
        response
    }
}
