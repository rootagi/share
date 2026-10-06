# `share`

Terminal-native HTTP(S) & WebDAV file server and live transfer monitor for local networks, written in Rust.

Point `share` at any directory or file from your shell to expose it immediately over HTTPS or HTTP. The host gets a real-time `ratatui` terminal dashboard (throughput sparklines, active transfer gauges, connected client table, QR overlay, and ring-buffered log viewer), while other machines and phones on the subnet browse, preview in an interactive media lightbox, stream folder archives (`.zip`, `.tar.gz`, `.tar`), download, upload, or mount as a native network drive (`dav://host:port`) using a standard web browser, `curl`, `wget`, or OS file manager.

```text
╭─ SHARE ─────────────────────────────────────────────────────────── HTTPS ● RUNNING ─╮
│ Sharing   ~/Releases  folder · uploads on                                           │
│ URL       ▸ https://192.168.1.15:8080   wlan0 · Wi-Fi                               │
│             https://10.0.0.42:8080      eth0 · Ethernet                             │
│ WebDAV      dav://192.168.1.15:8080     Finder / Explorer / GNOME Files             │
│ TLS       self-signed · SHA-256 8F:3A:19:C2:4B:10:7E:D1…                            │
│           LAN TRUST MODE  anyone who can reach this server can read the shared files│
├─ TRANSFERS ─────────────────────────────┬─ NETWORK ─────────────────────────────────┤
│ ↓ ubuntu-24.04-desktop-amd64.iso        │ ↓  112.4 MB/s                             │
│ ████████████░░░░░░░░  62%  112.4 MB/s   │ ↑    0.0 B/s                              │
│ 2.91 GiB / 4.70 GiB   192.168.1.21      │ ▁▂▄▆████████▇▆▇███                        │
│                                         │ ░░░░░░░░░░░░░░░░░░                        │
│ RECENT                                  │ Clients   1  (2 conn)                     │
│ ✓ ↑ notes.pdf          1.42 MiB  28 MB/s│ Peak      ↓ 118.1 MB/s  ↑ 28.4 MB/s       │
│ ✓ ↓ checksums.txt       412 B    1.2 MB/s│ Sent      2.91 GiB                        │
├─ CLIENTS ───────────────────────────────┴───────────────────────────────────────────┤
│ 192.168.1.21     ↓  ubuntu-24.04-desktop-amd64.iso                       112.4 MB/s │
│ [O] Open  [Y] Copy  [P] QR  [U] Upload:ON  [X] Kill  [R] Refresh  [L] Logs  [Q] Quit│
╰─────────────────────────────────────────────────────────────────────────────────────╯
```

---

## Synopsis

```bash
share [OPTIONS] <PATH>
share --completions <SHELL>
```

```bash
# Serve a directory over HTTPS on port 8080 (default; also accepts plain HTTP & WebDAV on the same port)
share ~/Downloads

# Serve a single file (prints both the web UI URL and a direct /download/<name> URL)
share ./debian-12.8.0-amd64-netinst.iso

# Enable browser/CLI uploads, recursive subfolder search, and startup QR modal
share ~/Drop --upload --recursive --qr

# Protect with a random capability URL and 6-digit PIN, auto-expiring after 30 minutes or 5 downloads
share ./confidential --random-url --pin 123456 --expire 30m --max-downloads 5

# Cap global transfer bandwidth at 20 MiB/s
share ./backups --rate-limit 20MiB

# Serve a single file while routing incoming uploads into a separate inbox folder
share ./handout.pdf --upload-dir ~/Inbox

# Plain HTTP on port 9000, headless mode (stdout banner + stderr logs)
share /srv/iso --http --port 9000 --no-tui

# Bind strictly to a specific network interface's IPv4 address
share . --interface wlan0
```

---

## Design Overview

