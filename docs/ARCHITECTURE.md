# Architecture of `share`

`share` is structured as a library crate (`src/lib.rs`) paired with a thin binary entrypoint (`src/main.rs`). Keeping all server, configuration, filesystem, TLS, metrics, and TUI logic in the library allows integration tests (`tests/*.rs`) to spin up the real server in-process on an ephemeral port (`127.0.0.1:0`) and inspect internal state (`AppState`, `MetricsSnapshot`) directly.

---

## High-Level ASCII Architecture Diagram

```text
                             ┌──────────────────────┐
                             │       CLI Args       │
                             │      (src/cli.rs)    │
                             └──────────┬───────────┘
                                        │
                                        ▼
                             ┌──────────────────────┐
                             │   Validated Config   │
                             │   (src/config.rs)    │
                             └──────────┬───────────┘
                                        │
         ┌──────────────────────────────┼──────────────────────────────┐
         ▼                              ▼                              ▼
┌─────────────────────┐      ┌─────────────────────┐      ┌─────────────────────┐
│  Network Discovery  │      │      TLS Setup      │      │   TCP Listener      │
│  (src/network/*)    │─────▶│     (src/tls/*)     │      │ (src/server/mod.rs) │
│  if-addrs, ranking  │ SANs │  rcgen / PEM cache  │      │ SO_REUSEADDR, 1024  │
└────────┬────────────┘      └──────────┬──────────┘      └──────────┬──────────┘
         │                              │                            │
         └──────────────────────────────┼────────────────────────────┘
                                        ▼
                        ┌───────────────────────────────┐
                        │      Shared (Arc<AppState>)   │
                        │  • Config                     │
                        │  • NetworkInfo (URLs, SHA256) │
                        │  • Arc<Metrics> (atomics)     │
                        │  • Arc<LogBuffer> (ring buf)  │
                        │  • AtomicU8 ServerStatus      │
                        └───────┬───────────────┬───────┘
                                │               │
         ┌──────────────────────┤               ├──────────────────────┐
         ▼                      ▼               ▼                      ▼
┌─────────────────┐   ┌──────────────────┐   ┌──────────────────┐   ┌─────────────────┐
│  server::serve  │   │ metrics::sampler │   │ tui::run / banner│   │  signals::spawn │
│  (axum + hyper) │   │ (500 ms ticker)  │   │ (100 ms / events)│   │ (SIGINT/SIGTERM)│
└────────┬────────┘   └────────┬─────────┘   └────────┬─────────┘   └────────┬────────┘
         │                     │                      │                      │
         │ Relaxed AtomicU64   │ EWMA speed calc      │ Snapshot read        │ cancel()
         └────────────────────▶│ (α = 0.5)            │◀─────────────────────┘
                               └─────────────────────▶│
                                        CancellationToken
```

---

## Module Responsibilities

