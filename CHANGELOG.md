# Changelog

All notable changes to `share` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-06

### Added
- **Core HTTP/HTTPS & Dual-Protocol Server**:
  - Automatic self-signed X.509 certificate generation and caching (`~/.config/share/`) with Subject Alternative Names (SANs) for detected LAN IPs and `localhost`.
  - First-byte TLS detection (`0x16`) allowing HTTPS browsers, plain-HTTP CLI clients, and OS WebDAV mounts to share a single TCP port.
  - Custom TLS certificate support (`--cert` and `--key`) and explicit plain-HTTP mode (`--http`).
- **Downloads & Streaming Folder Archives**:
  - Constant-memory 256 KiB chunked file streaming with RFC 9110 `Range` (`206 Partial Content`), `If-Range`, `If-None-Match` (`304 Not Modified`), `HEAD`, and 64-bit (`> 4 GiB`) offset support.
  - On-the-fly streaming folder archive downloads in `.zip`, `.tar.gz`, and `.tar` formats (`/archive/<path>?format=zip|tar.gz|tar`) with zero temporary files on disk.
- **Uploads, Recursive Folder Drop & Resumable Staging**:
  - Atomic `.part` staging and non-destructive collision naming (`name (1).ext`, `name (2).ext`) via `POST` and `PUT /api/upload` (`--upload` and `--upload-dir`).
  - Recursive folder drag-and-drop uploads (`?mkdir=true`) preserving nested directory hierarchies.
  - Resumable upload offset inspection (`GET /api/upload/status` and `HEAD /api/upload`) and byte-offset continuation (`?offset=<N>` / `X-Share-Offset`).
- **Native OS WebDAV Server (`RFC 4918`)**:
  - Built-in Class 1 & 2 WebDAV support (`OPTIONS`, `PROPFIND`, `GET`, `HEAD`, `PUT`, `MKCOL`, `DELETE`, `MOVE`, `COPY`, `LOCK`, `UNLOCK`) mounted at `/dav` and `/` for Linux (`gvfsd-dav` / GNOME Files), macOS Finder, and Windows Explorer.
- **Access Controls, Ephemeral Quotas & Rate Limiting**:
  - Secret capability URL prefixes (`--random-url` and `--token <TOKEN>`) scoping all routes under `/s/<token>/...`.
  - PIN (`--pin`) and username/password (`--auth`) authentication via HTTP Basic Auth and an in-page web session modal.
  - Automatic shutdown quotas by download count (`--max-downloads <N>`) and duration (`--expire <DURATION>`).
  - Global token-bucket bandwidth throttling (`--rate-limit <RATE>`).
- **Interactive Terminal UI (`ratatui`) & Embedded Web UI**:
  - Live terminal dashboard with throughput sparklines, per-transfer progress gauges and ETAs, connected client list, offline ANSI/Unicode QR code modal (`--qr`, `[P]`), and ring-buffered log viewer (`[L]`).
  - Live operator controls: `[Y]` copy URL/logs to clipboard (Wayland, X11, macOS, Windows, and OSC 52), `[U]` toggle uploads on/off at runtime, and `[X]` cancel active transfers.
  - Self-contained web interface with natural sorting, live filtering, optional recursive search (`--recursive`), dark/light theme toggle, and an interactive media & code preview lightbox.
  - Shell completion generator (`--completions bash|zsh|fish|elvish|powershell`) and example `systemd` user service unit.