- **Dual-protocol HTTPS + HTTP listener on a single port**: In default HTTPS mode, `share` peeks at the first byte of each incoming TCP stream (`0x16` TLS ClientHello vs. plain ASCII HTTP/WebDAV verbs). Browser HTTPS sessions, plain-HTTP CLI tools, and unencrypted OS WebDAV clients (`dav://host:8080`) all work seamlessly on the same port without extra configuration.
- **Standard HTTP/1.1 & HTTP/2 semantics**: Built on `axum`, `hyper`, and `tokio-rustls`. Supports RFC 9110 byte-range requests (`206 Partial Content`), `If-Range`, `If-None-Match` (`304 Not Modified`), and `HEAD` so command-line tools (`curl -C -`, `wget -c`), download managers, and browser media players can seek and resume interrupted downloads at arbitrary 64-bit (`u64`) offsets.
- **On-the-fly streaming folder archives**: Any shared directory or subfolder can be downloaded as a `.zip`, `.tar.gz`, or `.tar` stream (`/archive/<path>?format=zip|tar.gz|tar`). Archives are generated on the fly over a bounded in-memory duplex channel with zero temporary files on disk.
- **Constant-memory streaming & token-bucket rate limiting**: Downloads stream from `tokio::fs::File` in 256 KiB chunks (`ReaderStream`); uploads stream through a 1 MiB `BufWriter`. Optional `--rate-limit <RATE>` paces all active streams smoothly across connections.
- **Atomic, non-destructive & resumable uploads**: When uploads are enabled (`--upload`, `--upload-dir`, or toggled live via `[U]` in the TUI), raw `POST`/`PUT` bodies to `/api/upload` are staged to a hidden `.part` file in the destination directory and committed via `O_CREAT | O_EXCL` (`create_new`) reservation plus atomic `rename()`. Existing files are never overwritten (`report (1).pdf`, `report (2).pdf`, …). Interrupted uploads with file size/mtime metadata retain their `.part` file so browsers or CLI clients can query `GET /api/upload/status` and resume from the exact byte offset via `POST /api/upload?offset=<N>`.
- **Zero-dependency RFC 4918 WebDAV server**: Mount the share directly in Linux GNOME Files (`dav://host:port`), macOS Finder (`Cmd+K`), or Windows Explorer (`\\host@port\dav`) with full read/write support (`OPTIONS`, `PROPFIND`, `GET`, `HEAD`, `PUT`, `MKCOL`, `DELETE`, `MOVE`, `COPY`, `LOCK`, `UNLOCK`).
- **Self-contained single binary**: The web UI (`index.html`, `style.css`, `app.js`, `favicon.svg`) is compiled into the binary via `include_str!` with zero external fonts, scripts, or runtime dependencies.

---

## Building & Installing

### Toolchain Requirements

- Rust **1.85+** (2024 Edition) and Cargo.
- No C/GUI libraries, Node.js, Python, or system OpenSSL packages are required at build time or runtime.

### Compile from Source

```bash
# Optimized release binary -> ./target/release/share
cargo build --release

# Install into ~/.cargo/bin/share
cargo install --path .
```

### Shell Completions

Generate tab-completion scripts for your shell using `--completions`:

```bash
share --completions bash       > ~/.local/share/bash-completion/completions/share
share --completions zsh        > ~/.zfunc/_share
share --completions fish       > ~/.config/fish/completions/share.fish
share --completions elvish     > ~/.config/elvish/lib/share.elv
share --completions powershell > share.ps1
```

### Running as a `systemd` User Service

An example hardened service unit is included at [`examples/share.service`](examples/share.service). When invoked without an interactive terminal, `share` automatically disables the TUI and writes structured logs to `stderr` for `journald`:

```bash
mkdir -p ~/.config/systemd/user
cp examples/share.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now share
journalctl --user -u share -f
```

---

## Command-Line Options

