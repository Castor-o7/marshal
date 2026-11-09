//! Opt-in TLS (MAR-23, from muster's gate). With `marshal-daemon --tls`, each
//! public listener (`MARSHAL_BIND` for WebSocket and MCP, `MARSHAL_HOOK_BIND`
//! for `/hook/*`) is a TLS listener that forwards every connection, after the
//! handshake, to the real listener on loopback. myko-server has no TLS and its
//! router takes a plain `TcpStream`, so this sits in front of it rather than
//! inside it.
//!
//! The certificate is self-signed, made on first start and kept in the state
//! directory. Clients pin its SHA-256 fingerprint (64 lowercase hex characters,
//! the same form muster used), so no certificate authority is involved and the
//! name in it is never checked.
//!
//! TLS only: who may connect is decided behind it, by myko#129's auth provider
//! (MAR-24, MAR-80) and the hook listener's token (MAR-81). Without `--tls`
//! nothing here runs.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest as _, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// The subdirectory of the state directory that holds the identity.
pub const TLS_DIR: &str = "tls";
const CERT_FILE: &str = "cert.der";
const KEY_FILE: &str = "key.der";
/// The fingerprint, written beside the identity so the operator (and a shim
/// on the same host) can read it without parsing the certificate.
pub const FINGERPRINT_FILE: &str = "fingerprint";

/// The name in the certificate. Clients pin the fingerprint, so it's never
/// checked; it only has to be a valid DNS name for SNI.
pub const SERVER_NAME: &str = "marshal-daemon";

/// Time a client gets to finish the TLS handshake. A connection that opens and
/// sends nothing doesn't hold a task for longer than this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon's certificate and private key, both DER.
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }
}

/// Lowercase hex SHA-256 of a DER certificate: what clients pin.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where the identity lives under `state_dir`.
pub fn identity_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(TLS_DIR)
}

/// Load the identity from `dir`, making it on first use. The key is written
/// owner-only. The certificate and fingerprint aren't secret.
pub fn load_or_create_identity(dir: &Path) -> io::Result<Identity> {
    let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
    let identity = if cert_path.exists() && key_path.exists() {
        Identity {
            cert_der: std::fs::read(&cert_path)?,
            key_der: std::fs::read(&key_path)?,
        }
    } else {
        let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(io::Error::other)?;
        let identity = Identity {
            cert_der: generated.cert.der().to_vec(),
            key_der: generated.signing_key.serialize_der(),
        };
        create_private_dir(dir)?;
        write_file(&key_path, &identity.key_der, true)?;
        write_file(&cert_path, &identity.cert_der, false)?;
        log::info!(
            "[tls] made a new certificate in {} (fingerprint {})",
            dir.display(),
            identity.fingerprint()
        );
        identity
    };
    write_file(
        &dir.join(FINGERPRINT_FILE),
        format!("{}\n", identity.fingerprint()).as_bytes(),
        false,
    )?;
    Ok(identity)
}

/// A rustls server config for `identity`, on the ring provider, which keeps
/// the daemon smaller than aws-lc-rs would (see the daemon's Cargo.toml).
pub fn acceptor(identity: &Identity) -> io::Result<TlsAcceptor> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
        )
        .map_err(io::Error::other)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Accept TLS on `listener` and forward each connection to `upstream`, the real
/// listener on loopback, copying bytes both ways until either side closes.
/// Runs forever. A failed handshake or an unreachable upstream ends only that
/// connection.
pub async fn serve(listener: TcpListener, acceptor: TlsAcceptor, upstream: SocketAddr) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("[tls] accept error: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = forward(tcp, acceptor, upstream).await {
                log::debug!("[tls] connection from {peer} ended: {e}");
            }
        });
    }
}

async fn forward(tcp: TcpStream, acceptor: TlsAcceptor, upstream: SocketAddr) -> io::Result<()> {
    let _ = tcp.set_nodelay(true);
    let mut tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
    let mut inner = TcpStream::connect(upstream).await?;
    let _ = inner.set_nodelay(true);
    tokio::io::copy_bidirectional(&mut tls, &mut inner).await?;
    Ok(())
}

/// Refuse an internal address that isn't loopback: behind TLS, the plain
/// listeners must not be reachable from the network.
pub fn require_loopback(name: &str, addr: SocketAddr) -> io::Result<()> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a loopback address with --tls, got {addr}"),
        ))
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` through a temporary file and a rename, so a crash
/// can't leave half a key. `private` makes it owner-only.
fn write_file(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    options.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_identity(dir.path()).unwrap();
        let second = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);
        let written = std::fs::read_to_string(dir.path().join(FINGERPRINT_FILE)).unwrap();
        assert_eq!(written.trim(), first.fingerprint());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        load_or_create_identity(&dir.path().join("tls")).unwrap();
        let mode = |name: &str| {
            std::fs::metadata(dir.path().join("tls").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(KEY_FILE), 0o600);
        assert_eq!(mode(""), 0o700);
    }

    #[test]
    fn an_internal_address_off_loopback_is_refused() {
        assert!(require_loopback("MARSHAL_MYKO_BIND", "127.0.0.1:6157".parse().unwrap()).is_ok());
        assert!(require_loopback("MARSHAL_MYKO_BIND", "[::1]:6157".parse().unwrap()).is_ok());
        assert!(require_loopback("MARSHAL_MYKO_BIND", "0.0.0.0:6157".parse().unwrap()).is_err());
    }
}
