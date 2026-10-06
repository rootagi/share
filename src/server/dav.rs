//! Full read-write WebDAV endpoint (`/dav`) for native OS file-manager mounting.
//!
//! Supports macOS Finder, Windows Explorer, Linux (`davfs2` / GNOME Files `gvfs` /
//! KDE Dolphin), and iOS Files:
//! * Read operations (`OPTIONS`, `PROPFIND`, `GET`, `HEAD`) are always available.
//! * Write operations (`PUT`, `MKCOL`, `DELETE`, `MOVE`, `COPY`, `PROPPATCH`,
//!   `LOCK`, `UNLOCK`) require uploads to be enabled (`--upload` or live TUI toggle `u`).

use std::fmt::Write as _;
use std::fs::{self, Metadata};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use tokio::fs::OpenOptions;

use super::ClientAddr;
use super::download::{self, DownloadQuery};
use super::response::encode_path_segment;
use super::upload::{self, TempFile};
use crate::config::RootKind;
use crate::error::{Result, ShareError};
use crate::fs::metadata;
use crate::fs::paths::{self, TEMP_PREFIX, TEMP_SUFFIX, is_temp_name};
use crate::metrics::{Direction, Shared};

const MAX_INFINITY_DEPTH: usize = 8;
const MAX_PROPFIND_ENTRIES: usize = 2_000;
static LOCK_SEQ: AtomicU64 = AtomicU64::new(1);

pub async fn handle_root(
    State(st): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    dispatch(st, "", true, client, method, headers, body).await
}

pub async fn handle_path(
    State(st): State<Shared>,
    AxumPath(rel): AxumPath<String>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    dispatch(st, &rel, true, client, method, headers, body).await
}

/// WebDAV handler for clients that mount the root URL directly (e.g. `dav://host:8080`).
pub async fn handle_root_slash(
    State(st): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    dispatch(st, "", false, client, method, headers, body).await
}

/// Fallback WebDAV handler for `/{*path}` when mounted at the server root.
pub async fn handle_path_slash(
    State(st): State<Shared>,
    AxumPath(rel): AxumPath<String>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    if rel == "api" || rel.starts_with("api/") {
        return Err(ShareError::NotFound);
    }
    dispatch(st, &rel, false, client, method, headers, body).await
}

async fn dispatch(
    st: Shared,
    rel: &str,
    mounted_at_dav: bool,
    client: ClientAddr,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    match method.as_str() {
        "OPTIONS" => Ok(options(&st)),
        "PROPFIND" => {
            let _ = axum::body::to_bytes(body, 64 * 1024).await;
            propfind(st, rel, mounted_at_dav, &headers).await
        }
        "GET" | "HEAD" => get_or_head(st, rel, mounted_at_dav, client, method, headers).await,
        "PUT" => put(st, rel, client, headers, body).await,
        "MKCOL" => mkcol(st, rel, &headers).await,
        "DELETE" => delete(st, rel).await,
        "MOVE" => move_or_copy(st, rel, mounted_at_dav, &headers, true).await,
        "COPY" => move_or_copy(st, rel, mounted_at_dav, &headers, false).await,
        "PROPPATCH" => {
            let _ = axum::body::to_bytes(body, 64 * 1024).await;
            proppatch(st, rel, mounted_at_dav).await
        }
        "LOCK" => {
            let _ = axum::body::to_bytes(body, 64 * 1024).await;
            lock(st, rel, mounted_at_dav).await
        }
        "UNLOCK" => unlock(st).await,
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, allow_header(&st))
            .body(Body::empty())
            .unwrap()),
    }
}

fn allow_header(st: &Shared) -> &'static str {
    if st.is_upload_enabled() {
        "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, MOVE, COPY, PROPFIND, PROPPATCH, LOCK, UNLOCK"
    } else {
        "OPTIONS, GET, HEAD, PROPFIND"
    }
}

