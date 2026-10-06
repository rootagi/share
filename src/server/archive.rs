//! Streaming `.zip`, `.tar.gz`, and `.tar` directory downloads.
//!
//! Walks a shared directory inside `spawn_blocking`, serialises entries into a
//! streaming ZIP archive (`Deflate` + data descriptors) or POSIX USTAR/PAX tar
//! stream (optionally compressed on the fly with `flate2`), and feeds chunks
//! through a bounded `mpsc` channel into the HTTP response body.
//! No temporary archive file is ever written to disk.

use std::fs::{self, Metadata};
use std::io::{self, Read, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::UNIX_EPOCH;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::ClientAddr;
use super::response::content_disposition;
use super::throttle::Throttle;
use crate::config::RootKind;
use crate::error::{Result, ShareError};
use crate::fs::paths::{self, is_temp_name};
use crate::metrics::{Direction, Shared, TransferGuard};

const ARCHIVE_CHUNK: usize = 64 * 1024;
const CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
    Tar,
}

impl ArchiveFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::TarGz => ".tar.gz",
            Self::Tar => ".tar",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::TarGz => "application/gzip",
            Self::Tar => "application/x-tar",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ArchiveQuery {
    /// `"zip"`, `"tar.gz"` (default), or `"tar"`.
    pub format: Option<String>,
}

pub async fn archive_root(
    State(st): State<Shared>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    Query(q): Query<ArchiveQuery>,
) -> Result<Response> {
    serve_archive(st, "", client, method, q).await
}

pub async fn archive_path(
    State(st): State<Shared>,
    AxumPath(rel): AxumPath<String>,
    Extension(client): Extension<ClientAddr>,
    method: Method,
    Query(q): Query<ArchiveQuery>,
) -> Result<Response> {
    serve_archive(st, &rel, client, method, q).await
}

async fn serve_archive(
    st: Shared,
    raw_rel: &str,
    client: ClientAddr,
    method: Method,
    q: ArchiveQuery,
) -> Result<Response> {
    let root = &st.config.root;
    if root.kind != RootKind::Dir {
        return Err(ShareError::NotFound);
    }

    let explicit_format = match q.format.as_deref() {
        Some("zip") => Some(ArchiveFormat::Zip),
        Some("tar") => Some(ArchiveFormat::Tar),
        Some("tar.gz" | "tgz" | "gzip") => Some(ArchiveFormat::TarGz),
        Some(other) => {
            return Err(ShareError::BadRequest(format!(
                "unsupported archive format '{other}' (use zip, tar.gz, or tar)"
            )));
        }
        None => None,
    };

    // Resolve the directory, stripping a `.zip`, `.tar.gz`, `.tgz`, or `.tar` suffix if needed.
    let (dir, format) = match paths::resolve_dir(&root.path, raw_rel, st.config.show_hidden).await {
        Ok(d) => (d, explicit_format.unwrap_or(ArchiveFormat::TarGz)),
        Err(ShareError::NotFound) => {
            if let Some(stripped) = raw_rel.strip_suffix(".zip") {
                let d = paths::resolve_dir(&root.path, stripped, st.config.show_hidden).await?;
                (d, explicit_format.unwrap_or(ArchiveFormat::Zip))
            } else if let Some(stripped) = raw_rel.strip_suffix(".tar.gz") {
                let d = paths::resolve_dir(&root.path, stripped, st.config.show_hidden).await?;
                (d, explicit_format.unwrap_or(ArchiveFormat::TarGz))
            } else if let Some(stripped) = raw_rel.strip_suffix(".tgz") {
                let d = paths::resolve_dir(&root.path, stripped, st.config.show_hidden).await?;
                (d, explicit_format.unwrap_or(ArchiveFormat::TarGz))
            } else if let Some(stripped) = raw_rel.strip_suffix(".tar") {
                let d = paths::resolve_dir(&root.path, stripped, st.config.show_hidden).await?;
                (d, explicit_format.unwrap_or(ArchiveFormat::Tar))
            } else {
                return Err(ShareError::NotFound);
            }
        }
        Err(e) => return Err(e),
    };

    let base_name = if dir == root.path {
        root.name.clone()
    } else {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".into())
    };
    let archive_filename = format!("{base_name}{}", format.extension());

    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, format.content_type())
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(false, &archive_filename),
        )
        .header(header::CACHE_CONTROL, "no-cache");

    if method == Method::HEAD {
        return builder
            .body(Body::empty())
            .map_err(|e| ShareError::Internal(format!("building response: {e}")));
    }

    let guard = st
        .metrics
        .begin_transfer(client.0, archive_filename, Direction::Download, 0);
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(CHANNEL_CAPACITY);
    let root_path = root.path.clone();
    let show_hidden = st.config.show_hidden;
    let throttle = st.throttle.clone();

    tokio::task::spawn_blocking(move || {
        stream_directory_archive(
            &root_path,
            &dir,
            &base_name,
            show_hidden,
            format,
            tx,
            guard,
            throttle,
        );
    });

    let stream = ChannelStream { rx };
    builder
        .body(Body::from_stream(stream))
        .map_err(|e| ShareError::Internal(format!("building response: {e}")))
}

