//! HTTPS setup (rustls, `ring` crypto provider).

pub mod certificate;

use std::net::IpAddr;
use std::sync::Arc;

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::config::{Config, TlsMode};
use crate::error::{Result, ShareError};
use certificate::{CertMaterial, CertOrigin};

pub struct TlsSetup {
    pub acceptor: TlsAcceptor,
    pub fingerprint: String,
    /// Human-readable description of where the certificate came from.
    pub note: String,
}

/// Prepare TLS according to the configuration. Returns `None` for plain HTTP.
pub fn setup(config: &Config, advertised: &[IpAddr]) -> Result<Option<TlsSetup>> {
    let material = match &config.tls {
        TlsMode::Disabled => return Ok(None),
        TlsMode::Files { cert, key } => certificate::load_pem_files(cert, key)?,
        TlsMode::SelfSigned => {
            certificate::load_or_generate(certificate::wanted_names(advertised))?
        }
    };
    let fingerprint = certificate::fingerprint(&material.certs[0]);
    let note = match &material.origin {
        CertOrigin::User(p) => format!("certificate: {}", p.display()),
        CertOrigin::Cached(p) => format!("self-signed certificate (cached in {})", p.display()),
        CertOrigin::Generated(Some(p)) => {
            format!("self-signed certificate (new, saved to {})", p.display())
        }
        CertOrigin::Generated(None) => "self-signed certificate (new, not saved)".to_string(),
    };
    let acceptor = TlsAcceptor::from(Arc::new(server_config(material, config.http2)?));
    Ok(Some(TlsSetup {
        acceptor,
        fingerprint,
        note,
    }))
}

/// Build the rustls server configuration.
///
/// ALPN offers only `http/1.1` unless HTTP/2 was requested: for one large
/// download HTTP/1.1 avoids HTTP/2 flow-control windows throttling throughput.
pub fn server_config(material: CertMaterial, http2: bool) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ShareError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(material.certs, material.key)
        .map_err(|e| match e {
            rustls::Error::InconsistentKeys(_) => {
                ShareError::InvalidKey("the private key does not match the certificate".into())
            }
            other => ShareError::InvalidKey(other.to_string()),
        })?;
    cfg.alpn_protocols = if http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Ok(cfg)
}