fn dav_prefix(st: &Shared, mounted_at_dav: bool) -> String {
    match (&st.config.url_token, mounted_at_dav) {
        (Some(t), true) => format!("/s/{t}/dav"),
        (Some(t), false) => format!("/s/{t}"),
        (None, true) => "/dav".to_string(),
        (None, false) => String::new(),
    }
}

fn writable_root(st: &Shared) -> Result<&Path> {
    if !st.is_upload_enabled() {
        return Err(ShareError::UploadsDisabled);
    }
    if let Some(fixed) = &st.config.upload.dir {
        Ok(fixed.as_path())
    } else if st.config.root.kind == RootKind::Dir {
        Ok(st.config.root.path.as_path())
    } else {
        Err(ShareError::UploadsDisabled)
    }
}

fn options(st: &Shared) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("DAV", "1, 2")
        .header("MS-Author-Via", "DAV")
        .header(header::ALLOW, allow_header(st))
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .unwrap()
}

// ---- PROPFIND --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    Zero,
    One,
    Infinity,
}

fn parse_depth(headers: &HeaderMap) -> Result<Depth> {
    let Some(v) = headers.get("depth") else {
        return Ok(Depth::One);
    };
    match v.to_str().unwrap_or("").trim() {
        "0" => Ok(Depth::Zero),
        "1" => Ok(Depth::One),
        "infinity" | "Infinity" => Ok(Depth::Infinity),
        other => Err(ShareError::BadRequest(format!(
            "invalid Depth header '{other}'"
        ))),
    }
}

struct DavItem {
    href: String,
    display_name: String,
    is_dir: bool,
    size: u64,
    modified: Option<std::time::SystemTime>,
    etag: String,
    mime: String,
}

async fn propfind(
    st: Shared,
    rel: &str,
    mounted_at_dav: bool,
    headers: &HeaderMap,
) -> Result<Response> {
    let depth = parse_depth(headers)?;
    let prefix = dav_prefix(&st, mounted_at_dav);
    let root = &st.config.root;
    let show_hidden = st.config.show_hidden;

    let items = match root.kind {
        RootKind::File => {
            let file_name = root
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.name.clone());
            let clean = paths::normalize_rel(rel);
            let meta = tokio::fs::metadata(&root.path).await?;
            if clean.is_empty() {
                let mut v = vec![DavItem {
                    href: format!("{prefix}/"),
                    display_name: root.name.clone(),
                    is_dir: true,
                    size: 0,
                    modified: metadata::last_modified(&meta),
                    etag: metadata::etag(&meta),
                    mime: "inode/directory".into(),
                }];
                if depth != Depth::Zero {
                    v.push(item_from_meta(
                        &prefix, &file_name, &file_name, false, &meta,
                    ));
                }
                v
            } else if clean == file_name {
                vec![item_from_meta(
                    &prefix, &file_name, &file_name, false, &meta,
                )]
            } else {
                return Err(ShareError::NotFound);
            }
        }
        RootKind::Dir => {
            let clean = paths::normalize_rel(rel);
            let target = paths::resolve(&root.path, &clean, show_hidden).await?;
            let root_path = root.path.clone();
            let root_name = root.name.clone();
            tokio::task::spawn_blocking(move || {
                collect_dav_items(
                    &root_path,
                    &target,
                    &clean,
                    &root_name,
                    &prefix,
                    depth,
                    show_hidden,
                )
            })
            .await
            .map_err(|e| ShareError::Internal(format!("PROPFIND task failed: {e}")))??
        }
    };

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str("<D:multistatus xmlns:D=\"DAV:\">\n");
    for item in &items {
        write_propresponse(&mut xml, item);
    }
    xml.push_str("</D:multistatus>\n");

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(xml))
        .map_err(|e| ShareError::Internal(format!("building response: {e}")))
}