| Flag / Option | Short | Default | Description |
| :--- | :--- | :--- | :--- |
| `<PATH>` | | *(required unless `--completions`)* | File or directory to serve. Canonicalized at startup. |
| `--port <PORT>` | `-p` | `8080` | TCP port to listen on (`0` lets the kernel pick an ephemeral port). |
| `--bind <ADDRESS>` | `-b` | `0.0.0.0` | IPv4 or IPv6 address to bind. Conflicts with `--interface`. |
| `--interface <NAME>` | `-i` | | Bind the first usable IPv4 address on interface `<NAME>` (e.g. `eth0`, `wlan0`). Conflicts with `--bind`. |
| `--http` | | `false` | Serve unencrypted HTTP instead of HTTPS. Conflicts with `--tls`, `--cert`, `--key`. |
| `--tls` | | `true` (implicit) | Serve HTTPS (default behavior; flag exists for explicit scripts). |
| `--cert <PATH>` | | | PEM certificate chain file (requires `--key`). |
| `--key <PATH>` | | | PEM private key file (requires `--cert`). |
| `--upload` | `-u` | `false` | Permit clients to upload files into the browsed directory (can also be toggled live with `[U]` in the TUI). |
| `--upload-dir <PATH>` | | | Route all uploads into `<PATH>` (implies `--upload`; created if absent; required when sharing a single file with uploads). |
| `--read-only` | | `true` (implicit) | Refuse uploads (default; conflicts with `--upload` and `--upload-dir`). |
| `--recursive` | `-r` | `false` | Enable recursive subdirectory search in `/api/files?recursive=true` (`or /api/list`) and the browser UI. |
| `--hidden` | | `false` | List and serve dotfiles and dot-directories. Temporary `.share-upload-*.part` files remain hidden even with `--hidden`. |
| `--random-url` | | `false` | Generate a random 12-character secret capability URL prefix (`/s/<random>`). Conflicts with `--token`. |
| `--token <TOKEN>` | | | Require a secret token prefix in the URL path (`/s/<TOKEN>`); pass `"random"` to generate one. Requests outside `/s/<TOKEN>` receive `404 Not Found`. |
| `--auth <USER:PASS>` | | | Require HTTP Basic / session authentication (`USER:PASS` or a password). Conflicts with `--pin`. |
| `--pin <PIN>` | | | Require a PIN code to access the share (unlocks via the Web UI modal or HTTP Basic Auth `curl -u :<PIN>`). |
| `--max-downloads <N>` | | | Automatically shut down after `<N>` completed full-file or archive downloads (partial media range scrubs and `HEAD` probes do not count). |
| `--expire <DURATION>` | | | Automatically shut down after a duration (e.g. `30s`, `15m`, `2h`, `1d`). |
| `--rate-limit <RATE>` | | | Cap global combined download + upload throughput (e.g. `500K`, `5M`, `10MiB`, `1G`). |
| `--open` | | `false` | Launch the local default web browser pointing at the primary share URL on startup. |
| `--no-tui` | | `false` | Disable the Ratatui interface; print a startup summary to `stdout` and log events to `stderr`. Auto-enabled when `stdin` or `stdout` is not a TTY. |
| `--qr` | | `false` | Display the share URL as a QR code at startup (opens the QR overlay in TUI mode or prints an ANSI half-block QR code in `--no-tui` mode). |
| `--quiet` | `-q` | `false` | Emit only warnings and errors (`share=warn`). Conflicts with `--verbose`. |
| `--verbose` | `-v` | `0` | Increase log detail: `-v` enables `share=debug`, `-vv` enables `trace` across all crates. |
| `--max-connections <N>` | | `1024` | Cap concurrent TCP connections via an accept-loop semaphore ($\ge 1$). |
| `--workers <N>` | | Logical CPUs | Number of Tokio worker threads ($\ge 1$). |
| `--name <NAME>` | | Basename of `<PATH>` | Override the share title displayed in the TUI and browser header. |
| `--http2` | | `false` | Advertise HTTP/2 (`h2`) via ALPN alongside HTTP/1.1. HTTP/1.1 is the default because single large LAN streams avoid HTTP/2 flow-control framing overhead. |
| `--shutdown-timeout <SECS>` | | `5` | Seconds to wait for in-flight transfers to finish during graceful shutdown before closing active connections. |
| `--completions <SHELL>` | | | Emit shell completion script (`bash`, `zsh`, `fish`, `elvish`, `powershell`) and exit. |
| `--help` | `-h` | | Print command help. |
| `--version` | `-V` | | Print version. |

---

## HTTPS, Self-Signed Certificates & Dual-Protocol Fallback

Unless `--http` is passed, `share` serves HTTPS via `rustls` (backed by `ring`) while simultaneously accepting plain HTTP on the same TCP port.

1. **Automatic SAN generation and caching**:
   - On startup without `--cert`/`--key`, `share` enumerates the host's usable LAN IP addresses and combines them with `localhost`, `127.0.0.1`, `::1`, `<hostname>`, and `<hostname>.local`.
   - It checks `${XDG_CONFIG_HOME:-~/.config}/share/` for cached `cert.pem`, `key.pem`, and `names.txt`.
   - If `cert.pem` is younger than 300 days and its Subject Alternative Names (SANs) already cover all current addresses, the cached certificate is reused so browsers only prompt once across restarts.
   - Otherwise, `rcgen` generates a fresh 365-day certificate (preserving up to 32 previously cached SANs so switching between home and office networks does not invalidate earlier exceptions) and writes `key.pem` with `0600` permissions inside a `0700` directory.
