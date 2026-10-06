//! Certificate material: loading user-supplied PEM files and generating (and
//! caching) a self-signed certificate for the machine's LAN addresses.
//!
//! The generated certificate is cached in `~/.config/share/` so that a browser
//! "trust this certificate" exception survives restarts. It is regenerated when
//! it is about to expire or when the machine gains an IP address it does not cover.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::error::{Result, ShareError};

/// Validity of generated certificates. Kept under 398 days, the limit several
/// platforms (notably Apple's) enforce for TLS server certificates.
const VALIDITY_DAYS: i64 = 365;
/// Regenerate cached certificates after this many days.
const REFRESH_AFTER: Duration = Duration::from_secs(300 * 24 * 3600);
/// Bound on the number of names accumulated in the cached certificate.
const MAX_NAMES: usize = 32;

#[derive(Debug, Clone)]
pub enum CertOrigin {
    /// Supplied with `--cert` / `--key`.
    User(PathBuf),
    /// Reused from the on-disk cache.
    Cached(PathBuf),
    /// Freshly generated; `Some(path)` when it was also stored.
    Generated(Option<PathBuf>),
}

pub struct CertMaterial {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub origin: CertOrigin,
}

/// Load a PEM certificate chain and private key from disk.
pub fn load_pem_files(cert_path: &Path, key_path: &Path) -> Result<CertMaterial> {
    let cert_bytes = fs::read(cert_path).map_err(|e| {
        ShareError::InvalidCertificate(format!("cannot read '{}': {e}", cert_path.display()))
    })?;
    let key_bytes = fs::read(key_path).map_err(|e| {
        ShareError::InvalidKey(format!("cannot read '{}': {e}", key_path.display()))
    })?;
    let (certs, key) = parse_pem(&cert_bytes, &key_bytes)?;
    Ok(CertMaterial {
        certs,
        key,
        origin: CertOrigin::User(cert_path.to_path_buf()),
    })
}

fn parse_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ShareError::InvalidCertificate(e.to_string()))?;
    if certs.is_empty() {
        return Err(ShareError::InvalidCertificate(
            "no PEM certificate found in file".into(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| ShareError::InvalidKey(format!("no usable PEM private key found ({e})")))?;
    Ok((certs, key))
}

/// SHA-256 fingerprint in the colon-separated form browsers display.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
    digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Names (IP addresses and host names) the certificate should be valid for.
pub fn wanted_names(ips: &[IpAddr]) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = ips.iter().map(|ip| ip.to_string()).collect();
    names.insert("localhost".into());
    names.insert("127.0.0.1".into());
    names.insert("::1".into());
    if let Some(host) = hostname() {
        names.insert(format!("{host}.local"));
        names.insert(host);
    }
    names
}

fn hostname() -> Option<String> {
    let raw = fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())?;
    let h = raw.trim().to_string();
    let valid = !h.is_empty()
        && h.len() <= 63
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !h.starts_with('-')
        && !h.starts_with('.');
    valid.then_some(h)
}

/// Generate a self-signed certificate; returns `(certificate PEM, private key PEM)`.
pub fn generate_self_signed(names: &BTreeSet<String>) -> Result<(String, String)> {
    let tls_err = |e: rcgen::Error| ShareError::Tls(format!("certificate generation failed: {e}"));
    let mut params =
        CertificateParams::new(names.iter().cloned().collect::<Vec<_>>()).map_err(tls_err)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "share LAN file server");
    dn.push(DnType::OrganizationName, "share");
    params.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(VALIDITY_DAYS);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key_pair = KeyPair::generate().map_err(tls_err)?;
    let cert = params.self_signed(&key_pair).map_err(tls_err)?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("share"))
}

struct Cached {
    names: BTreeSet<String>,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    fresh: bool,
}

fn read_cache(dir: &Path) -> Option<Cached> {
    let names: BTreeSet<String> = fs::read_to_string(dir.join("names.txt"))
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let cert_pem = fs::read(dir.join("cert.pem")).ok()?;
    let key_pem = fs::read(dir.join("key.pem")).ok()?;
    let age = fs::metadata(dir.join("cert.pem"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or(Duration::MAX);
    Some(Cached {
        names,
        cert_pem,
        key_pem,
        fresh: age < REFRESH_AFTER,
    })
}

fn write_file(path: &Path, data: &[u8], private: bool) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    opts.open(path)?.write_all(data)
}

fn store_cache(
    dir: &Path,
    names: &BTreeSet<String>,
    cert_pem: &str,
    key_pem: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    write_file(&dir.join("key.pem"), key_pem.as_bytes(), true)?;
    write_file(&dir.join("cert.pem"), cert_pem.as_bytes(), false)?;
    let list = names.iter().cloned().collect::<Vec<_>>().join("\n");
    write_file(&dir.join("names.txt"), list.as_bytes(), false)
}

/// Reuse the cached self-signed certificate when it still covers `wanted`,
/// otherwise generate (and store) a new one.
pub fn load_or_generate(wanted: BTreeSet<String>) -> Result<CertMaterial> {
    let dir = cache_dir();
    let mut names = wanted.clone();

    if let Some(dir) = &dir {
        if let Some(cached) = read_cache(dir) {
            if cached.fresh && wanted.is_subset(&cached.names) {
                if let Ok((certs, key)) = parse_pem(&cached.cert_pem, &cached.key_pem) {
                    return Ok(CertMaterial {
                        certs,
                        key,
                        origin: CertOrigin::Cached(dir.join("cert.pem")),
                    });
                }
            }
            if cached.fresh {
                // Keep names from earlier networks so a later return to them
                // does not trigger yet another browser warning.
                names.extend(cached.names.into_iter().take(MAX_NAMES));
            }
        }
    }

    let (cert_pem, key_pem) = generate_self_signed(&names)?;
    let (certs, key) = parse_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let stored = dir
        .as_ref()
        .and_then(|d| match store_cache(d, &names, &cert_pem, &key_pem) {
            Ok(()) => Some(d.join("cert.pem")),
            Err(e) => {
                tracing::warn!(
                    "could not cache the generated certificate in {}: {e}",
                    d.display()
                );
                None
            }
        });
    Ok(CertMaterial {
        certs,
        key,
        origin: CertOrigin::Generated(stored),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_certificate_round_trips_through_pem() {
        let names: BTreeSet<String> = ["192.168.1.15", "localhost", "box.local"]
            .map(String::from)
            .into();
        let (cert_pem, key_pem) = generate_self_signed(&names).unwrap();
        let (certs, _key) = parse_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
        assert_eq!(certs.len(), 1);
        let fp = fingerprint(&certs[0]);
        assert_eq!(fp.split(':').count(), 32);
    }

    #[test]
    fn garbage_is_rejected_with_specific_errors() {
        assert!(matches!(
            parse_pem(b"not pem", b"x"),
            Err(ShareError::InvalidCertificate(_))
        ));
        let (cert_pem, _) = generate_self_signed(&["localhost".to_string()].into()).unwrap();
        assert!(matches!(
            parse_pem(cert_pem.as_bytes(), b"nope"),
            Err(ShareError::InvalidKey(_))
        ));
    }

    #[test]
    fn wanted_names_always_include_loopback() {
        let n = wanted_names(&["10.1.2.3".parse().unwrap()]);
        assert!(n.contains("10.1.2.3") && n.contains("localhost") && n.contains("127.0.0.1"));
    }

    #[test]
    fn missing_files_give_clear_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_pem_files(&dir.path().join("a.crt"), &dir.path().join("a.key"))
            .err()
            .unwrap();
        assert!(matches!(err, ShareError::InvalidCertificate(_)));
    }
}