| Module | File(s) | Responsibility |
| :--- | :--- | :--- |
| **`main`** | `src/main.rs` | Parses CLI flags, handles `--completions`, detects TTY for TUI vs. headless mode, initializes `tracing`, builds the multi-threaded Tokio runtime, runs the app, and prints final transfer summary statistics on exit. |
| **`app`** | `src/app.rs` | Orchestrates startup (`app::start`): runs network discovery, sets up TLS, binds the TCP socket, constructs `Arc<AppState>`, and spawns `server::serve` and `metrics::spawn_sampler` attached to a shared `CancellationToken`. |
| **`cli`** | `src/cli.rs` | Declarative `clap` derive struct defining all flags, value names, defaults, and mutual-exclusion rules (`conflicts_with`, `requires`). |
| **`config`** | `src/config.rs` | Transforms `Cli` + `[NetIface]` into a validated `Config`: canonicalizes the share root (`RootKind::Dir` or `RootKind::File`), prepares `--upload-dir`, resolves `--interface`, and maps log verbosity. |
| **`error`** | `src/error.rs` | Central `ShareError` enum (`thiserror`) covering both startup errors (with actionable `.hint()` strings) and HTTP request errors (implementing `axum::response::IntoResponse` to return structured JSON `{"error": ..., "status": ...}`). |
| **`fs::paths`** | `src/fs/paths.rs` | Security boundary for filesystem access. Cleans URL relative paths (`clean_components`), rejects `..` (`403`) and NUL bytes (`400`), hides dotfiles unless `--hidden` (`404`), blocks `.share-upload-*.part` temp files, resolves and verifies canonical paths stay inside the share root, sanitizes upload filenames, and generates collision-free candidate names (`name (1).ext`). |
| **`fs::metadata`** | `src/fs/metadata.rs` | Maps extensions to MIME types (`mime_guess`), classifies entries into UI categories (`dir`, `image`, `video`, `audio`, `archive`, `code`, `document`, `text`, `file`), and computes strong `ETag`s (`"<size>-<mtime_sec>-<mtime_nsec>"`) and `Last-Modified` timestamps. |
| **`fs::browser`** | `src/fs/browser.rs` | Executes directory listing and bounded recursive search (`MAX_SEARCH_RESULTS = 5,000`, `MAX_SEARCH_VISITED = 500,000`) inside a single `tokio::task::spawn_blocking` call. Uses case-insensitive natural sorting (`file2` < `file10`) and defers `stat()` calls to the current pagination slice when sorting by name or type. |
| **`server`** | `src/server/mod.rs` | Binds the listening socket (`SO_REUSEADDR`, backlog 1024), configures `hyper_util::server::conn::auto::Builder` (HTTP/1.1 by default, optional HTTP/2 with 8 MiB stream / 32 MiB connection windows), enforces `--max-connections` via a `Semaphore`, performs TLS handshakes (10 s timeout), and coordinates graceful connection draining via `TaskTracker`. |
| **`server::routes`** | `src/server/routes.rs` | Wires the `axum::Router` for `/`, `/browse/{*path}`, `/download/{*path}`, `/api/status`, `/api/files`, `/api/upload`, `/assets/{name}`, `/favicon.ico`, and the `security_headers` middleware. |
| **`server::handlers`** | `src/server/handlers.rs` | Serves embedded HTML/JS/CSS/SVG assets with strict `Content-Security-Policy` headers, `/api/status`, and `/api/files`. |
| **`server::download`** | `src/server/download.rs` | Handles `GET`/`HEAD /download` with RFC 9110 `Range`, `If-Range`, and `If-None-Match` support, safe `?inline=1` content-type filtering, and 256 KiB chunked streaming via `FileStream`. |
| **`server::upload`** | `src/server/upload.rs` | Handles `POST`/`PUT /api/upload` with `X-Share-Upload` CSRF check, streaming body writes to `.share-upload-<pid>-<id>.part`, `TempFile` RAII cleanup on error/abort, and atomic `create_new` + `rename` commit. |
| **`server::response`** | `src/server/response.rs` | Percent-encoding helpers (`RFC 5987` / `RFC 6266` `Content-Disposition` with ASCII fallback and `filename*=UTF-8''...`) and global security headers (`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`). |
| **`metrics`** | `src/metrics/{mod,state,transfer}.rs` | Lock-free atomic byte/connection counters (`Counters`), active/finished transfer registry (`TransferRegistry`), RAII `TransferGuard` and `ConnectionGuard`, and the 500 ms background speed sampler (`spawn_sampler`). |
| **`network`** | `src/network/{mod,interfaces,addresses}.rs` | Queries OS interfaces via `if-addrs`, classifies interface names (`Ethernet`, `Wi-Fi`, `VPN`, `Virtual`, `Loopback`, `Other`), filters unusable addresses, and ranks LAN addresses so physical private IPv4 interfaces appear first. |
| **`tls`** | `src/tls/{mod,certificate}.rs` | Builds `rustls::ServerConfig` (`ring` crypto provider, ALPN `http/1.1` or `h2`+`http/1.1`), loads user PEM files or manages cached self-signed certificates in `~/.config/share/`, and computes colon-separated SHA-256 fingerprints. |
| **`tui`** | `src/tui/{mod,app,events,ui,widgets}.rs` | Ratatui + Crossterm alternate-screen UI: `TerminalGuard` (restores terminal on drop, panic, or double signal), 100 ms tick + async event stream loop, dashboard/log/QR/help views. |
| **`qr`** | `src/qr.rs` | Encodes URLs into QR matrices (`EcLevel::L`) and renders two vertical modules per character cell (`▀`, `▄`, `█`, ` `) with a 4-module quiet zone. |
| **`logging`** | `src/logging.rs` | Custom `tracing_subscriber::Layer` (`BufferLayer`) pushing events into a bounded `LogBuffer` ring buffer for the TUI, paired with an optional stderr `fmt::layer()` when `--no-tui` is active. |
| **`signals`** | `src/signals.rs` | Listens for `SIGINT`, `SIGTERM`, and `SIGHUP`. First signal cancels the `CancellationToken` for graceful shutdown; a second signal restores the terminal and exits immediately with code `130`. |
| **`banner`** | `src/banner.rs` | Renders the plain-text startup banner (URLs, direct download link, TLS SHA-256 fingerprint, LAN trust warning) for `--no-tui` mode. |
| **`web::assets`** | `src/web/assets.rs` | Embeds `assets/web/{index.html,app.js,style.css,favicon.svg}` at compile time using `include_str!`. |
| **`util`** | `src/util.rs` | Pure formatting helpers: `format_bytes` (binary KiB..PiB), `format_speed` (decimal B/s..GB/s), `format_duration`, and Unicode-safe `truncate_middle`. |