2. **Same-port HTTP & WebDAV fallback**:
   - The listener inspects the first byte on each accepted socket without consuming it. Connections starting with `0x16` (TLS handshake) are routed through `tokio-rustls`; plain-HTTP connections (`GET`, `PROPFIND`, `OPTIONS`, etc.) are served directly over HTTP on the same port.
3. **First-visit browser warning**:
   - Because the auto-generated certificate is self-signed, browsers display a certificate authority warning on first visit. Choose **Advanced → Proceed** (or **Accept the Risk and Continue**).
   - You can verify the server's **SHA-256 certificate fingerprint** against the fingerprint shown in the TUI (`?` help modal or dashboard header) and `--no-tui` startup banner.
4. **CLI clients (`curl` / `wget`)**:
   ```bash
   # Skip CA verification with -k, pin the cached certificate, or use plain http:// on the same port
   curl -k -O https://192.168.1.15:8080/download/archive.tar.zst
   curl --cacert ~/.config/share/cert.pem -O https://127.0.0.1:8080/download/archive.tar.zst
   curl -O http://192.168.1.15:8080/download/archive.tar.zst
   ```

---

## Streaming Folder Archives (`.zip`, `.tar.gz`, `.tar`)

Directories can be downloaded as a single archive stream with zero temporary disk usage:

- **Formats**: `.zip` (`?format=zip` or `.zip` suffix), `.tar.gz` (`?format=tar.gz` or `.tar.gz` suffix, default), and `.tar` (`?format=tar` or `.tar` suffix).
- **Web UI**: Click the **`▾ Download`** button in the top toolbar to archive the current folder, or click **`▾ Download`** on any subfolder row to choose `.zip`, `.tar.gz`, or `.tar`.
- **CLI Examples**:
  ```bash
  # Download a subfolder as a streaming .zip, .tar.gz, or .tar archive
  curl -k -OJ "https://127.0.0.1:8080/archive/sub?format=zip"
  curl -k -OJ "https://127.0.0.1:8080/archive/sub?format=tar.gz"
  curl -k -OJ "https://127.0.0.1:8080/archive/sub?format=tar"
  ```

---

## Uploading Files, Recursive Folder Drop & Resumable Staging

Uploads are enabled with `--upload`, `--upload-dir <PATH>`, or at runtime by pressing `[U]` in the TUI.

### Wire Semantics & CSRF Guard

- **Routes**:
  - `POST /api/upload?name=<filename>&dir=<relative_dir>&mkdir=<bool>&size=<bytes>&mtime=<ms>&offset=<bytes>`
  - `PUT /api/upload?name=<filename>&dir=<relative_dir>&mkdir=<bool>`
  - `GET /api/upload/status?name=<filename>&dir=<relative_dir>&size=<bytes>&mtime=<ms>` (also `HEAD /api/upload`)
- **Payload**: Raw file bytes in the HTTP request body (streamed directly to disk without multipart boundary parsing).
- **CSRF Protection**: Browser `POST` requests require the `X-Share-Upload: 1` header (triggering a CORS preflight on cross-origin attempts, which `share` never grants), and any request carrying `Sec-Fetch-Site: cross-site` is rejected with `403 Forbidden`. Direct CLI `PUT` uploads (`curl -T file.bin`) work out of the box.
- **Recursive Folder Drag-and-Drop (`mkdir=true`)**:
  - Dropping one or more folders into the browser UI traverses the full directory tree (`webkitGetAsEntry`) and uploads each file with `?dir=<relative/subpath>&mkdir=true`, automatically creating nested subdirectories inside the share root while enforcing strict path-traversal containment.