struct ChannelStream {
    rx: mpsc::Receiver<io::Result<Bytes>>,
}

impl Stream for ChannelStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

struct ChannelWriter<'a> {
    tx: mpsc::Sender<io::Result<Bytes>>,
    buf: Vec<u8>,
    guard: &'a TransferGuard,
    throttle: Option<Arc<Throttle>>,
}

impl<'a> ChannelWriter<'a> {
    fn new(
        tx: mpsc::Sender<io::Result<Bytes>>,
        guard: &'a TransferGuard,
        throttle: Option<Arc<Throttle>>,
    ) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(ARCHIVE_CHUNK),
            guard,
            throttle,
        }
    }

    fn send_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.guard.transfer().cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transfer cancelled by operator",
            ));
        }
        let chunk = Bytes::from(std::mem::replace(
            &mut self.buf,
            Vec::with_capacity(ARCHIVE_CHUNK),
        ));
        let n = chunk.len() as u64;
        if let Some(delay) = self.throttle.as_ref().and_then(|t| t.acquire_delay(n)) {
            std::thread::sleep(delay);
        }
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client disconnected"))?;
        self.guard.add(n);
        Ok(())
    }
}

impl Write for ChannelWriter<'_> {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            let space = ARCHIVE_CHUNK - self.buf.len();
            let take = data.len().min(space);
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() >= ARCHIVE_CHUNK {
                self.send_buf()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_buf()
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_directory_archive(
    root: &Path,
    dir: &Path,
    base_name: &str,
    show_hidden: bool,
    format: ArchiveFormat,
    tx: mpsc::Sender<io::Result<Bytes>>,
    guard: TransferGuard,
    throttle: Option<Arc<Throttle>>,
) {
    let writer = ChannelWriter::new(tx.clone(), &guard, throttle);
    let res = match format {
        ArchiveFormat::Zip => {
            let mut w = writer;
            write_zip_tree(&mut w, root, dir, base_name, show_hidden, &guard)
                .and_then(|()| w.flush())
        }
        ArchiveFormat::TarGz => {
            let mut gz = GzEncoder::new(writer, Compression::fast());
            let r = write_tar_tree(&mut gz, root, dir, base_name, show_hidden, &guard);
            r.and_then(|()| {
                let mut w = gz.finish()?;
                w.flush()
            })
        }
        ArchiveFormat::Tar => {
            let mut w = writer;
            write_tar_tree(&mut w, root, dir, base_name, show_hidden, &guard)
                .and_then(|()| w.flush())
        }
    };

    match res {
        Ok(()) => guard.complete(),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionAborted
            ) =>
        {
            // Client disconnected or operator cancelled: dropping `guard` records Aborted.
            drop(guard);
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = tx.blocking_send(Err(e));
            guard.fail(&msg);
        }
    }
}

