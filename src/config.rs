//! Validated runtime configuration, derived from the parsed command line.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Cli;
use crate::error::{Result, ShareError};
use crate::network::{self, NetIface};

/// What the user pointed `share` at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Dir,
    File,
}

/// The shared path. `path` is canonical; every request is resolved against it.
#[derive(Debug, Clone)]
pub struct ShareRoot {
    pub path: PathBuf,
    pub kind: RootKind,
    /// Name shown in the UIs (`--name` or the file/folder name).
    pub name: String,
    /// `path` with the home directory abbreviated to `~`.
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    Disabled,
    SelfSigned,
    Files { cert: PathBuf, key: PathBuf },
}

#[derive(Debug, Clone)]
pub struct UploadConfig {
    pub enabled: bool,
    /// Fixed destination (`--upload-dir`). `None` = the directory being browsed.
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Quiet,
    Normal,
    Debug,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Expected username (`None` when configured via `--pin` or password-only `--auth`).
    pub username: Option<String>,
    /// Expected password or PIN.
    pub secret: String,
    /// Display-friendly description (`"PIN"` or `"user:***"`).
    pub display: String,
    /// Random per-run token accepted in `share_auth` cookie.
    pub session_token: String,
}

impl AuthConfig {
    pub fn verify(&self, user: &str, pass: &str) -> bool {
        let pass_ok = constant_time_eq(pass.as_bytes(), self.secret.as_bytes());
        match &self.username {
            Some(expected_user) => {
                constant_time_eq(user.as_bytes(), expected_user.as_bytes()) && pass_ok
            }
            None => pass_ok,
        }
    }

    pub fn verify_raw(&self, decoded: &str) -> bool {
        if let Some((user, pass)) = decoded.split_once(':') {
            self.verify(user, pass)
        } else if self.username.is_none() {
            constant_time_eq(decoded.as_bytes(), self.secret.as_bytes())
        } else {
            false
        }
    }

    pub fn verify_session(&self, token: &str) -> bool {
        constant_time_eq(token.as_bytes(), self.session_token.as_bytes())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Clone)]
pub struct Config {
    pub root: ShareRoot,
    pub bind: IpAddr,
    pub port: u16,
    pub tls: TlsMode,
    pub http2: bool,
    pub upload: UploadConfig,
    pub show_hidden: bool,
    pub recursive_search: bool,
    pub open_browser: bool,
    pub no_tui: bool,
    pub show_qr: bool,
    pub log_level: LogLevel,
    pub max_connections: usize,
    pub workers: Option<usize>,
    pub shutdown_timeout: Duration,
    /// If set, all routes are nested under `/s/<token>`.
    pub url_token: Option<String>,
    /// Optional HTTP Basic / PIN authentication.
    pub auth: Option<AuthConfig>,
    /// Shut down after this many completed downloads.
    pub max_downloads: Option<u64>,
    /// Shut down after this duration.
    pub expire_after: Option<Duration>,
    /// Global transfer rate limit in bytes/sec.
    pub rate_limit: Option<u64>,
}