- **Resumable `.part` Staging & Atomic Commit**:
  1. When `size` is provided, bytes stream into a deterministic `.share-upload-resume-<hash>.part` file in the target directory; if a connection drops mid-transfer, the partial data is preserved on disk.
  2. Re-selecting the same file in the browser (or querying `GET /api/upload/status?name=...&size=...&mtime=...`) returns `{"offset": <N>}` (and header `Upload-Offset: <N>`). The client then sends only the remaining bytes via `POST /api/upload?...&offset=<N>` (or header `X-Share-Offset: <N>`).
  3. While in flight, `.part` files are excluded from `/api/files` (`/api/list`) and return `404 Not Found` on `/download`.
  4. On completion, `share` reserves a non-colliding filename (`name.ext`, `name (1).ext`, `name (2).ext`, … up to `9999`) via `OpenOptions::create_new(true)` and atomically `rename()`s the `.part` file over the reservation.

### `curl` Upload Examples

```bash
# Upload a file using curl -T (PUT)
curl -k -T file.bin "https://127.0.0.1:8080/api/upload?name=file.bin"

# Upload a file using POST with X-Share-Upload header
curl -k -X POST \
  -H 'X-Share-Upload: 1' \
  --data-binary @backup.tar.gz \
  'https://127.0.0.1:8080/api/upload?name=backup.tar.gz'

# Query resumable byte offset and resume an interrupted upload from offset N
curl -k "https://127.0.0.1:8080/api/upload/status?name=large.iso&size=104857600&mtime=1700000000"
curl -k -X POST \
  -H 'X-Share-Upload: 1' \
  --data-binary @remaining.part \
  "https://127.0.0.1:8080/api/upload?name=large.iso&size=104857600&mtime=1700000000&offset=52428800"
```

Response (`201 Created`):

```json
{"name":"file.bin","size":1048576,"renamed":false}
```

---

## WebDAV Native OS Mounting (`dav://host:port`)

`share` includes a built-in RFC 4918 Class 1 & 2 WebDAV server mounted at both `/dav` and the root `/` (when accessed by WebDAV clients). When `--upload` is enabled (or toggled on with `[U]`), clients can create folders, upload, rename, move, copy, lock, and delete files directly from their native file manager:

- **Linux (GNOME Files / Nautilus / `gvfsd-dav`)**:
  ```bash
  gio mount dav://192.168.1.15:8080
  # or explicitly at /dav:
  gio mount dav://192.168.1.15:8080/dav
  ```
- **macOS Finder**:
  Press `Cmd+K` (**Connect to Server**) and enter:
  ```text
  http://192.168.1.15:8080/dav
  ```
- **Windows Explorer**:
  Map Network Drive or enter in the Explorer address bar:
  ```text
  \\192.168.1.15@8080\dav
  ```

---

## Terminal QR Code Rendering

`share` generates QR codes completely offline (`qrcode` crate at `EcLevel::L`) and renders two vertical modules per character cell using Unicode half-blocks (`▀`, `▄`, `█`, ` `) with explicit black-on-white ANSI escape sequences (`\x1b[30;47m`) and a 4-module quiet zone.

- Pass `--qr` at launch to open the QR modal immediately in the TUI (or print the QR block beneath the startup banner in `--no-tui` mode).
- Press `p` in the TUI at any time to toggle the QR modal, and `Tab` to switch which interface URL (`wlan0`, `eth0`, `tailscale0`, etc.) is encoded.
- When `--random-url` or `--token <TOKEN>` is used, the QR code embeds the full capability URL (`/s/<TOKEN>`).
- When serving a single file (`share ./file.apk --qr`), the QR code encodes the direct `/download/file.apk` URL so scanning starts the download immediately.

---

## Terminal UI Keybindings

| Key | Action |
| :--- | :--- |
| `q`, `Q`, `Ctrl+C`, `Ctrl+D` | Initiate graceful shutdown and restore the terminal |
| `o`, `O` | Open the selected share URL in the local default browser |
| `y`, `Y` | Copy the selected share URL on the **Dashboard** (or copy all server log lines in the **Logs** view) to the system clipboard across Wayland (`wl-copy`), X11 (`xclip`/`xsel`), macOS (`pbcopy`), Windows (`clip.exe`), and remote SSH (`OSC 52`) |
| `u`, `U` | Live-toggle client uploads **ON** / **OFF** without restarting the server (immediately updates `/api/list`, `/api/status`, WebDAV write permissions, and the Web UI upload panel) |
| `x`, `X`, `Delete` | Cancel/disconnect the currently selected active transfer in the `TRANSFERS` panel |
| `p`, `P` | Toggle the QR code popup for the selected URL |
| `Tab` | Cycle through detected network interface URLs |
| `r`, `R` | Re-query OS network interfaces and refresh advertised URLs |
| `l`, `L` | Switch between the **Dashboard** view and the **Logs** view |
| `c`, `C` | Clear completed/aborted entries from the recent transfers list |
| `?`, `h`, `F1` | Toggle the **Help** popup (displays full key list and TLS SHA-256 fingerprint) |
| `↑` / `k`, `↓` / `j` | Select active transfers / scroll recent transfers (Dashboard) or scroll log entries (Logs) by 1 line |
| `PgUp`, `PgDn` | Scroll transfers or log entries by 10 lines |
| `Esc` | Dismiss the active popup (`Help` or `QR`) |