fn collect_dav_items(
    root: &Path,
    target: &Path,
    clean_rel: &str,
    root_name: &str,
    prefix: &str,
    depth: Depth,
    show_hidden: bool,
) -> io::Result<Vec<DavItem>> {
    let meta = fs::metadata(target)?;
    let is_dir = meta.is_dir();
    let display_name = if clean_rel.is_empty() {
        root_name.to_string()
    } else {
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| clean_rel.to_string())
    };
    let mut items = vec![item_from_meta(
        prefix,
        clean_rel,
        &display_name,
        is_dir,
        &meta,
    )];

    if !is_dir || depth == Depth::Zero {
        return Ok(items);
    }

    let max_level = match depth {
        Depth::Zero => 0,
        Depth::One => 1,
        Depth::Infinity => MAX_INFINITY_DEPTH,
    };

    let mut stack = vec![(target.to_path_buf(), clean_rel.to_string(), 1usize)];
    while let Some((dir, dir_rel, level)) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in read_dir {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if is_temp_name(&name) || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let (child_path, child_meta, child_is_dir, was_symlink) = if ft.is_symlink() {
                let Ok(canon) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                if !canon.starts_with(root) {
                    continue;
                }
                let Ok(m) = fs::metadata(&canon) else {
                    continue;
                };
                let is_dir = m.is_dir();
                (canon, m, is_dir, true)
            } else {
                let Ok(m) = entry.metadata() else { continue };
                let is_dir = m.is_dir();
                (entry.path(), m, is_dir, false)
            };
            if !child_is_dir && !child_meta.is_file() {
                continue;
            }
            children.push((name, child_path, child_meta, child_is_dir, was_symlink));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, child_path, child_meta, child_is_dir, was_symlink) in children {
            if items.len() >= MAX_PROPFIND_ENTRIES {
                return Ok(items);
            }
            let child_rel = paths::join_rel(&dir_rel, &name);
            items.push(item_from_meta(
                prefix,
                &child_rel,
                &name,
                child_is_dir,
                &child_meta,
            ));
            if child_is_dir && !was_symlink && level < max_level {
                stack.push((child_path, child_rel, level + 1));
            }
        }
    }

    Ok(items)
}

fn item_from_meta(
    prefix: &str,
    rel: &str,
    display_name: &str,
    is_dir: bool,
    meta: &Metadata,
) -> DavItem {
    let encoded = rel
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/");
    let href = if encoded.is_empty() {
        format!("{prefix}/")
    } else if is_dir {
        format!("{prefix}/{encoded}/")
    } else {
        format!("{prefix}/{encoded}")
    };
    DavItem {
        href,
        display_name: display_name.to_string(),
        is_dir,
        size: if is_dir { 0 } else { meta.len() },
        modified: metadata::last_modified(meta),
        etag: metadata::etag(meta),
        mime: if is_dir {
            "inode/directory".into()
        } else {
            metadata::mime_for(display_name).to_string()
        },
    }
}