struct CountingWriter<'a, W> {
    inner: &'a mut W,
    written: u64,
}

impl<'a, W: Write> CountingWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct ZipCentralEntry {
    name: String,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    external_attr: u32,
    local_header_offset: u32,
}

fn write_zip_tree<W: Write>(
    out: &mut W,
    root: &Path,
    dir: &Path,
    base_name: &str,
    show_hidden: bool,
    guard: &TransferGuard,
) -> io::Result<()> {
    let clean_base = base_name.trim_matches('/');
    let root_prefix = if clean_base.is_empty() {
        "archive".to_string()
    } else {
        clean_base.to_string()
    };

    let mut cw = CountingWriter::new(out);
    let mut entries: Vec<ZipCentralEntry> = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), root_prefix)];

    while let Some((current_dir, archive_rel)) = stack.pop() {
        if guard.transfer().cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transfer cancelled by operator",
            ));
        }

        if let Ok(dir_meta) = fs::metadata(&current_dir) {
            let dir_entry_name = format!("{archive_rel}/");
            let (dos_time, dos_date) = zip_dos_datetime(&dir_meta);
            let offset = cw.written as u32;
            write_zip_local_header(&mut cw, &dir_entry_name, 0x0800, 0, dos_time, dos_date)?;
            entries.push(ZipCentralEntry {
                name: dir_entry_name,
                flags: 0x0800,
                method: 0,
                dos_time,
                dos_date,
                crc32: 0,
                compressed_size: 0,
                uncompressed_size: 0,
                external_attr: (0o40755u32 << 16) | 0x10,
                local_header_offset: offset,
            });
        }

        let Ok(read_dir) = fs::read_dir(&current_dir) else {
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
            let (real_path, meta, is_dir, was_symlink) = if ft.is_symlink() {
                let Ok(target) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                if !target.starts_with(root) {
                    continue;
                }
                let Ok(m) = fs::metadata(&target) else {
                    continue;
                };
                let is_dir = m.is_dir();
                (target, m, is_dir, true)
            } else {
                let Ok(m) = entry.metadata() else { continue };
                let is_dir = m.is_dir();
                (entry.path(), m, is_dir, false)
            };
            if !is_dir && !meta.is_file() {
                continue;
            }
            children.push((name, real_path, meta, is_dir, was_symlink));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, real_path, _, is_dir, was_symlink) in children.iter().rev() {
            if *is_dir && !*was_symlink {
                stack.push((real_path.clone(), format!("{archive_rel}/{name}")));
            }
        }

        for (name, real_path, meta, is_dir, _) in children {
            if is_dir {
                continue;
            }
            let entry_rel = format!("{archive_rel}/{name}");
            let entry = write_zip_file(&mut cw, &real_path, &entry_rel, &meta, guard)?;
            entries.push(entry);
        }
    }

    let cd_offset = cw.written as u32;
    for e in &entries {
        write_zip_central_header(&mut cw, e)?;
    }
    let cd_size = (cw.written as u32).saturating_sub(cd_offset);
    write_zip_eocd(&mut cw, entries.len() as u16, cd_size, cd_offset)
}