---

## Browser Interface & Media Lightbox

Navigating to `/` or `/browse/<path>` (or `/s/<token>/` when capability URLs are enabled) serves the embedded web UI:

- **Directory ledger**: Lists folders first with case-insensitive natural sort (`part2.bin` before `part10.bin`), file-kind color rails, proportional file-size bars, and per-folder **`▾ Download`** archive menus (`.zip`, `.tar.gz`, `.tar`).
- **In-page Media Lightbox**: Clicking any image, video, audio track, or text/source code file opens an interactive modal lightbox with native media playback, syntax/monospace text preview (up to 256 KiB), **`←` / `→` keyboard navigation** between previewable items in the folder, direct download button, and **`Esc` to close**.
- **Live filter & recursive search**: Filters the current directory as you type; when the server is started with `--recursive`, toggling **Include subfolders** searches the directory tree (bounded to 5,000 matches / 500,000 visited nodes per query).
- **Sort & infinite scroll**: Sort ascending or descending by `Name`, `Size`, `Modified`, or `Type`. Pages of 200 items load automatically via `IntersectionObserver`.
- **Safe inline preview (`?inline=1`)**: Images (except SVG), audio, video, PDFs, and text/code files are served inline for the lightbox or new-tab preview. HTML/XML files are served as `text/plain; charset=utf-8` when previewed inline, and `image/svg+xml` is forced to `attachment`, preventing script execution inside the origin.
- **Drag-and-drop files & recursive folders**: Drop individual files or entire nested folder trees onto the window, or use the **Upload** button. Up to 3 concurrent streams run with per-file progress bars, live byte rates, individual cancel buttons, and automatic `.part` offset resume on retry.

---

## Security, Capability URLs & Ephemeral Modes

By default `share` starts in **LAN Trust Mode** for zero-friction local sharing, and provides layered security controls when sharing on larger or semi-trusted networks:

1. **Secret Capability URLs (`--random-url` / `--token <TOKEN>`)**:
   - Mounts the entire web UI, REST API (`/api/list`, `/api/files`, `/api/status`, `/api/upload`), file downloads, archive streams, and WebDAV tree under `/s/<token>/...`.
   - Any request outside `/s/<token>` (including `GET /` and `GET /api/list`) returns an indistinguishable `404 Not Found`.
2. **Password & PIN Authentication (`--auth <USER:PASS>` / `--pin <PIN>`)**:
   - Unauthenticated CLI requests receive `401 Unauthorized` (`WWW-Authenticate: Basic`); authenticating with `curl -u admin:secret` or `curl -u :123456` succeeds directly.
   - Browsing to the share displays an in-page Password/PIN modal that exchanges credentials via `POST /api/auth` (constant-time byte comparison) for an `HttpOnly; SameSite=Lax` session cookie (`share_auth`).
3. **Ephemeral Limits (`--max-downloads <N>` & `--expire <DURATION>`)**:
   - `--max-downloads <N>` tracks completed full-file and folder-archive downloads and triggers a clean shutdown as soon as the quota is reached. Partial `Range` requests (such as browser video/audio scrubbing), inline previews (`?inline=1`), `HEAD` probes, and aborted transfers do not consume the download quota.
   - `--expire <DURATION>` (`30s`, `15m`, `2h`, etc.) shuts the server down automatically when the timer elapses.