---

## Detailed Data & Control Flows

### 1. HTTP Request Flow

```text
TCP accept() ──▶ Semaphore permit (max_connections)
             ──▶ set_nodelay(true)
             ──▶ [optional TlsAcceptor::accept (10s timeout)]
             ──▶ Metrics::connection_opened(peer_ip) -> ConnectionGuard
             ──▶ hyper_util auto::Builder (HTTP/1.1 or HTTP/2, 30s header timeout)
             ──▶ service_fn: inject Extension(ClientAddr(peer_ip))
             ──▶ axum::Router::oneshot(req)
                   ├── middleware: security_headers (nosniff, no-referrer, DENY)
                   └── route handler -> Result<Response, ShareError>
```

1. **Accept & Admission Control**: `server::serve` acquires an owned permit from `Arc<Semaphore>` (`max_connections`, default 1024) *before* calling `listener.accept()`. When the server is at capacity, incoming connections queue in the kernel's TCP listen backlog (`1024`) rather than exhausting file descriptors.
2. **TLS Handshake**: If TLS is enabled, the spawned connection task runs `acceptor.accept(stream)` under a 10-second timeout (`HANDSHAKE_TIMEOUT`). Handshake failures or plain-HTTP requests sent to an HTTPS port are logged at `debug` level and closed cleanly without affecting other connections.
3. **Connection Tracking & Dispatch**: `state.metrics.connection_opened(peer.ip())` increments `total_connections`, `active_connections`, and the per-IP connection count, returning a `ConnectionGuard`. Every HTTP request on that connection receives `ClientAddr(peer.ip())` via request extensions and is dispatched through the `axum::Router`.

---

### 2. Download Streaming Path