fn write_propresponse(xml: &mut String, item: &DavItem) {
    let _ = writeln!(xml, "  <D:response>");
    let _ = writeln!(xml, "    <D:href>{}</D:href>", xml_escape(&item.href));
    let _ = writeln!(xml, "    <D:propstat>");
    let _ = writeln!(xml, "      <D:prop>");
    let _ = writeln!(
        xml,
        "        <D:displayname>{}</D:displayname>",
        xml_escape(&item.display_name)
    );
    if item.is_dir {
        let _ = writeln!(
            xml,
            "        <D:resourcetype><D:collection/></D:resourcetype>"
        );
    } else {
        let _ = writeln!(xml, "        <D:resourcetype/>");
        let _ = writeln!(
            xml,
            "        <D:getcontentlength>{}</D:getcontentlength>",
            item.size
        );
        let _ = writeln!(
            xml,
            "        <D:getcontenttype>{}</D:getcontenttype>",
            xml_escape(&item.mime)
        );
    }
    if let Some(m) = item.modified {
        let _ = writeln!(
            xml,
            "        <D:getlastmodified>{}</D:getlastmodified>",
            httpdate::fmt_http_date(m)
        );
    }
    let _ = writeln!(
        xml,
        "        <D:getetag>{}</D:getetag>",
        xml_escape(&item.etag)
    );
    let _ = writeln!(
        xml,
        "        <D:supportedlock><D:lockentry><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry></D:supportedlock>"
    );
    let _ = writeln!(xml, "      </D:prop>");
    let _ = writeln!(xml, "      <D:status>HTTP/1.1 200 OK</D:status>");
    let _ = writeln!(xml, "    </D:propstat>");
    let _ = writeln!(xml, "  </D:response>");
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ---- GET / HEAD / PUT / MKCOL / DELETE / MOVE / COPY -----------------------------------

async fn get_or_head(
    st: Shared,
    rel: &str,
    mounted_at_dav: bool,
    client: ClientAddr,
    method: Method,
    headers: HeaderMap,
) -> Result<Response> {
    if st.config.root.kind == RootKind::Dir {
        let path = paths::resolve(&st.config.root.path, rel, st.config.show_hidden).await?;
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.is_dir() {
                if !mounted_at_dav && method == Method::GET {
                    return Err(ShareError::NotFound);
                }
                return Ok((StatusCode::OK, Body::empty()).into_response());
            }
        }
    }
    download::serve(st, rel, client, method, headers, DownloadQuery::default()).await
}

async fn put(
    st: Shared,
    rel: &str,
    client: ClientAddr,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let root = writable_root(&st)?.to_path_buf();
    let (parent, name) = paths::resolve_parent_and_name(&root, rel, st.config.show_hidden).await?;
    let dest = parent.join(&name);

    let existed = match tokio::fs::metadata(&dest).await {
        Ok(m) if m.is_dir() => {
            return Err(ShareError::BadRequest(
                "cannot overwrite a directory with a file".into(),
            ));
        }
        Ok(_) => true,
        Err(_) => false,
    };

    let expected = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let guard = st.metrics.begin_transfer(
        client.0,
        name.clone(),
        Direction::Upload,
        expected.unwrap_or(0),
    );
    let tmp_path = parent.join(format!(
        "{TEMP_PREFIX}{}-{}{TEMP_SUFFIX}",
        std::process::id(),
        guard.transfer().id
    ));
    let mut tmp = TempFile::new(tmp_path.clone());
    let file = match OpenOptions::new()
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

    match upload::receive(body, file, &guard, expected, st.throttle.clone()).await {
        Ok(_) => {
            if let Err(e) = tokio::fs::rename(&tmp_path, &dest).await {
                let err = ShareError::from(e);
                guard.fail(&err.to_string());
                return Err(err);
            }
            tmp.disarm();
            guard.complete();
            let status = if existed {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            };
            Ok((status, Body::empty()).into_response())
        }
        Err(_) => {
            drop(guard);
            Err(ShareError::BadRequest("WebDAV PUT interrupted".into()))
        }
    }
}

async fn mkcol(st: Shared, rel: &str, headers: &HeaderMap) -> Result<Response> {
    let root = writable_root(&st)?.to_path_buf();
    // RFC 4918 §9.3.1: MKCOL with a non-empty request body must return 415.
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > 0)
    {
        return Ok((StatusCode::UNSUPPORTED_MEDIA_TYPE, Body::empty()).into_response());
    }

    let components = paths::clean_components(rel, st.config.show_hidden)?;
    let Some((&last, parent_parts)) = components.split_last() else {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, Body::empty()).into_response());
    };
    let name = paths::sanitize_upload_name(last)?;
    let parent_rel = parent_parts.join("/");
    let parent = match paths::resolve_dir(&root, &parent_rel, st.config.show_hidden).await {
        Ok(p) => p,
        // RFC 4918: missing intermediate collection is 409 Conflict.
        Err(ShareError::NotFound) => {
            return Ok((StatusCode::CONFLICT, Body::empty()).into_response());
        }
        Err(e) => return Err(e),
    };

    let dest = parent.join(&name);
    if tokio::fs::symlink_metadata(&dest).await.is_ok() {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, Body::empty()).into_response());
    }
    tokio::fs::create_dir(&dest).await?;
    Ok((StatusCode::CREATED, Body::empty()).into_response())
}