fn write_zip_file<W: Write>(
    cw: &mut CountingWriter<'_, W>,
    path: &Path,
    archive_rel: &str,
    meta: &Metadata,
    guard: &TransferGuard,
) -> io::Result<ZipCentralEntry> {
    let mut file = fs::File::open(path)?;
    let (dos_time, dos_date) = zip_dos_datetime(meta);
    let offset = cw.written as u32;
    // Bit 3 (0x0008): sizes & CRC follow in Data Descriptor; Bit 11 (0x0800): UTF-8 filename.
    let flags: u16 = 0x0808;
    let method: u16 = 8; // Deflate
    write_zip_local_header(cw, archive_rel, flags, method, dos_time, dos_date)?;

    let data_start = cw.written;
    let mut crc = Crc::new();
    let mut uncompressed: u64 = 0;
    {
        let mut deflate = DeflateEncoder::new(&mut *cw, Compression::fast());
        let mut buf = [0u8; 64 * 1024];
        loop {
            if guard.transfer().cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "transfer cancelled by operator",
                ));
            }
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            deflate.write_all(&buf[..n])?;
            uncompressed += n as u64;
        }
        deflate.finish()?;
    }
    let compressed = cw.written.saturating_sub(data_start);
    let crc32 = crc.sum();

    // 16-byte Data Descriptor (PK\x07\x08 + crc32 + compressed_size + uncompressed_size)
    let mut dd = [0u8; 16];
    dd[0..4].copy_from_slice(&0x0807_4b50u32.to_le_bytes());
    dd[4..8].copy_from_slice(&crc32.to_le_bytes());
    dd[8..12].copy_from_slice(&(compressed as u32).to_le_bytes());
    dd[12..16].copy_from_slice(&(uncompressed as u32).to_le_bytes());
    cw.write_all(&dd)?;

    Ok(ZipCentralEntry {
        name: archive_rel.to_string(),
        flags,
        method,
        dos_time,
        dos_date,
        crc32,
        compressed_size: compressed as u32,
        uncompressed_size: uncompressed as u32,
        external_attr: 0o100644u32 << 16,
        local_header_offset: offset,
    })
}

fn write_zip_local_header<W: Write>(
    out: &mut W,
    name: &str,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
) -> io::Result<()> {
    let name_bytes = name.as_bytes();
    let mut hdr = [0u8; 30];
    hdr[0..4].copy_from_slice(&0x0403_4b50u32.to_le_bytes());
    hdr[4..6].copy_from_slice(&20u16.to_le_bytes());
    hdr[6..8].copy_from_slice(&flags.to_le_bytes());
    hdr[8..10].copy_from_slice(&method.to_le_bytes());
    hdr[10..12].copy_from_slice(&dos_time.to_le_bytes());
    hdr[12..14].copy_from_slice(&dos_date.to_le_bytes());
    // CRC-32, compressed size, uncompressed size are 0 in local header (supplied in Data Descriptor or 0 for dirs).
    hdr[26..28].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    hdr[28..30].copy_from_slice(&0u16.to_le_bytes());
    out.write_all(&hdr)?;
    out.write_all(name_bytes)
}

fn write_zip_central_header<W: Write>(out: &mut W, e: &ZipCentralEntry) -> io::Result<()> {
    let name_bytes = e.name.as_bytes();
    let mut hdr = [0u8; 46];
    hdr[0..4].copy_from_slice(&0x0201_4b50u32.to_le_bytes());
    hdr[4..6].copy_from_slice(&0x0314u16.to_le_bytes()); // Unix + version 2.0
    hdr[6..8].copy_from_slice(&20u16.to_le_bytes());
    hdr[8..10].copy_from_slice(&e.flags.to_le_bytes());
    hdr[10..12].copy_from_slice(&e.method.to_le_bytes());
    hdr[12..14].copy_from_slice(&e.dos_time.to_le_bytes());
    hdr[14..16].copy_from_slice(&e.dos_date.to_le_bytes());
    hdr[16..20].copy_from_slice(&e.crc32.to_le_bytes());
    hdr[20..24].copy_from_slice(&e.compressed_size.to_le_bytes());
    hdr[24..28].copy_from_slice(&e.uncompressed_size.to_le_bytes());
    hdr[28..30].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    hdr[30..32].copy_from_slice(&0u16.to_le_bytes());
    hdr[32..34].copy_from_slice(&0u16.to_le_bytes());
    hdr[34..36].copy_from_slice(&0u16.to_le_bytes());
    hdr[36..38].copy_from_slice(&0u16.to_le_bytes());
    hdr[38..42].copy_from_slice(&e.external_attr.to_le_bytes());
    hdr[42..46].copy_from_slice(&e.local_header_offset.to_le_bytes());
    out.write_all(&hdr)?;
    out.write_all(name_bytes)
}