```text
GET /download/{*path}
  │
  ├─▶ paths::resolve(root, rel, show_hidden)
  │     └── reject ".." (403), NUL (400), hidden/temp (404), symlink escape (403)
  │
  ├─▶ tokio::fs::File::open + metadata()
  │     └── compute size (u64), ETag ("size-sec-nsec"), Last-Modified, MIME, Content-Disposition
  │
  ├─▶ Check If-None-Match ──(match)──▶ 304 Not Modified (empty body)
  │
  ├─▶ Check Range + If-Range
  │     ├── malformed Range ─────────▶ 400 Bad Request
  │     ├── out-of-bounds Range ─────▶ 416 Range Not Satisfiable (Content-Range: bytes */size)
  │     ├── multi-range / non-bytes ─▶ ignore Range, 200 OK (full file)
  │     ├── valid single Range ──────▶ 206 Partial Content (Content-Range: bytes start-end/size)
  │     └── no Range / stale If-Range▶ 200 OK (full file)
  │
  ├─▶ If Method::HEAD or len == 0 ───▶ return headers + empty body (no transfer registered)
  │
  ├─▶ If start > 0: file.seek(SeekFrom::Start(start))
  │
  └─▶ Metrics::begin_transfer(client, filename, Download, len) -> TransferGuard
        │
        ▼
      ReaderStream::with_capacity(file.take(len), 256 KiB)
        │
        ▼
      FileStream (Stream<Item = io::Result<Bytes>>)
        ├── Poll::Ready(Some(Ok(chunk))) ──▶ guard.add(chunk.len()) [2 relaxed atomic adds]
        ├── Poll::Ready(Some(Err(e)))    ──▶ guard.fail()
        ├── EOF with sent < expected     ──▶ guard.fail() + UnexpectedEof (truncation detected)
        ├── EOF with sent == expected    ──▶ guard.complete()
        └── Drop while sent < expected   ──▶ TransferGuard::drop records TransferStatus::Aborted
```

- **Why 256 KiB chunks (`FILE_CHUNK`)?**: `tokio::fs::File` performs reads on Tokio's blocking threadpool. Reading 256 KiB at a time amortizes the cross-thread wakeup cost while keeping per-connection buffer memory small ($\approx 256\text{ KiB}$ per active download).
- **Back-pressure**: `FileStream` is pulled lazily by `hyper` as socket send buffers drain. A slow client naturally pauses disk reads; a client that disconnects drops `FileStream`, which drops `TransferGuard` and immediately marks the transfer as `Aborted` without reading the rest of the file.

---

### 3. Upload Streaming & Atomic Commit Path

```text
POST|PUT /api/upload?name=<name>&dir=<dir>
  │
  ├─▶ Check config.upload.enabled ────────(false)──▶ 403 UploadsDisabled
  ├─▶ Check header "X-Share-Upload" ──────(absent)─▶ 403 Forbidden (CSRF guard)
  ├─▶ paths::sanitize_upload_name(name) ──(bad)────▶ 400 BadRequest
  ├─▶ Resolve target directory:
  │     ├── --upload-dir set ──▶ use fixed canonical inbox directory
  │     └── otherwise ─────────▶ paths::resolve_dir(root, dir, show_hidden)
  │
  ├─▶ Metrics::begin_transfer(client, name, Upload, content_length.unwrap_or(0))
  │
  ├─▶ OpenOptions::new().write(true).create_new(true)
  │     └── <dir>/.share-upload-<pid>-<transfer_id>.part
  │     └── Arm TempFile RAII guard (unlinks .part on Drop unless disarmed)
  │
  ├─▶ receive():
  │     body.into_data_stream()
  │       ──▶ BufWriter::with_capacity(1 MiB, file)
  │       ──▶ guard.add(chunk.len()) on each chunk
  │       ──▶ writer.flush()
  │       ──▶ verify received == Content-Length (if header was present)
  │
  └─▶ commit():
        For candidate in ["name.ext", "name (1).ext", ... "name (9999).ext"]:
          ├── OpenOptions::new().write(true).create_new(true).open(dest)
          │     ├── AlreadyExists ──▶ try next candidate
          │     └── Ok(reserved)  ──▶ drop(reserved); tokio::fs::rename(tmp, dest)
          │                           disarm TempFile; guard.complete()
          │                           return 201 Created {"name", "size", "renamed"}
```

