# Security Policy

## Intended Scope & LAN Design

`share` is designed specifically for **ad-hoc file and directory sharing on local networks (LANs)** rather than as a hardened, public-facing internet server.

By default, `share` starts in **LAN Trust Mode**:
- **No authentication by default**: Unless `--pin`, `--auth`, `--token`, or `--random-url` is explicitly passed on the command line, anyone who can reach the listening TCP port on your subnet can browse and download shared files (and upload files if `--upload` is enabled).
- **Self-signed TLS certificates**: Default HTTPS mode uses a locally generated self-signed X.509 certificate for link encryption on the LAN rather than a public CA-issued certificate.
- **Same-port HTTP / WebDAV fallback**: To support native OS file managers (`dav://host:port`) without extra flags, the default listener accepts both TLS and plain HTTP connections on the same port.
- **Not hardened against public-internet threat models**: This project is a lightweight local utility and may have limitations or edge cases if exposed to untrusted networks. **Do not port-forward `share` to the public internet or run it on untrusted networks without appropriate network-level access controls (e.g., VPN/Tailscale or firewall rules).**

## Built-In Safeguards

Within its intended LAN scope, `share` implements the following boundaries to protect the host filesystem:
- **Path containment**: Relative paths are normalized and verified with `canonicalize()` + `starts_with()` to stay within the shared root directory; `..` segments and symlink escapes outside the root are rejected.
- **Hidden & temporary file isolation**: Dotfiles are hidden unless `--hidden` is specified, and in-progress `.share-upload-*.part` files are never listed or served.
- **Non-destructive uploads**: Uploaded files use `create_new` reservation and collision naming (`file (1).ext`) so existing files are not overwritten via `/api/upload`.
- **Optional access controls**: `--random-url` / `--token` (capability URLs), `--pin` / `--auth` (PIN and password authentication), `--max-downloads`, `--expire`, and `--rate-limit`.

## Supported Versions

| Version | Supported |
| :--- | :--- |
| `0.1.x` | Yes |

## Reporting an Issue

If you find a bug that breaks the intended filesystem boundary (such as path traversal outside the shared directory, unintended file overwrites in read-only mode, or authentication bypass when `--pin`/`--auth`/`--token` is enabled):

1. Please report it via [GitHub Security Advisories](https://github.com/rootagi/share/security/advisories/new) or open an issue on the [repository tracker](https://github.com/rootagi/share/issues) with reproduction steps.
2. Reports describing expected behavior of **LAN Trust Mode** (for example, unauthenticated access when running without `--auth`/`--pin`, or self-signed certificate warnings in browsers) are by design and not treated as security defects.