4. **Strict Path & Symlink Containment**:
   - Client paths are split on `/`; `..` segments are rejected immediately with `403 Forbidden`, NUL bytes return `400 Bad Request`, and every candidate path is `canonicalize()`d and verified with `starts_with(&canonical_root)`.
   - Symlinks resolving outside the canonical share root are omitted from listings and rejected with `403 Forbidden` on direct access.
5. **Dotfile & `.part` Isolation**:
   - Hidden files/folders starting with `.` return `404 Not Found` and are omitted from listings unless `--hidden` is passed. In-progress `.share-upload-*.part` files are never listed or served under any flag combination.
6. **Response Hardening**:
   - Every HTTP response sets `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and `X-Frame-Options: DENY`, and the browser UI is served with `Content-Security-Policy: default-src 'self'; ...`.

---

## Performance Characteristics

- **No whole-file buffering**: Downloads stream in 256 KiB reads (`FILE_CHUNK`); uploads stream via a 1 MiB `BufWriter`. Neither kernel `sendfile(2)` nor zero-copy is claimed (user-space TLS in `rustls` and `hyper`'s `Body` stream operate on `Bytes` buffers in user space).
- **Lock-free transfer hot path**: Every chunk read or written executes only two `Ordering::Relaxed` `AtomicU64::fetch_add` operations. A single background task samples byte deltas every 500 ms to compute exponentially smoothed speeds ($\alpha = 0.5$), keeping both HTTP worker threads and the 10 Hz TUI loop free of mutex contention.
- **Lazy `stat()` on directory listings**: Directory scans run inside a single `spawn_blocking` call. When sorting by name or extension, `readdir`'s `d_type` classifies directories vs. files without calling `stat()`, and `fs::metadata` is invoked only for the 200 entries on the requested page.

For loopback benchmarks (single-stream 8 GiB sparse transfer, `1/2/5/10` concurrent client scaling with `scripts/bench.sh`, and `/proc/<pid>/status` `VmRSS`/`VmHWM` measurements) and bottleneck analysis, see [`docs/BENCHMARKING.md`](docs/BENCHMARKING.md). For internal data flows and module structure, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Verification & Quality Checks

- **`cargo test --locked`**: Builds and runs unit tests in `src/` and integration tests in `tests/{integration,downloads,uploads}.rs` against an in-process server bound to `127.0.0.1:0`. Covers CLI flag validation, natural sort order, path traversal encodings, symlink escape refusal, RFC 9110 `Range` / `If-Range` / `If-None-Match` / `HEAD` responses, $>4\text{ GiB}$ 64-bit offsets, byte-exact resumed downloads, streaming `.zip`/`.tar.gz`/`.tar` archives, resumable `.part` uploads, recursive folder uploads, WebDAV methods, capability URLs, PIN/Basic auth, ephemeral quotas, rate limiting, TLS handshakes, and headless TUI rendering via `ratatui::backend::TestBackend`.
- **`cargo clippy --all-targets --locked -- -D warnings`**: Runs Clippy static analysis across the library, binary, and all test targets with warnings promoted to errors.
- **`cargo fmt --all -- --check`**: Verifies that all Rust source and test files conform to `rustfmt` style.

---

## Troubleshooting

| Symptom / Error | Cause & Resolution |
| :--- | :--- |
| `port 8080 is already in use on 0.0.0.0` | Another process is bound to TCP port 8080. Pass `-p 0` for a random free port or `-p 9000` to pick another port. |
| `ports below 1024 need elevated privileges` | Binding privileged ports (`< 1024`) requires `CAP_NET_BIND_SERVICE` or root. Use a port $\ge 1024$. |
| `network interface 'wlan0' was not found or has no usable IPv4 address` | Check active interface names with `ip -br addr`, or bind by IP with `--bind <ADDRESS>`. |
| Other LAN devices time out connecting | Verify both machines are on the same subnet, Wi-Fi client/AP isolation is off, and your host firewall (`ufw`, `firewalld`, `nftables`) permits TCP on the chosen port. If your IP changed after switching Wi-Fi networks, press `r` in the TUI (or restart `share` to regenerate the TLS certificate SANs). |
| Browser warns `ERR_CERT_AUTHORITY_INVALID` | Expected on first connection with the auto-generated self-signed certificate. Click **Advanced → Proceed** and optionally match the SHA-256 fingerprint displayed in `share` (`?` in TUI), or run with `--http` if encryption is not needed. |
