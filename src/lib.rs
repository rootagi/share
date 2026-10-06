//! `share` – fast LAN file and directory sharing.
//!
//! The crate is a library plus a thin binary so that integration tests can start the
//! real server in-process. See `docs/ARCHITECTURE.md` for the module map.

pub mod app;
pub mod banner;
pub mod cli;
pub mod config;
pub mod error;
pub mod fs;
pub mod logging;
pub mod metrics;
pub mod network;
pub mod qr;
pub mod server;
pub mod signals;
pub mod tls;
pub mod tui;
pub mod util;
pub mod web;

pub use error::{Result, ShareError};
