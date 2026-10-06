# Contributing to `share`

Thank you for your interest in contributing to `share`!

## Development Setup

`share` requires **Rust 1.85+** (2024 Edition) and Cargo. No external C libraries, Node.js toolchains, or system OpenSSL packages are needed.

```bash
git clone https://github.com/rootagi/share.git
cd share
cargo build
```

## Pre-Submit Checks

Before opening a pull request, please ensure all formatting, lint, and test checks pass locally:

```bash
# 1. Check formatting
cargo fmt --all -- --check

# 2. Run Clippy with warnings denied
cargo clippy --all-targets --locked -- -D warnings

# 3. Run unit and integration tests
cargo test --locked
```

## Design Guidelines

- **Single self-contained binary**: Avoid adding runtime dependencies or external web assets (fonts, CDN scripts). Browser assets in `assets/web/` are embedded into the binary via `include_str!`.
- **Constant-memory streaming**: File downloads, uploads, and folder archives must stream in bounded chunks rather than buffering entire files in memory or writing temporary archives to disk.
- **Filesystem safety**: Any new route that accesses the filesystem must resolve paths through `src/fs/paths.rs` (`resolve`, `resolve_dir`, or `resolve_or_create_dir`) to preserve root containment.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `share` by you shall be dual-licensed under the [MIT License](LICENSE-MIT) and the [Apache License, Version 2.0](LICENSE-APACHE), without any additional terms or conditions.