impl Config {
    /// Build a configuration, querying the real network interfaces.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        Self::from_cli_with(cli, &network::interfaces::list())
    }

    /// Same as [`Config::from_cli`] with an explicit interface list (testable).
    pub fn from_cli_with(cli: Cli, ifaces: &[NetIface]) -> Result<Self> {
        let raw_path = cli
            .path
            .clone()
            .ok_or_else(|| ShareError::Config("no PATH given".into()))?;
        let root = resolve_root(&raw_path, cli.name.as_deref())?;

        let bind = match (&cli.bind, &cli.interface) {
            (Some(ip), _) => *ip,
            (None, Some(name)) => network::addresses::resolve_interface(name, ifaces)?,
            (None, None) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };

        let tls = if cli.http {
            TlsMode::Disabled
        } else if let (Some(cert), Some(key)) = (&cli.cert, &cli.key) {
            TlsMode::Files {
                cert: cert.clone(),
                key: key.clone(),
            }
        } else {
            TlsMode::SelfSigned
        };

        let upload_enabled = cli.upload || cli.upload_dir.is_some();
        let upload_dir = match &cli.upload_dir {
            Some(dir) => Some(prepare_upload_dir(dir)?),
            None => None,
        };
        if upload_enabled && root.kind == RootKind::File && upload_dir.is_none() {
            return Err(ShareError::Config(
                "uploads while sharing a single file need a destination: add --upload-dir <PATH>"
                    .into(),
            ));
        }

        if cli.max_connections == 0 {
            return Err(ShareError::Config(
                "--max-connections must be at least 1".into(),
            ));
        }
        if cli.workers == Some(0) {
            return Err(ShareError::Config("--workers must be at least 1".into()));
        }
        if cli.max_downloads == Some(0) {
            return Err(ShareError::Config(
                "--max-downloads must be at least 1".into(),
            ));
        }

        let url_token = if cli.random_url {
            Some(generate_random_token(12))
        } else {
            match cli.token.as_deref() {
                Some("random") => Some(generate_random_token(12)),
                Some(t) => Some(validate_token(t)?),
                None => None,
            }
        };

        let auth = match (&cli.auth, &cli.pin) {
            (Some(raw), None) => Some(parse_auth(raw, false)?),
            (None, Some(pin)) => Some(parse_auth(pin, true)?),
            (None, None) => None,
            (Some(_), Some(_)) => {
                return Err(ShareError::Config(
                    "--auth and --pin cannot be used together".into(),
                ));
            }
        };

        let expire_after = match cli.expire.as_deref() {
            Some(s) => Some(parse_duration(s)?),
            None => None,
        };

        let rate_limit = match cli.rate_limit.as_deref() {
            Some(s) => Some(parse_rate_limit(s)?),
            None => None,
        };

        let log_level = if cli.quiet {
            LogLevel::Quiet
        } else {
            match cli.verbose {
                0 => LogLevel::Normal,
                1 => LogLevel::Debug,
                _ => LogLevel::Trace,
            }
        };

        Ok(Config {
            root,
            bind,
            port: cli.port,
            tls,
            http2: cli.http2,
            upload: UploadConfig {
                enabled: upload_enabled,
                dir: upload_dir,
            },
            show_hidden: cli.hidden,
            recursive_search: cli.recursive,
            open_browser: cli.open,
            no_tui: cli.no_tui,
            show_qr: cli.qr,
            log_level,
            max_connections: cli.max_connections,
            workers: cli.workers,
            shutdown_timeout: Duration::from_secs(cli.shutdown_timeout),
            url_token,
            auth,
            max_downloads: cli.max_downloads,
            expire_after,
            rate_limit,
        })
    }

    /// Socket address to bind.
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }

    pub fn is_tls(&self) -> bool {
        self.tls != TlsMode::Disabled
    }

    pub fn scheme(&self) -> &'static str {
        if self.is_tls() { "https" } else { "http" }
    }

    /// Format a base URL for `addr`, including `/s/<token>` when configured.
    pub fn format_base_url(&self, addr: SocketAddr) -> String {
        let base = format!("{}://{addr}", self.scheme());
        match &self.url_token {
            Some(t) => format!("{base}/s/{t}"),
            None => base,
        }
    }
}

fn generate_random_token(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..36u8);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect()
}

fn validate_token(raw: &str) -> Result<String> {
    let t = raw.trim();
    if t.is_empty()
        || t == "."
        || t == ".."
        || !t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(ShareError::Config(
            "--token must contain only ASCII letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(t.to_string())
}

fn parse_auth(raw: &str, is_pin: bool) -> Result<AuthConfig> {
    if raw.is_empty() {
        return Err(ShareError::Config(
            "authentication secret cannot be empty".into(),
        ));
    }
    let session_token = generate_random_token(32);
    if is_pin {
        return Ok(AuthConfig {
            username: None,
            secret: raw.to_string(),
            display: "PIN".to_string(),
            session_token,
        });
    }
    if let Some((user, pass)) = raw.split_once(':') {
        if pass.is_empty() {
            return Err(ShareError::Config("--auth password cannot be empty".into()));
        }
        let username = if user.is_empty() {
            None
        } else {
            Some(user.to_string())
        };
        let display = match &username {
            Some(u) => format!("{u}:***"),
            None => "PIN".to_string(),
        };
        Ok(AuthConfig {
            username,
            secret: pass.to_string(),
            display,
            session_token,
        })
    } else {
        Ok(AuthConfig {
            username: None,
            secret: raw.to_string(),
            display: "PIN".to_string(),
            session_token,
        })
    }
}

pub fn parse_duration(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ShareError::Config("duration cannot be empty".into()));
    }
    let split_idx = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split_idx);
    let n: u64 = num_str
        .parse()
        .map_err(|_| ShareError::Config(format!("invalid duration: '{raw}'")))?;
    if n == 0 {
        return Err(ShareError::Config("duration must be greater than 0".into()));
    }
    match unit.trim().to_ascii_lowercase().as_str() {
        "ms" => Ok(Duration::from_millis(n)),
        "" | "s" | "sec" | "secs" => Ok(Duration::from_secs(n)),
        "m" | "min" | "mins" => Ok(Duration::from_secs(n.saturating_mul(60))),
        "h" | "hr" | "hrs" => Ok(Duration::from_secs(n.saturating_mul(3600))),
        "d" | "day" | "days" => Ok(Duration::from_secs(n.saturating_mul(86400))),
        other => Err(ShareError::Config(format!(
            "unknown duration unit '{other}' in '{raw}' (use s, m, h, or d)"
        ))),
    }
}

