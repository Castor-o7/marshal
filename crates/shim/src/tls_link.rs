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
/// The bearer token variable (MAR-24).
pub const TOKEN_ENV: &str = "MARSHAL_TOKEN";
/// This machine's pairing, as a code, in the config directory (MAR-24).
pub const PAIRING_FILE: &str = "pairing";
/// The daemon host's own token, written by the daemon to its config directory
/// (MAR-24). A shim on the same host reads it.
pub const LOCAL_TOKEN_FILE: &str = "local-token";

/// The SNI name sent. The certificate is pinned by fingerprint, so the name
/// is never checked; it matches the one the daemon puts in its certificate.
const SERVER_NAME: &str = "marshal-daemon";

/// Caps on each tunnel connection's dial and handshake, so a daemon that's
/// gone quiet doesn't hold a task forever.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        .or_else(|| load_pairing().map(|p| p.fingerprint))
}

/// This machine's pairing, if `marshal-shim pair` made one.
pub fn load_pairing() -> Option<marshal_entities::pairing::Pairing> {
    crate::config_file_candidates(PAIRING_FILE)
        .into_iter()
        .find_map(|path| {
            let code = std::fs::read_to_string(&path).ok()?;
            match marshal_entities::pairing::Pairing::from_code(&code) {
                Ok(pairing) => Some(pairing),
                Err(e) => {
                    log::warn!("[marshal-shim] ignoring {}: {e}", path.display());
                    None
                }
            }
        })
}

/// The hook listener's base URL from this machine's pairing, if it has one:
/// `https://<hooks>`, since a paired daemon serves its hooks over TLS.
pub fn paired_hook_base() -> Option<String> {
    load_pairing().map(|p| format!("https://{}", p.hooks))
}

/// The bearer this shim presents to a `--tls` daemon: `MARSHAL_TOKEN`, else
/// the pairing's token, else the daemon host's local token (same host, same
/// user). `None` means it presents nothing, which a `--tls` daemon refuses
/// and a plain one never asks for.
pub fn bearer() -> Option<String> {
    if let Some(token) = crate::address_env(TOKEN_ENV) {
        return Some(token);
    }
    if let Some(pairing) = load_pairing() {
        return Some(pairing.token);
    }
    crate::config_file_candidates(LOCAL_TOKEN_FILE)
        .into_iter()
        .find_map(|path| {
            let text = std::fs::read_to_string(path).ok()?;
            let token = text.lines().next().unwrap_or("").trim();
            (!token.is_empty()).then(|| token.to_string())
        })
}

/// Point `client` at `address`, through the TLS tunnel when it's `wss://`
/// with a pin, and give it this machine's bearer when there is one.
pub fn connect(client: &myko::client::MykoClient, address: String) {
    client.set_address(Some(myko_address(address)));
    if let Some(token) = bearer()
        && let Err(e) = client.set_bearer_token(token)
    {
        log::warn!("[marshal-shim] can't set the bearer token: {e}");
    }
}

/// `marshal-shim pair <code>`: exchange the invite at the daemon's `/pair`,
/// pinned by the fingerprint in the code, and keep the pairing. Also writes
/// `daemon-address` and `tls-pin`, so every other command finds the daemon.
pub fn pair_with_code(code: &str) -> io::Result<String> {
    use marshal_entities::pairing::{Invite, Pairing};
    let invite =
        Invite::from_code(code).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let body = serde_json::json!({ "invite": invite.invite }).to_string();
    let reply = post_pinned(&invite.hooks, "/pair", &body, &invite.fingerprint)?;
    let token = serde_json::from_str::<serde_json::Value>(&reply)
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_string))
        .ok_or_else(|| io::Error::other("the daemon's answer had no token"))?;
    let pairing = Pairing {
        address: invite.address.clone(),
        hooks: invite.hooks.clone(),
        fingerprint: invite.fingerprint.clone(),
        token,
        name: invite.name.clone(),
    };
    let path = save_config(PAIRING_FILE, &format!("{}\n", pairing.to_code()), true)?;
    save_config(
        "daemon-address",
        &format!("wss://{}\n", invite.address),
        false,
    )?;
    save_config(PIN_FILE, &format!("{}\n", invite.fingerprint), false)?;
    Ok(format!("{} (kept in {})", pairing.masked(), path.display()))
}

