//! The shim's side of opt-in TLS (MAR-23). A daemon started with `--tls` has a
//! self-signed certificate, and the shim pins its SHA-256 fingerprint (the
//! daemon writes it to `<state dir>/tls/fingerprint`). The pin comes from
//! `MARSHAL_TLS_PIN`, or else the first line of a `tls-pin` file beside
//! `daemon-address` in the config directory.
//!
//! - myko's client can't be handed a certificate verifier, so a `wss://`
//!   daemon address is served through a loopback tunnel: [`myko_address`]
//!   starts it (once per process and daemon) and returns the plain `ws://`
//!   address of its local end. Each local connection gets its own pinned TLS
//!   connection to the daemon.
//! - The hook subcommands speak blocking HTTP, so [`connect_https`] wraps
//!   their TCP stream in pinned TLS directly.
//!
//! A plain `ws://` address never reaches any of this.

use std::collections::HashMap;
use std::io;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};

/// The pin variable.
pub const PIN_ENV: &str = "MARSHAL_TLS_PIN";
/// The pin file, in the shim's config directory.
pub const PIN_FILE: &str = "tls-pin";

/// The SNI name sent. The certificate is pinned by fingerprint, so the name
/// is never checked; it matches the one the daemon puts in its certificate.
const SERVER_NAME: &str = "marshal-daemon";

/// Make ring the process's default rustls provider. The shim's tree enables
/// aws-lc-rs too, through myko, and with two providers enabled rustls's
/// `builder()` panics unless one is installed. The first call wins.
pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The pin, if one is set: 64 hex characters, lowercased.
pub fn pin() -> Option<String> {
    if let Some(value) = crate::address_env(PIN_ENV) {
        return valid_pin(&value).or_else(|| {
            log::error!("[marshal-shim] {PIN_ENV} isn't a 64-character hex fingerprint");
            None
        });
    }
    crate::config_file_candidates(PIN_FILE)
        .into_iter()
        .find_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            let line = contents.lines().next().unwrap_or("").trim().to_string();
            valid_pin(&line).or_else(|| {
                log::error!(
                    "[marshal-shim] {} isn't a 64-character hex fingerprint",
                    path.display()
                );
                None
            })
        })
}

fn valid_pin(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

/// Lowercase hex SHA-256 of a DER certificate.
fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A rustls client config that accepts exactly the certificate whose SHA-256
/// is `pin`, and still checks the handshake signatures.
pub fn client_config(pin: &str) -> io::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned {
        pin: pin.to_ascii_lowercase(),
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct Pinned {
    pin: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn server_name() -> ServerName<'static> {
    ServerName::try_from(SERVER_NAME).expect("valid DNS name")
}

/// Split `wss://host:port/path` into `host:port` and `/path`.
fn split_wss(address: &str) -> Option<(String, String)> {
    let rest = address.strip_prefix("wss://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    let authority = if authority.starts_with('[') || authority.rsplit_once(':').is_some() {
        authority.to_string()
    } else {
        format!("{authority}:443")
    };
    Some((authority, path.to_string()))
}

/// The address myko's client should dial for `address`. A plain address comes
/// back as it is. A `wss://` one, with a pin set, comes back as the `ws://`
/// address of a loopback tunnel to it. Without a pin it comes back unchanged:
/// that's a TLS proxy with a certificate the system trusts, as before MAR-23
/// (and a `--tls` daemon's self-signed certificate is refused, as it should be).
pub fn myko_address(address: String) -> String {
    let Some((authority, path)) = split_wss(&address) else {
        return address;
    };
    let Some(pin) = pin() else {
        log::debug!(
            "[marshal-shim] {address}: no {PIN_ENV} or {PIN_FILE}, so TLS is left to the \
             system's certificates"
        );
        return address;
    };
    static TUNNELS: OnceLock<Mutex<HashMap<(String, String), u16>>> = OnceLock::new();
    let tunnels = TUNNELS.get_or_init(Default::default);
    let mut tunnels = tunnels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (authority.clone(), pin.clone());
    let port = match tunnels.get(&key) {
        Some(port) => *port,
        None => match start_tunnel(authority.clone(), &pin) {
            Ok(port) => {
                tunnels.insert(key, port);
                port
            }
            Err(e) => {
                log::error!("[marshal-shim] can't start the TLS tunnel to {authority}: {e}");
                return address;
            }
        },
    };
    format!("ws://127.0.0.1:{port}{path}")
}

/// Bind a loopback port and carry each connection to it over pinned TLS to
/// `authority`, on a thread of its own so it doesn't depend on the caller's
/// runtime. Returns the local port.
fn start_tunnel(authority: String, pin: &str) -> io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let connector = tokio_rustls::TlsConnector::from(client_config(pin)?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("marshal-tls-tunnel".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(e) => {
                        log::error!("[marshal-shim] TLS tunnel listener: {e}");
                        return;
                    }
                };
                loop {
                    let (mut local, _) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            log::warn!("[marshal-shim] TLS tunnel accept error: {e}");
                            continue;
                        }
                    };
                    let (connector, authority) = (connector.clone(), authority.clone());
                    tokio::spawn(async move {
                        let tcp = match tokio::net::TcpStream::connect(&authority).await {
                            Ok(tcp) => tcp,
                            Err(e) => {
                                log::warn!("[marshal-shim] can't reach {authority}: {e}");
                                return;
                            }
                        };
                        let _ = tcp.set_nodelay(true);
                        let mut remote = match connector.connect(server_name(), tcp).await {
                            Ok(tls) => tls,
                            Err(e) => {
                                log::warn!("[marshal-shim] TLS to {authority} failed: {e}");
                                return;
                            }
                        };
                        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                    });
                }
            });
        })?;
    Ok(port)
}

/// A blocking pinned TLS stream over `tcp`, for the hook subcommands. The
/// handshake runs on first read or write, under `tcp`'s timeouts.
pub type HttpsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// Wrap `tcp` in pinned TLS. Fails at once when no pin is set.
pub fn connect_https(tcp: TcpStream) -> io::Result<HttpsStream> {
    let pin = pin().ok_or_else(|| {
        io::Error::other(format!(
            "an https:// hook address needs the daemon's TLS fingerprint in {PIN_ENV} or {PIN_FILE}"
        ))
    })?;
    let connection = rustls::ClientConnection::new(client_config(&pin)?, server_name())
        .map_err(io::Error::other)?;
    Ok(rustls::StreamOwned::new(connection, tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_64_hex_characters_in_any_case() {
        let pin = "AB".repeat(32);
        assert_eq!(valid_pin(&format!(" {pin}\n")), Some("ab".repeat(32)));
        assert_eq!(valid_pin(&"ab".repeat(31)), None);
        assert_eq!(valid_pin(&"zz".repeat(32)), None);
    }

    #[test]
    fn a_wss_address_splits_into_authority_and_path() {
        assert_eq!(
            split_wss("wss://murph:6155/myko"),
            Some(("murph:6155".into(), "/myko".into()))
        );
        assert_eq!(
            split_wss("wss://[::1]:6155"),
            Some(("[::1]:6155".into(), String::new()))
        );
        assert_eq!(
            split_wss("wss://murph/myko"),
            Some(("murph:443".into(), "/myko".into()))
        );
        assert_eq!(split_wss("ws://murph:6155"), None);
        assert_eq!(split_wss("wss:///myko"), None);
    }

    #[test]
    fn a_plain_address_is_left_alone() {
        assert_eq!(
            myko_address("ws://127.0.0.1:6155".into()),
            "ws://127.0.0.1:6155"
        );
    }
}