async fn delete(st: Shared, rel: &str) -> Result<Response> {
    let root = writable_root(&st)?.to_path_buf();
    let clean = paths::normalize_rel(rel);
    if clean.is_empty() {
        return Err(ShareError::Forbidden(
            "cannot delete the shared root".into(),
        ));
    }
    let target = paths::resolve(&root, &clean, st.config.show_hidden).await?;
    if target == root {
        return Err(ShareError::Forbidden(
            "cannot delete the shared root".into(),
        ));
    }
    let meta = tokio::fs::metadata(&target).await?;
    if meta.is_dir() {
        tokio::fs::remove_dir_all(&target).await?;
    } else {
        tokio::fs::remove_file(&target).await?;
    }
    Ok((StatusCode::NO_CONTENT, Body::empty()).into_response())
}

async fn move_or_copy(
    st: Shared,
    rel: &str,
    mounted_at_dav: bool,
    headers: &HeaderMap,
    is_move: bool,
) -> Result<Response> {
    let root = writable_root(&st)?.to_path_buf();
    let clean_src = paths::normalize_rel(rel);
    if clean_src.is_empty() {
        return Err(ShareError::Forbidden("cannot move/copy the root".into()));
    }
    let src_path = paths::resolve(&root, &clean_src, st.config.show_hidden).await?;

    let dest_hdr = headers
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ShareError::BadRequest("missing Destination header".into()))?;
    let prefix = dav_prefix(&st, mounted_at_dav);
    let dst_rel = parse_destination_rel(dest_hdr, &prefix)?;
    let (dst_parent, dst_name) =
        match paths::resolve_parent_and_name(&root, &dst_rel, st.config.show_hidden).await {
            Ok(v) => v,
            Err(ShareError::NotFound) => {
                return Ok((StatusCode::CONFLICT, Body::empty()).into_response());
            }
            Err(e) => return Err(e),
        };
    let dst_path = dst_parent.join(&dst_name);
    if src_path == dst_path {
        return Err(ShareError::Forbidden(
            "source and destination are identical".into(),
        ));
    }
    if dst_path.starts_with(&src_path) {
        return Err(ShareError::Forbidden(
            "cannot move/copy a directory into itself".into(),
        ));
    }

    let overwrite = headers
        .get("overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|s| !s.trim().eq_ignore_ascii_case("F"))
        .unwrap_or(true);

    let dst_existed = tokio::fs::symlink_metadata(&dst_path).await.is_ok();
    if dst_existed {
        if !overwrite {
            return Ok((StatusCode::PRECONDITION_FAILED, Body::empty()).into_response());
        }
        if let Ok(m) = tokio::fs::metadata(&dst_path).await {
            if m.is_dir() {
                tokio::fs::remove_dir_all(&dst_path).await?;
            } else {
                tokio::fs::remove_file(&dst_path).await?;
            }
        }
    }

    if is_move {
        tokio::fs::rename(&src_path, &dst_path).await?;
    } else {
        let src_meta = tokio::fs::metadata(&src_path).await?;
        if src_meta.is_dir() {
            let root_clone = root.clone();
            let show_hidden = st.config.show_hidden;
            tokio::task::spawn_blocking(move || {
                copy_dir_recursive(&root_clone, &src_path, &dst_path, show_hidden)
            })
            .await
            .map_err(|e| ShareError::Internal(format!("COPY task failed: {e}")))??;
        } else {
            tokio::fs::copy(&src_path, &dst_path).await?;
        }
    }

    let status = if dst_existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    Ok((status, Body::empty()).into_response())
}