- **Why `create_new` + `rename`?**: Linux's `renameat2(..., RENAME_NOREPLACE)` is not portable across all filesystems/platforms, and `hard_link` fails on FAT/exFAT USB drives. Opening the destination with `O_CREAT | O_EXCL` (`create_new(true)`) atomically reserves the name on every filesystem, and `rename()` within the same directory atomically replaces the empty reservation with the completed `.part` file.

---

### 4. Metrics Path

```text
Hot Path (per 256 KiB download chunk / upload chunk):
  TransferGuard::add(n)
    ├── Transfer.transferred.fetch_add(n, Ordering::Relaxed)
    └── Counters.bytes_{sent,received}.fetch_add(n, Ordering::Relaxed)

Cold Path (every 500 ms in metrics::spawn_sampler):
  Metrics::sample(dt)
    ├── For each active Transfer in TransferRegistry:
    │     delta = transferred - last_sampled
    │     instant = delta / dt
    │     smoothed = 0.5 * instant + 0.5 * prev_speed
    │     update Transfer.speed and Transfer.peak
    └── For global bytes_sent and bytes_received:
          compute EWMA download_speed, upload_speed, peak_download_speed, peak_upload_speed

Read Path (TUI 10 Hz tick or GET /api/status):
  Metrics::snapshot() -> MetricsSnapshot
    └── Reads atomic counters, clones active/finished TransferInfo and per-client summaries
```

- **Zero lock contention on data transfer**: The `TransferRegistry` `RwLock` is only touched when a transfer starts, finishes, or is snapshotted by the sampler/UI—never while reading or writing chunks.

---

### 5. TUI Update Path

```text
tokio::select! in tui::run:
  ├── tick (every 100 ms) ──▶ App::on_tick()
  │                             ├── app.snapshot = state.metrics.snapshot()
  │                             ├── every 5th tick (500 ms): push speed to sparkline VecDeques
  │                             │                            and refresh logs (recent 500)
  │                             └── expire 4-second status notices
  ├── crossterm EventStream ─▶ events::map_key() -> Action ──▶ App::handle(action)
  └── token.cancelled() ─────▶ exit loop
        │
        ▼
  guard.0.draw(|frame| ui::draw(frame, &app))
```

- `ui::draw` is a pure function of `&App`: it performs no I/O, no system calls, and acquires no locks.
- `TerminalGuard` wraps `ratatui::DefaultTerminal` (`ratatui::try_init()`), which installs a panic hook that restores the terminal before printing panic backtraces, and `TerminalGuard::drop` calls `restore_terminal()` (guarded by an `AtomicBool` so calling it from both the signal handler and normal teardown is idempotent).

---

### 6. HTTPS Setup & Certificate Caching

