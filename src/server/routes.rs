//! Route table.

use axum::Router;
use axum::middleware;
use axum::routing::{any, get, post};

use super::{archive, auth, dav, download, handlers, response, upload};
use crate::metrics::Shared;

/// Build the application router.
///
/// | Route                     | Purpose                                          |
/// |---------------------------|--------------------------------------------------|
/// | `GET /`, `/browse/{*path}`| browser UI (single page)                         |
/// | `GET /download/{*path}`   | file download (range/resume, `?inline=1`)        |
/// | `GET /archive/{*path}`    | streaming directory archive (`.tar.gz` / `.tar`) |
/// | `ANY /dav/{*path}`        | WebDAV mount endpoint (read/write)               |
/// | `GET /api/files`          | JSON directory listing (sort, filter, pagination)|
/// | `GET /api/status`         | JSON server/transfer status                      |
/// | `POST /api/auth`          | PIN / password login for web UI                  |
/// | `POST/PUT/HEAD /api/upload`| streaming and resumable upload (if enabled)     |
/// | `GET /assets/{name}`      | embedded JS/CSS/icon                             |
pub fn build(state: Shared) -> Router {
    let app = Router::new()
        .route("/", get(handlers::index).fallback(dav::handle_root_slash))
        .route("/browse", get(handlers::index))
        .route("/browse/", get(handlers::index))
        .route("/browse/{*path}", get(handlers::index))
        .route("/download", get(download::download_root))
        .route("/download/", get(download::download_root))
        .route("/download/{*path}", get(download::download_path))
        .route("/archive", get(archive::archive_root))
        .route("/archive/", get(archive::archive_root))
        .route("/archive/{*path}", get(archive::archive_path))
        .route("/dav", any(dav::handle_root))
        .route("/dav/", any(dav::handle_root))
        .route("/dav/{*path}", any(dav::handle_path))
        .route("/api/status", get(handlers::status))
        .route("/api/files", get(handlers::files))
        .route("/api/list", get(handlers::files))
        .route("/api/auth", post(auth::login))
        .route(
            "/api/upload/status",
            get(upload::upload_status).head(upload::upload_status),
        )
        .route(
            "/api/upload",
            post(upload::upload)
                .put(upload::upload)
                .head(upload::upload_status),
        )
        .route("/assets/{name}", get(handlers::asset))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/{*path}", any(dav::handle_path_slash))
        .fallback(handlers::not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let router = if let Some(token) = &state.config.url_token {
        let prefix = format!("/s/{token}");
        let slash_prefix = format!("/s/{token}/");
        Router::new()
            .route(
                &slash_prefix,
                get(handlers::index).fallback(dav::handle_root_slash).layer(
                    middleware::from_fn_with_state(state.clone(), auth::require_auth),
                ),
            )
            .nest(&prefix, app)
            .route("/assets/{name}", get(handlers::asset))
            .route("/favicon.ico", get(handlers::favicon))
            .fallback(handlers::not_found)
    } else {
        app
    };

    router
        .layer(middleware::from_fn(response::security_headers))
        .with_state(state)
}
