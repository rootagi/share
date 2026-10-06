//! HTTP Basic Authentication and Web UI PIN/password session middleware (`--auth` / `--pin`).

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::Deserialize;

use crate::config::AuthConfig;
use crate::metrics::Shared;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub username: String,
    #[serde(alias = "pin")]
    pub password: String,
}

/// `POST /api/auth`: verifies PIN/password from the web UI modal and sets an `HttpOnly` session cookie.
pub async fn login(State(st): State<Shared>, Json(req): Json<LoginRequest>) -> Response {
    let Some(auth) = &st.config.auth else {
        return (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
    };

    if auth.verify(&req.username, &req.password) {
        let cookie = format!(
            "share_auth={}; Path=/; HttpOnly; SameSite=Strict",
            auth.session_token
        );
        let mut res = (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            res.headers_mut().insert(header::SET_COOKIE, v);
        }
        res
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid credentials",
                "status": 401
            })),
        )
            .into_response()
    }
}

/// Middleware enforcing `--auth` / `--pin`.
pub async fn require_auth(State(st): State<Shared>, req: Request, next: Next) -> Response {
    let Some(auth) = &st.config.auth else {
        return next.run(req).await;
    };

    if is_authenticated(req.headers(), auth) {
        return next.run(req).await;
    }

    let path = strip_token_prefix(req.uri().path(), st.config.url_token.as_deref());
    if path.starts_with("/assets/") || path == "/favicon.ico" || path == "/api/auth" {
        return next.run(req).await;
    }

    // Let browsers load the static SPA HTML shell (which contains no file data) so the
    // in-page PIN/password modal can render without triggering a native Basic Auth dialog,
    // while CLI clients (`curl`, `wget`) receive `401 Unauthorized` directly.
    let is_cli_ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| {
            let ua_low = ua.to_ascii_lowercase();
            ua_low.starts_with("curl/") || ua_low.starts_with("wget/")
        });
    if !is_cli_ua
        && *req.method() == Method::GET
        && (path == "/" || path == "/browse" || path.starts_with("/browse/"))
    {
        return next.run(req).await;
    }

    let auth_mode = if auth.username.is_some() {
        "user_pass"
    } else {
        "pin"
    };
    let body = serde_json::json!({
        "error": "authentication required",
        "auth_mode": auth_mode,
        "status": 401
    });
    let mut res = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    // Omit WWW-Authenticate for XHR/fetch requests from the web UI so the browser does not
    // pop up its native modal over the custom PIN modal.
    if !req.headers().contains_key("x-requested-with") {
        res.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"share\""),
        );
    }
    res
}

fn strip_token_prefix<'a>(path: &'a str, token: Option<&str>) -> &'a str {
    if let Some(t) = token {
        let prefix = format!("/s/{t}");
        if let Some(rest) = path.strip_prefix(&prefix) {
            if rest.is_empty() {
                return "/";
            }
            if rest.starts_with('/') {
                return rest;
            }
        }
    }
    path
}

fn is_authenticated(headers: &HeaderMap, auth: &AuthConfig) -> bool {
    if let Some(val) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(b64) = val
            .strip_prefix("Basic ")
            .or_else(|| val.strip_prefix("basic "))
        {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                if let Ok(decoded) = String::from_utf8(bytes) {
                    if auth.verify_raw(&decoded) {
                        return true;
                    }
                }
            }
        }
    }

    for cookie_hdr in headers.get_all(header::COOKIE) {
        let Ok(cookie_str) = cookie_hdr.to_str() else {
            continue;
        };
        for pair in cookie_str.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=') {
                if k.trim() == "share_auth" && auth.verify_session(v.trim()) {
                    return true;
                }
            }
        }
    }

    false
}
