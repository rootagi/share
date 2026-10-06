//! Command-line interface definition (clap derive).

use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;
use clap_complete::Shell;

const AFTER_HELP: &str = "\
EXAMPLES:
    share ~/Downloads                 Share a folder over HTTPS (self-signed certificate)
    share ./movie.mkv                 Share a single file
    share ~/Drop --upload --qr        Allow uploads and show a QR code
    share . --random-url --pin 123456 Require a secret URL and 6-digit PIN
    share . --http --port 9000        Plain HTTP on port 9000
    share . --interface wlan0         Listen only on the wlan0 address

LAN TRUST MODE: unless --auth, --pin, --token, or --random-url is specified,
anyone who can reach the server can read the shared files.";

/// Share files and folders over your local network.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "share",
    version,
    about,
    after_help = AFTER_HELP,
    arg_required_else_help = true,
    max_term_width = 100
)]
pub struct Cli {
    /// File or directory to share
    #[arg(value_name = "PATH", required_unless_present = "completions")]
    pub path: Option<PathBuf>,

    /// TCP port to listen on (0 = pick a free port)
    #[arg(short, long, default_value_t = 8080, value_name = "PORT")]
    pub port: u16,

    /// Address to listen on (default: all IPv4 interfaces)
    #[arg(short, long, value_name = "ADDRESS")]
    pub bind: Option<IpAddr>,

    /// Listen on the first IPv4 address of this network interface
    #[arg(short, long, value_name = "NAME", conflicts_with = "bind")]
    pub interface: Option<String>,

    /// Serve plain HTTP instead of HTTPS
    #[arg(long, conflicts_with_all = ["tls", "cert", "key"])]
    pub http: bool,

    /// Serve HTTPS (this is the default; the flag exists for explicit scripts)
    #[arg(long)]
    pub tls: bool,

    /// TLS certificate chain in PEM format (requires --key)
    #[arg(long, value_name = "PATH", requires = "key")]
    pub cert: Option<PathBuf>,

    /// TLS private key in PEM format (requires --cert)
    #[arg(long, value_name = "PATH", requires = "cert")]
    pub key: Option<PathBuf>,

    /// Allow clients to upload files
    #[arg(short, long)]
    pub upload: bool,

    /// Store all uploads in this directory (implies --upload)
    #[arg(long, value_name = "PATH")]
    pub upload_dir: Option<PathBuf>,

    /// Explicitly refuse uploads (this is the default)
    #[arg(long, conflicts_with_all = ["upload", "upload_dir"])]
    pub read_only: bool,

    /// Let the browser search sub-directories recursively
    #[arg(short, long)]
    pub recursive: bool,

    /// Show and serve dotfiles and hidden directories
    #[arg(long)]
    pub hidden: bool,

    /// Open the share URL in the local browser at start-up
    #[arg(long)]
    pub open: bool,

    /// Disable the terminal UI and log to stderr instead
    #[arg(long)]
    pub no_tui: bool,

    /// Show a QR code for the share URL
    #[arg(long)]
    pub qr: bool,

    /// Only log warnings and errors
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// More logging (-v debug, -vv trace including dependencies)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Maximum number of simultaneous TCP connections
    #[arg(long, default_value_t = 1024, value_name = "N")]
    pub max_connections: usize,

    /// Number of async worker threads (default: number of CPUs)
    #[arg(long, value_name = "N")]
    pub workers: Option<usize>,

    /// Display name of the share (default: the file or folder name)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Also negotiate HTTP/2 (HTTP/1.1 is the default and is usually faster for single large downloads)
    #[arg(long)]
    pub http2: bool,

    /// Seconds to wait for running transfers when shutting down
    #[arg(long, default_value_t = 5, value_name = "SECS")]
    pub shutdown_timeout: u64,

    /// Require a secret token prefix in the URL path (/s/<TOKEN>); use "random" to generate one
    #[arg(long, value_name = "TOKEN", conflicts_with = "random_url")]
    pub token: Option<String>,

    /// Generate a random secret capability URL prefix (/s/<random>)
    #[arg(long)]
    pub random_url: bool,

    /// Require HTTP Basic authentication (USER:PASS or just a password)
    #[arg(long, value_name = "USER:PASS", conflicts_with = "pin")]
    pub auth: Option<String>,

    /// Require a PIN code to access the share (HTTP Basic Auth and web UI PIN prompt)
    #[arg(long, value_name = "PIN")]
    pub pin: Option<String>,

    /// Automatically shut down after N completed downloads
    #[arg(long, value_name = "N")]
    pub max_downloads: Option<u64>,

    /// Automatically shut down after a duration (e.g. 15m, 2h, 30s)
    #[arg(long, value_name = "DURATION")]
    pub expire: Option<String>,

    /// Global transfer rate limit (e.g. 50M, 10MB, 500K, 1G)
    #[arg(long, value_name = "RATE")]
    pub rate_limit: Option<String>,

    /// Print a shell completion script and exit
    #[arg(long, value_name = "SHELL")]
    pub completions: Option<Shell>,
}