1. **Modes**: `Config::tls` is `TlsMode::Disabled` (`--http`), `TlsMode::Files { cert, key }` (`--cert` & `--key`), or `TlsMode::SelfSigned` (default).
2. **SAN Discovery (`wanted_names`)**: Collects all advertised LAN IPs plus `localhost`, `127.0.0.1`, `::1`, `<hostname>`, and `<hostname>.local`.
3. **Cache Lookup (`load_or_generate`)**:
   - Checks `${XDG_CONFIG_HOME:-~/.config}/share/{cert.pem,key.pem,names.txt}`.
   - If `cert.pem` is newer than 300 days (`REFRESH_AFTER`) and `wanted_names` is a subset of `names.txt`, the cached certificate and private key are parsed and reused (`CertOrigin::Cached`).
   - Otherwise, `rcgen` generates a new ECDSA self-signed certificate valid for 365 days (`VALIDITY_DAYS`, under Apple's 398-day limit) covering `wanted_names` unioned with up to 32 previously cached names (`MAX_NAMES`).
4. **ALPN**: Offers `["http/1.1"]` by default, or `["h2", "http/1.1"]` when `--http2` is passed.

---

### 7. Network Discovery

1. `network::interfaces::list()` queries OS network interfaces via `if_addrs::get_if_addrs()` and groups IP addresses by interface name.
2. `network::interfaces::kind_label()` classifies each interface by name (`Ethernet` for `en*`/`eth*`, `Wi-Fi` for `wl*`, `VPN` for `tun*`/`wg*`/`tailscale*`/`zt*`, `Virtual` for `docker*`/`veth*`/`br-*`/`virbr*`, `Loopback` for `lo*`).
3. `network::addresses::lan_addresses()` filters out loopback, unspecified, link-local (`169.254.0.0/16`, `fe80::/10`), multicast, and broadcast addresses, then sorts remaining addresses by `(interface_priority, address_range_priority, iface_name)` so RFC 1918 private IPv4 addresses on physical Ethernet and Wi-Fi adapters are listed first.
4. If the machine is offline (no usable non-loopback address), `advertised()` falls back to `127.0.0.1` (`lo`) so the server still provides a working local URL.

---

### 8. Graceful Shutdown Flow

```text
Trigger: 'q' in TUI  OR  first SIGINT / SIGTERM / SIGHUP
  │
  ├─▶ token.cancel() (CancellationToken shared across TUI, server, and sampler)
  │
  ├─▶ TUI exits loop, drops TerminalGuard (leaves raw mode + alternate screen)
  │
  ├─▶ server::serve breaks out of accept loop:
  │     ├── state.set_status(ServerStatus::Stopping)
  │     ├── drop(listener)  [immediately stops accepting new TCP connections]
  │     ├── each active serve_connection task sees shutdown.cancelled()
  │     │     └── calls conn.as_mut().graceful_shutdown() (finishes in-flight HTTP response)
  │     └── tokio::time::timeout(shutdown_timeout, tracker.wait())
  │           └── waits up to --shutdown-timeout (default 5s) for active transfers
  │
  ├─▶ sampler task exits on token.cancelled()
  │
  ├─▶ Any remaining uncompleted upload tasks are dropped with the runtime,
  │   triggering TempFile::drop() to unlink `.share-upload-*.part` files
  │
  └─▶ main prints final transfer summary to stderr and exits with code 0

Second SIGINT / SIGTERM while draining:
  └─▶ signals::spawn calls tui::restore_terminal() and std::process::exit(130)
```

---

## Dependency Table

| Crate | Version | Component(s) | Why Chosen | Alternatives Considered |
| :--- | :--- | :--- | :--- | :--- |
| **`tokio`** | `1.53` | Runtime, `server`, `fs`, `signals`, `metrics` | Industry-standard async runtime with multi-threaded work-stealing scheduler, async timers, TCP networking, signal handling, and blocking threadpool for file I/O. | `async-std`, `smol` (smaller ecosystem compatibility with `hyper` / `axum`). |
| **`tokio-util`** | `0.7` | `app`, `server`, `server::download` | Provides `ReaderStream` (adapting `AsyncRead` into a `Bytes` `Stream`), `CancellationToken` (structured shutdown), and `TaskTracker` (connection draining). | Hand-rolled `Stream` state machines and `Notify`/`AtomicUsize` waitgroups. |
| **`futures-util`** | `0.3` | `server::download`, `server::upload`, `tui` | `Stream` trait and `StreamExt::next` combinator for body and terminal event streams. | `tokio-stream` (lacks some general `Stream` combinators). |
| **`bytes`** | `1` | `server::download`, `server::upload` | Reference-counted byte buffer (`Bytes`) used across `hyper`, `axum`, and `ReaderStream` to pass chunks without copying. | `Vec<u8>` (requires cloning or ownership transfer overhead in HTTP bodies). |
| **`axum`** | `0.8` | `server`, `error` | Ergonomic, macro-free routing, extractors (`Path`, `Query`, `State`, `Extension`), and `IntoResponse` error conversion built directly on `hyper` and `tower`. | `actix-web` (uses its own single-threaded runtime model), `warp` (heavy trait-type complexity). |
| **`hyper`** | `1` | `server` | Low-level HTTP/1.1 and HTTP/2 server implementation with back-pressure and fine-grained flow-control window tuning. | Using `axum::serve` directly (does not expose per-connection HTTP/1.1 header timeouts or custom HTTP/2 window sizes alongside TLS). |
| **`hyper-util`** | `0.1` | `server` | Bridges `tokio` I/O (`TokioIo`, `TokioExecutor`, `TokioTimer`) with `hyper` 1.x's `auto::Builder` for serving HTTP/1.1 and HTTP/2 over the same listener. | Separate HTTP/1 and HTTP/2 servers. |
| **`tower`** | `0.5` | `server` | Provides `ServiceExt::oneshot` to dispatch `hyper` requests into the cloned `axum::Router`. | Manual `tower_service::Service` polling boilerplate. |
| **`httpdate`** | `1` | `server::download` | Fast, zero-dependency RFC 7231 HTTP-date formatting and parsing for `Last-Modified` and `If-Range` headers. | `chrono` or `time` HTTP date formatting (larger API surface for a single header format). |
| **`percent-encoding`** | `2` | `server::response` | RFC 5987 / RFC 6266 percent-encoding for `Content-Disposition` `filename*` and URL path segments. | `urlencoding` (less flexible custom `AsciiSet` control). |
| **`mime_guess`** | `2` | `fs::metadata`, `server::download` | Static compile-time table mapping file extensions to MIME types (`Mime`). | `infer` (inspects magic bytes via extra disk reads instead of filename extension). |
| **`tokio-rustls` & `rustls`** | `0.26` / `0.23` | `tls`, `server` | Modern, memory-safe TLS 1.2/1.3 implementation in Rust with ALPN negotiation. | `native-tls` / `openssl` (C build dependency, harder cross-compilation and static linking). |
| **`rcgen`** | `0.14` | `tls::certificate` | Pure-Rust X.509 certificate generator for creating self-signed certificates with IP and DNS Subject Alternative Names at startup. | Invoking the `openssl` CLI binary (violates the zero-external-runtime-dependency requirement). |
| **`ring`** | `0.17` | `tls`, `tls::certificate` | Fast, audited cryptographic primitives powering `rustls`, `rcgen`, and SHA-256 certificate fingerprinting. | `aws-lc-rs` (larger C/CMake build footprint). |
| **`time`** | `0.3` | `tls::certificate` | Used with `rcgen` to set `not_before` and `not_after` certificate validity timestamps. | `chrono` (`rcgen` natively uses `time::OffsetDateTime`). |
| **`ratatui`** | `0.30` | `tui` | Immediate-mode terminal UI library with layout constraints, sparklines, tables, popups, and `TestBackend` for headless unit testing. | `cursive` (callback-heavy architecture), plain ANSI escape codes. |
| **`crossterm`** | `0.29` | `tui` | Cross-platform raw-mode terminal control and async `EventStream` for keyboard and resize events. | `termion` (Unix-only, lacks native async `EventStream` integration with Ratatui). |
| **`qrcode`** | `0.14` | `qr` | Pure-Rust QR matrix encoder used to render half-block ANSI/TUI QR codes offline. | `qrencode` C bindings or external web APIs (violates offline requirement). |
| **`open`** | `5` | `main`, `tui::app` | Opens the share URL in the system's default web browser (`--open` flag and `o` key in TUI) using `that_detached`. | `webbrowser` crate (heavier dependency tree). |
| **`clap` & `clap_complete`** | `4` | `cli`, `main` | Derive-based command-line parser with declarative flag conflict/requirement validation and shell completion generation. | `argh` or `lexopt` (lack declarative conflict matrix and built-in shell completions). |
| **`serde` & `serde_json`** | `1` | `fs::browser`, `metrics`, `server` | Serialization and deserialization for JSON API responses (`/api/files`, `/api/status`, `/api/upload`, error payloads) and query parameters. | Manual JSON string formatting (error-prone escaping for arbitrary filenames). |
| **`thiserror`** | `2` | `error` | Derive macro for `std::error::Error` and `Display` on `ShareError`. | `anyhow` (erases concrete error variants needed to map errors to HTTP status codes and hints). |
| **`tracing` & `tracing-subscriber`** | `0.1` / `0.3` | `logging`, all modules | Structured, level-filtered instrumentation routed simultaneously to the in-memory `LogBuffer` layer and (in `--no-tui` mode) stderr. | `log` + `env_logger` (harder to compose a custom in-memory ring-buffer layer for the TUI alongside stderr). |
| **`if-addrs`** | `0.15` | `network::interfaces` | Queries the OS kernel (`getifaddrs`) for all network interfaces and their IPv4/IPv6 addresses without spawning `ip` or `ifconfig`. | `local-ip-address` or `pnet_datalink` (heavier or less control over enumerating and ranking all interfaces). |
| **`tempfile`** *(dev)* | `3` | Unit & integration tests | Creates isolated, auto-cleaned temporary directories for filesystem, symlink, TLS, and upload tests. | Manual `/tmp` directory management (risks test collisions and leaked files). |
| **`reqwest`** *(dev)* | `0.12` | Integration tests (`tests/*`) | Async HTTP/HTTPS client with `rustls-tls` and `stream` support for testing downloads, chunked uploads, ranges, and TLS certificate validation. | `hyper` client boilerplate in tests. |

---

## What `cargo test`, `cargo clippy`, and `cargo fmt --check` Verify

1. **`cargo test`**:
   - **Unit tests (`src/**`) — 74 tests**: Validates CLI flag conflict rules, configuration defaults, path cleaning and `..`/NUL/dotfile/symlink-escape rejection, upload filename sanitization and collision naming (`report (1).pdf`), natural sorting (`a2` < `a10`), pagination, recursive search, MIME and file-kind detection, ETag generation, RFC 9110 `Range` / `If-Range` / `If-None-Match` parsing (including 64-bit offsets $>4\text{ GiB}$), inline content-type safety (blocking HTML and SVG), LAN interface classification and priority ranking, self-signed certificate generation and PEM parsing, QR rendering, log ring-buffer eviction, atomic transfer metrics and EWMA speed decay, and TUI rendering across terminal sizes (`1x1` through `200x60`) via Ratatui's `TestBackend`.
   - **Integration tests (`tests/{integration,downloads,uploads}.rs`) — 41 tests**: Starts the full server in-process on `127.0.0.1:0` and tests end-to-end HTTP/HTTPS behavior over real TCP sockets: UI asset serving and CSP/security headers, `/api/status` and `/api/files` sorting/filtering/pagination, 17 raw-socket directory traversal encodings, Unix symlink escape blocking, single-file share mode, full and partial (`206`) downloads, byte-exact download resume, `416 Range Not Satisfiable` and `400 Bad Request` ranges, `5 GiB` sparse file 64-bit range reads, 10 concurrent downloads, client disconnect abort detection, live speed sampling, atomic uploads, `X-Share-Upload` CSRF enforcement, 48 MiB chunked streaming upload without `Content-Length`, 8 concurrent uploads, aborted upload `.part` file cleanup, `--upload-dir` redirection, user-supplied TLS certificates, invalid certificate error reporting, port-in-use hints, and graceful shutdown.
2. **`cargo clippy --all-targets -- -D warnings`**:
   - Runs the Clippy static analyzer over the library, the binary, and all unit/integration test targets with `-D warnings` (turning any lint warning into a compilation error). Checks for correctness bugs, suspicious type conversions, unidiomatic Rust patterns, redundant clones/allocations, and API misuse.
3. **`cargo fmt --check`**:
   - Runs `rustfmt` in verification mode across every `.rs` file in `src/` and `tests/`, failing with a diff if any file deviates from standard Rust formatting rules.