pub fn parse_rate_limit(raw: &str) -> Result<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ShareError::Config("rate limit cannot be empty".into()));
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split_idx);
    let val: f64 = num_str
        .parse()
        .map_err(|_| ShareError::Config(format!("invalid rate limit: '{raw}'")))?;
    if !val.is_finite() || val <= 0.0 {
        return Err(ShareError::Config(
            "rate limit must be greater than 0".into(),
        ));
    }
    let unit_clean = unit
        .trim()
        .trim_end_matches("/s")
        .trim_end_matches("ps")
        .to_ascii_lowercase();
    let mult: f64 = match unit_clean.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "kib" => 1_024.0,
        "m" | "mb" => 1_000_000.0,
        "mib" => 1_048_576.0,
        "g" | "gb" => 1_000_000_000.0,
        "gib" => 1_073_741_824.0,
        other => {
            return Err(ShareError::Config(format!(
                "unknown rate limit unit '{other}' in '{raw}' (use K, M, or G)"
            )));
        }
    };
    let bytes = (val * mult) as u64;
    if bytes == 0 {
        return Err(ShareError::Config(
            "rate limit must be at least 1 byte/s".into(),
        ));
    }
    Ok(bytes)
}

fn resolve_root(raw: &Path, name: Option<&str>) -> Result<ShareRoot> {
    let path = std::fs::canonicalize(raw).map_err(|e| map_path_error(raw, e))?;
    let meta = std::fs::metadata(&path).map_err(|e| map_path_error(&path, e))?;
    let kind = if meta.is_dir() {
        RootKind::Dir
    } else if meta.is_file() {
        RootKind::File
    } else {
        return Err(ShareError::Config(format!(
            "'{}' is neither a regular file nor a directory",
            path.display()
        )));
    };
    let default_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    Ok(ShareRoot {
        display: tilde(&path),
        name: name.map(str::to_owned).unwrap_or(default_name),
        path,
        kind,
    })
}

fn prepare_upload_dir(dir: &Path) -> Result<PathBuf> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| {
            ShareError::io(format!("creating upload directory '{}'", dir.display()), e)
        })?;
    }
    let canon = std::fs::canonicalize(dir).map_err(|e| map_path_error(dir, e))?;
    if !canon.is_dir() {
        return Err(ShareError::Config(format!(
            "--upload-dir '{}' is not a directory",
            canon.display()
        )));
    }
    Ok(canon)
}

fn map_path_error(path: &Path, err: std::io::Error) -> ShareError {
    match err.kind() {
        std::io::ErrorKind::NotFound => ShareError::PathNotFound(path.to_path_buf()),
        std::io::ErrorKind::PermissionDenied => ShareError::PermissionDenied(path.to_path_buf()),
        _ => ShareError::io(format!("accessing '{}'", path.display()), err),
    }
}