fn write_zip_eocd<W: Write>(
    out: &mut W,
    count: u16,
    cd_size: u32,
    cd_offset: u32,
) -> io::Result<()> {
    let mut eocd = [0u8; 22];
    eocd[0..4].copy_from_slice(&0x0605_4b50u32.to_le_bytes());
    eocd[4..6].copy_from_slice(&0u16.to_le_bytes());
    eocd[6..8].copy_from_slice(&0u16.to_le_bytes());
    eocd[8..10].copy_from_slice(&count.to_le_bytes());
    eocd[10..12].copy_from_slice(&count.to_le_bytes());
    eocd[12..16].copy_from_slice(&cd_size.to_le_bytes());
    eocd[16..20].copy_from_slice(&cd_offset.to_le_bytes());
    eocd[20..22].copy_from_slice(&0u16.to_le_bytes());
    out.write_all(&eocd)
}

fn zip_dos_datetime(meta: &Metadata) -> (u16, u16) {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(315_532_800);
    if let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(secs) {
        let year = (dt.year().clamp(1980, 2107) - 1980) as u16;
        let month = dt.month() as u16;
        let day = dt.day() as u16;
        let hour = dt.hour() as u16;
        let minute = dt.minute() as u16;
        let second = (dt.second() as u16) / 2;
        let dos_date = (year << 9) | (month << 5) | day;
        let dos_time = (hour << 11) | (minute << 5) | second;
        (dos_time, dos_date)
    } else {
        (0, (1 << 5) | 1)
    }
}

fn write_tar_tree<W: Write>(
    out: &mut W,
    root: &Path,
    dir: &Path,
    base_name: &str,
    show_hidden: bool,
    guard: &TransferGuard,
) -> io::Result<()> {
    let clean_base = base_name.trim_matches('/');
    let root_prefix = if clean_base.is_empty() {
        "archive".to_string()
    } else {
        clean_base.to_string()
    };

    let mut stack = vec![(dir.to_path_buf(), root_prefix)];

    while let Some((current_dir, archive_rel)) = stack.pop() {
        if guard.transfer().cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "transfer cancelled by operator",
            ));
        }

        if let Ok(dir_meta) = fs::metadata(&current_dir) {
            write_tar_header(out, &format!("{archive_rel}/"), 0, &dir_meta, b'5')?;
        }

        let Ok(read_dir) = fs::read_dir(&current_dir) else {
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
            let (real_path, meta, is_dir, was_symlink) = if ft.is_symlink() {
                let Ok(target) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                if !target.starts_with(root) {
                    continue;
                }
                let Ok(m) = fs::metadata(&target) else {
                    continue;
                };
                let is_dir = m.is_dir();
                (target, m, is_dir, true)
            } else {
                let Ok(m) = entry.metadata() else { continue };
                let is_dir = m.is_dir();
                (entry.path(), m, is_dir, false)
            };
            if !is_dir && !meta.is_file() {
                continue;
            }
            children.push((name, real_path, meta, is_dir, was_symlink));
        }
        // Sort so archive order is deterministic.
        children.sort_by(|a, b| a.0.cmp(&b.0));

        // Push subdirectories in reverse so they pop in alphabetical order.
        for (name, real_path, _, is_dir, was_symlink) in children.iter().rev() {
            if *is_dir && !*was_symlink {
                stack.push((real_path.clone(), format!("{archive_rel}/{name}")));
            }
        }

        // Write files in this directory.
        for (name, real_path, meta, is_dir, _) in children {
            if is_dir {
                continue;
            }
            let entry_rel = format!("{archive_rel}/{name}");
            write_tar_file(out, &real_path, &entry_rel, &meta)?;
        }
    }

    // End-of-archive marker: two 512-byte zero blocks.
    out.write_all(&[0u8; 1024])
}