/// Remove this machine's pairing and what `pair` wrote with it. Returns
/// whether there was one.
pub fn remove_pairing() -> io::Result<bool> {
    let mut removed = false;
    for name in [PAIRING_FILE, PIN_FILE, "daemon-address"] {
        for path in crate::config_file_candidates(name) {
            match std::fs::remove_file(&path) {
                Ok(()) => removed |= name == PAIRING_FILE,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(removed)
}

fn save_config(name: &str, contents: &str, private: bool) -> io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    let path = crate::config_file_candidates(name)
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("no config directory (is HOME set?)"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    options.open(&path)?.write_all(contents.as_bytes())?;
    Ok(path)
}

/// A blocking `POST path` to `authority` over TLS pinned by `pin`, with a
/// JSON body. Returns the reply's body on 2xx.
fn post_pinned(authority: &str, path: &str, body: &str, pin: &str) -> io::Result<String> {
    use std::io::Write as _;
    use std::net::ToSocketAddrs as _;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let addrs: Vec<_> = authority.to_socket_addrs()?.collect();
    let tcp = crate::link::connect_any(addrs.as_slice(), deadline)
        .ok_or_else(|| io::Error::other(format!("can't reach the daemon at {authority}")))?;
    let mut stream = crate::link::Stream::Tls(Box::new(connect_https_pinned(tcp, pin)?));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.write_all(request.as_bytes())?;
    match crate::link::read_reply(&mut stream, deadline) {
        Ok(reply) => Ok(reply.body),
        Err(crate::link::PostFailure::Status(401)) => Err(io::Error::other(
            "the daemon refused the code: unknown, already used, or expired",
        )),
        Err(crate::link::PostFailure::Status(404)) => Err(io::Error::other(
            "the daemon isn't running with --tls, so it has no pairing",
        )),
        Err(crate::link::PostFailure::Status(status)) => {
            Err(io::Error::other(format!("the daemon answered {status}")))
        }
        Err(crate::link::PostFailure::Unreachable) => {
            Err(io::Error::other("no usable answer from the daemon"))
        }
    }
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
    let has_port = if authority.starts_with('[') {
        !authority.ends_with(']')
    } else {
        authority.contains(':')
    };
    let authority = if has_port {
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
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let (connector, authority) = (connector.clone(), authority.clone());
                    tokio::spawn(async move {
                        let connect = tokio::net::TcpStream::connect(&authority);
                        let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
                            Ok(Ok(tcp)) => tcp,
                            Ok(Err(e)) => {
                                log::warn!("[marshal-shim] can't reach {authority}: {e}");
                                return;
                            }
                            Err(_) => {
                                log::warn!("[marshal-shim] can't reach {authority}: timed out");
                                return;
                            }
                        };
                        let _ = tcp.set_nodelay(true);
                        let handshake = connector.connect(server_name(), tcp);
                        let mut remote =
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                                Ok(Ok(tls)) => tls,
                                Ok(Err(e)) => {
                                    log::warn!("[marshal-shim] TLS to {authority} failed: {e}");
                                    return;
                                }
                                Err(_) => {
                                    log::warn!("[marshal-shim] TLS to {authority} timed out");
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

/// Wrap `tcp` in pinned TLS and complete the handshake. Fails at once when no
/// pin is set, and with the certificate error when the pin doesn't match.
pub fn connect_https(tcp: TcpStream) -> io::Result<HttpsStream> {
    let pin = pin().ok_or_else(|| {
        io::Error::other(format!(
            "an https:// hook address needs the daemon's TLS fingerprint in {PIN_ENV} or {PIN_FILE}"
        ))
    })?;
    connect_https_pinned(tcp, &pin)
}

/// [`connect_https`] with the pin given.
pub fn connect_https_pinned(tcp: TcpStream, pin: &str) -> io::Result<HttpsStream> {
    let pin = pin.to_string();
    let connection = rustls::ClientConnection::new(client_config(&pin)?, server_name())
        .map_err(io::Error::other)?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    // Finish the handshake here, so a wrong pin fails in this call with its
    // reason, not later as an unexplained read error. The caller sets its own
    // timeouts afterwards.
    stream.sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.sock.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(stream)
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
            split_wss("wss://[::1]/myko"),
            Some(("[::1]:443".into(), "/myko".into()))
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