/// Abbreviate the home directory as `~` for display.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            if let Ok(rest) = path.strip_prefix(&home) {
                return if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", rest.display())
                };
            }
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("share").chain(args.iter().copied()))
    }

    fn cfg(args: &[&str]) -> Result<Config> {
        Config::from_cli_with(parse(args).unwrap(), &[])
    }

    #[test]
    fn defaults_are_safe_and_simple() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(&[dir.path().to_str().unwrap()]).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.tls, TlsMode::SelfSigned);
        assert!(!c.upload.enabled);
        assert!(!c.show_hidden);
        assert_eq!(c.root.kind, RootKind::Dir);
        assert!(c.bind.is_unspecified());
        assert_eq!(c.scheme(), "https");
    }

    #[test]
    fn http_flag_disables_tls() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(&[dir.path().to_str().unwrap(), "--http"]).unwrap();
        assert_eq!(c.tls, TlsMode::Disabled);
        assert_eq!(c.scheme(), "http");
    }

    #[test]
    fn conflicting_flags_are_rejected_by_clap() {
        assert!(parse(&[".", "--http", "--cert", "a", "--key", "b"]).is_err());
        assert!(parse(&[".", "--cert", "a"]).is_err(), "--cert needs --key");
        assert!(parse(&[".", "--upload", "--read-only"]).is_err());
        assert!(parse(&[".", "--bind", "1.2.3.4", "--interface", "lo"]).is_err());
        assert!(parse(&[".", "--quiet", "--verbose"]).is_err());
        assert!(parse(&[".", "--bind", "not-an-ip"]).is_err());
    }

    #[test]
    fn single_file_upload_requires_upload_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let err = cfg(&[file.to_str().unwrap(), "--upload"]).unwrap_err();
        assert!(matches!(err, ShareError::Config(_)));

        let inbox = dir.path().join("inbox");
        let c = cfg(&[
            file.to_str().unwrap(),
            "--upload-dir",
            inbox.to_str().unwrap(),
        ])
        .unwrap();
        assert!(c.upload.enabled);
        assert_eq!(c.root.kind, RootKind::File);
        assert!(inbox.is_dir(), "missing upload dir is created");
    }

    #[test]
    fn missing_path_is_a_clear_error() {
        let err = cfg(&["/definitely/not/here"]).unwrap_err();
        assert!(matches!(err, ShareError::PathNotFound(_)));
    }

    #[test]
    fn interface_flag_resolves_to_its_address() {
        let dir = tempfile::tempdir().unwrap();
        let ifaces = [NetIface {
            name: "wlan0".into(),
            addrs: vec!["192.168.7.7".parse().unwrap()],
        }];
        let cli = parse(&[dir.path().to_str().unwrap(), "-i", "wlan0"]).unwrap();
        let c = Config::from_cli_with(cli, &ifaces).unwrap();
        assert_eq!(c.bind.to_string(), "192.168.7.7");

        let cli = parse(&[dir.path().to_str().unwrap(), "-i", "nope"]).unwrap();
        assert!(matches!(
            Config::from_cli_with(cli, &ifaces),
            Err(ShareError::InterfaceUnavailable(_))
        ));
    }

    #[test]
    fn verbosity_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        assert_eq!(cfg(&[p, "-q"]).unwrap().log_level, LogLevel::Quiet);
        assert_eq!(cfg(&[p, "-v"]).unwrap().log_level, LogLevel::Debug);
        assert_eq!(cfg(&[p, "-vv"]).unwrap().log_level, LogLevel::Trace);
    }

    #[test]
    fn token_auth_expire_and_rate_limit_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();

        let c = cfg(&[p, "--random-url"]).unwrap();
        assert_eq!(c.url_token.as_ref().unwrap().len(), 12);

        let c = cfg(&[p, "--token", "secret_123"]).unwrap();
        assert_eq!(c.url_token.as_deref(), Some("secret_123"));
        assert!(cfg(&[p, "--token", "bad/token"]).is_err());

        let c = cfg(&[p, "--auth", "alice:wonderland"]).unwrap();
        let auth = c.auth.unwrap();
        assert!(auth.verify("alice", "wonderland"));
        assert!(!auth.verify("bob", "wonderland"));
        assert!(!auth.verify("alice", "wrong"));

        let c = cfg(&[p, "--pin", "123456"]).unwrap();
        let pin = c.auth.unwrap();
        assert!(pin.verify("", "123456"));
        assert!(pin.verify("any", "123456"));
        assert!(pin.verify_raw("123456"));
        assert!(!pin.verify("", "654321"));

        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("10w").is_err());

        assert_eq!(parse_rate_limit("50M").unwrap(), 50_000_000);
        assert_eq!(parse_rate_limit("10MiB").unwrap(), 10 * 1_048_576);
        assert_eq!(parse_rate_limit("500K/s").unwrap(), 500_000);
        assert!(parse_rate_limit("0M").is_err());
        assert!(parse_rate_limit("bad").is_err());
    }
}