fn write_tar_file<W: Write>(
    out: &mut W,
    path: &Path,
    archive_rel: &str,
    meta: &Metadata,
) -> io::Result<()> {
    let mut file = fs::File::open(path)?;
    let size = meta.len();
    write_tar_header(out, archive_rel, size, meta, b'0')?;

    let mut remaining = size;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            // File shrank while reading: pad with zeros so the tar stream stays aligned.
            let zeros = [0u8; 4096];
            while remaining > 0 {
                let z = (remaining as usize).min(zeros.len());
                out.write_all(&zeros[..z])?;
                remaining -= z as u64;
            }
            break;
        }
        out.write_all(&buf[..n])?;
        remaining -= n as u64;
    }

    let pad = (512 - (size % 512) as usize) % 512;
    if pad > 0 {
        let zeros = [0u8; 512];
        out.write_all(&zeros[..pad])?;
    }
    Ok(())
}

fn write_tar_header<W: Write>(
    out: &mut W,
    path: &str,
    size: u64,
    meta: &Metadata,
    typeflag: u8,
) -> io::Result<()> {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let needs_pax = path.len() > 99 || !path.is_ascii() || size > 0o777_7777_7777;
    if needs_pax {
        let mut pax = format_pax_record("path", path);
        if size > 0o777_7777_7777 {
            pax.extend_from_slice(&format_pax_record("size", &size.to_string()));
        }
        let pax_header = build_ustar_block("././@PaxHeader", pax.len() as u64, mtime, 0o644, b'x');
        out.write_all(&pax_header)?;
        out.write_all(&pax)?;
        let pad = (512 - (pax.len() % 512)) % 512;
        if pad > 0 {
            out.write_all(&[0u8; 512][..pad])?;
        }
    }

    let mode = if typeflag == b'5' { 0o755 } else { 0o644 };
    let ustar_size = size.min(0o777_7777_7777);
    let block = build_ustar_block(path, ustar_size, mtime, mode, typeflag);
    out.write_all(&block)
}

fn format_pax_record(key: &str, val: &str) -> Vec<u8> {
    let body_len = 1 + key.len() + 1 + val.len() + 1;
    let mut digits = body_len.to_string().len();
    while (digits + body_len).to_string().len() > digits {
        digits += 1;
    }
    let total = digits + body_len;
    format!("{total} {key}={val}\n").into_bytes()
}

fn build_ustar_block(path: &str, size: u64, mtime: u64, mode: u32, typeflag: u8) -> [u8; 512] {
    let mut block = [0u8; 512];
    let path_bytes = path.as_bytes();
    let copy_len = path_bytes.len().min(100);
    block[..copy_len].copy_from_slice(&path_bytes[..copy_len]);

    write_octal(&mut block[100..108], mode as u64, 7);
    write_octal(&mut block[108..116], 0, 7);
    write_octal(&mut block[116..124], 0, 7);
    write_octal(&mut block[124..136], size, 11);
    write_octal(&mut block[136..148], mtime, 11);

    block[148..156].fill(b' ');
    block[156] = typeflag;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[265..270].copy_from_slice(b"share");
    block[297..302].copy_from_slice(b"share");
    write_octal(&mut block[329..337], 0, 7);
    write_octal(&mut block[337..345], 0, 7);

    let checksum: u32 = block.iter().map(|&b| b as u32).sum();
    let chk_str = format!("{checksum:06o}\0 ");
    block[148..156].copy_from_slice(chk_str.as_bytes());
    block
}

fn write_octal(dst: &mut [u8], val: u64, digits: usize) {
    let s = format!("{val:0digits$o}");
    let bytes = s.as_bytes();
    let len = bytes.len().min(dst.len().saturating_sub(1));
    dst[..len].copy_from_slice(&bytes[bytes.len() - len..]);
    dst[len] = 0;
}