fn parse_destination_rel(dest_hdr: &str, dav_prefix: &str) -> Result<String> {
    let raw_path = if let Some(scheme_end) = dest_hdr.find("://") {
        let after_scheme = &dest_hdr[scheme_end + 3..];
        match after_scheme.find('/') {
            Some(slash) => &after_scheme[slash..],
            None => "/",
        }
    } else {
        dest_hdr
    };
    let path_only = raw_path.split('?').next().unwrap_or(raw_path);
    let rel_encoded = if dav_prefix.is_empty() {
        path_only.strip_prefix("/dav").unwrap_or(path_only)
    } else {
        path_only
            .strip_prefix(dav_prefix)
            .or_else(|| path_only.strip_prefix("/dav"))
            .ok_or_else(|| ShareError::BadRequest("Destination is outside /dav".into()))?
    };

    let mut decoded_parts = Vec::new();
    for seg in rel_encoded.split('/') {
        if seg.is_empty() {
            continue;
        }
        let decoded = percent_decode_str(seg)
            .decode_utf8()
            .map_err(|_| ShareError::BadRequest("Destination is not valid UTF-8".into()))?;
        decoded_parts.push(decoded.into_owned());
    }
    Ok(decoded_parts.join("/"))
}

fn copy_dir_recursive(root: &Path, src: &Path, dst: &Path, show_hidden: bool) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_temp_name(&name) || (!show_hidden && name.starts_with('.')) {
            continue;
        }
        let canon = fs::canonicalize(entry.path())?;
        if !canon.starts_with(root) {
            continue;
        }
        let meta = fs::metadata(&canon)?;
        let dst_child = dst.join(&name);
        if meta.is_dir() {
            copy_dir_recursive(root, &canon, &dst_child, show_hidden)?;
        } else if meta.is_file() {
            fs::copy(&canon, &dst_child)?;
        }
    }
    Ok(())
}

// ---- PROPPATCH / LOCK / UNLOCK (macOS Finder & Windows Explorer compatibility) ---------

async fn proppatch(st: Shared, rel: &str, mounted_at_dav: bool) -> Result<Response> {
    let root = writable_root(&st)?;
    let clean = paths::normalize_rel(rel);
    let _ = paths::resolve(root, &clean, st.config.show_hidden).await?;
    let href = format!(
        "{}/{}",
        dav_prefix(&st, mounted_at_dav),
        encode_path_segment(&clean)
    );
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <D:multistatus xmlns:D=\"DAV:\">\n\
         <D:response>\n\
         <D:href>{}</D:href>\n\
         <D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat>\n\
         </D:response>\n\
         </D:multistatus>\n",
        xml_escape(&href)
    );
    Ok((
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn lock(st: Shared, rel: &str, mounted_at_dav: bool) -> Result<Response> {
    let _ = writable_root(&st)?;
    let id = LOCK_SEQ.fetch_add(1, Relaxed);
    let token = format!("opaquelocktoken:share-lock-{}-{id}", std::process::id());
    let href = format!(
        "{}/{}",
        dav_prefix(&st, mounted_at_dav),
        paths::normalize_rel(rel)
    );
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <D:prop xmlns:D=\"DAV:\">\n\
         <D:lockdiscovery>\n\
         <D:activelock>\n\
         <D:locktype><D:write/></D:locktype>\n\
         <D:lockscope><D:exclusive/></D:lockscope>\n\
         <D:depth>infinity</D:depth>\n\
         <D:timeout>Second-3600</D:timeout>\n\
         <D:locktoken><D:href>{token}</D:href></D:locktoken>\n\
         <D:lockroot><D:href>{}</D:href></D:lockroot>\n\
         </D:activelock>\n\
         </D:lockdiscovery>\n\
         </D:prop>\n",
        xml_escape(&href)
    );
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/xml; charset=utf-8".to_string(),
            ),
            (
                header::HeaderName::from_static("lock-token"),
                format!("<{token}>"),
            ),
        ],
        xml,
    )
        .into_response())
}

async fn unlock(st: Shared) -> Result<Response> {
    let _ = writable_root(&st)?;
    Ok((StatusCode::NO_CONTENT, Body::empty()).into_response())
}
