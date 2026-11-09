//! `marshal-daemon --tls` (MAR-23), end to end against the real binary: a
//! client that pins the daemon's fingerprint reaches myko (a WebSocket upgrade
//! and a ping answered) and the hook listener; a wrong pin is refused in the
//! handshake; plain WebSocket on the TLS port gets nothing; and `--tls` with
//! `--plain` is refused. Plain mode is what every other test here runs.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

const DEADLINE: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    _state: tempfile::TempDir,
    public: SocketAddr,
    hooks: SocketAddr,
    fingerprint: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start `marshal-daemon --tls` on port 0 for its public listeners, and read
/// back where they bound from its log. myko's loopback port has to be named,
/// since myko binds it itself, so a lost race for it is retried.
fn start() -> Daemon {
    for _ in 0..3 {
        if let Some(daemon) = try_start() {
            return daemon;
        }
    }
    panic!("marshal-daemon --tls didn't start in 3 tries");
}

fn try_start() -> Option<Daemon> {
    let state = tempfile::tempdir().unwrap();
    let log = state.path().join("daemon.log");
    let child = Command::new(env!("CARGO_BIN_EXE_marshal-daemon"))
        .arg("--tls")
        .env("MARSHAL_BIND", "127.0.0.1:0")
        .env("MARSHAL_HOOK_BIND", "127.0.0.1:0")
        .env("MARSHAL_MYKO_BIND", format!("127.0.0.1:{}", free_port()))
        .env("MARSHAL_STATE_DIR", state.path())
        // Its own home, so the local token (MAR-24) lands in the temp dir.
        .env("HOME", state.path().join("home"))
        .env_remove("XDG_CONFIG_HOME")
        .env("RUST_LOG", "info")
        .env_remove("MYKO_POSTGRES_URL")
        .env_remove("MARSHAL_PERSIST")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn marshal-daemon");
    let mut daemon = Daemon {
        child,
        _state: state,
        public: "0.0.0.0:0".parse().unwrap(),
        hooks: "0.0.0.0:0".parse().unwrap(),
        fingerprint: String::new(),
    };
    let started = Instant::now();
    while started.elapsed() < DEADLINE {
        if daemon.child.try_wait().unwrap().is_some() {
            return None;
        }
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        // myko logs through `tracing`, not `log`, so its own "listening" line
        // doesn't reach here. Its port answering is the sign it's up.
        if let Some(line) = text.lines().find(|l| l.contains("listening on wss://")) {
            let field = |prefix: &str| -> SocketAddr {
                let rest = &line[line.find(prefix).unwrap() + prefix.len()..];
                rest.split([',', ' ']).next().unwrap().parse().unwrap()
            };
            daemon.public = field("wss://");
            daemon.hooks = field("https://");
            daemon.fingerprint = field_after(line, "fingerprint ");
            let myko: SocketAddr = field("myko on ws://");
            let up = Instant::now();
            while up.elapsed() < DEADLINE {
                if std::net::TcpStream::connect(myko).is_ok() {
                    return Some(daemon);
                }
                if daemon.child.try_wait().unwrap().is_some() {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "marshal-daemon --tls never said where it listens:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

fn field_after(line: &str, prefix: &str) -> String {
    let rest = &line[line.find(prefix).unwrap() + prefix.len()..];
    rest.split([';', ',', ' ', ')']).next().unwrap().to_string()
}

/// A verifier that accepts exactly the certificate whose SHA-256 is `pin`,
/// and still checks the handshake signatures, as the shim's will.
#[derive(Debug)]
struct Pinned {
    pin: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if daemon::tls::fingerprint(end_entity) == self.pin {
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

async fn tls_connect(
    addr: SocketAddr,
    pin: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            pin: pin.to_string(),
            provider,
        }))
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(daemon::tls::SERVER_NAME).unwrap(), tcp)
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_client_reaches_myko_and_the_hooks_through_tls() {
    let daemon = start();
    assert_eq!(daemon.fingerprint.len(), 64, "{}", daemon.fingerprint);
    let written = std::fs::read_to_string(daemon._state.path().join("tls/fingerprint")).unwrap();
    assert_eq!(written.trim(), daemon.fingerprint);

    // myko, through the TLS listener: the upgrade succeeds and a ping is
    // answered. With --tls, myko wants a token (MAR-24): the host's own.
    let token_path = daemon
        ._state
        .path()
        .join("home/.config/marshal/local-token");
    assert!(token_path.exists(), "no local token was written");
    let local_token = std::fs::read_to_string(token_path)
        .unwrap()
        .trim()
        .to_string();
    let tls = tls_connect(daemon.public, &daemon.fingerprint)
        .await
        .unwrap();
    let mut request = format!("wss://{}/myko", daemon.public)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {local_token}").parse().unwrap(),
    );
    let (mut ws, response) = tokio_tungstenite::client_async(request, tls)
        .await
        .expect("WebSocket upgrade through TLS");
    assert_eq!(response.status(), 101);
    let ping = serde_json::json!({"event": "ws:m:ping", "data": {"id": "mar-23", "timestamp": 0}});
    ws.send(Message::Text(ping.to_string().into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = ws.next().await {
            if let Message::Text(text) = frame.unwrap()
                && text.contains("mar-23")
            {
                return text.to_string();
            }
        }
        panic!("the socket closed before the ping was answered");
    })
    .await
    .expect("a ping answered through TLS");
    assert!(reply.contains("ws:m:ping"), "{reply}");

    // The hook listener, through its TLS listener: a plain HTTP request over
    // TLS gets an HTTP answer.
    let mut tls = tls_connect(daemon.hooks, &daemon.fingerprint)
        .await
        .unwrap();
    tls.write_all(b"GET /hook/none HTTP/1.1\r\nHost: marshal\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), tls.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 "), "{answer}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_pin_is_refused_in_the_handshake() {
    let daemon = start();
    let wrong = "0".repeat(64);
    for addr in [daemon.public, daemon.hooks] {
        let err = tls_connect(addr, &wrong)
            .await
            .expect_err("a wrong pin connected");
        assert!(err.to_string().contains("certificate"), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_websocket_gets_nothing_on_the_tls_port() {
    let daemon = start();
    let attempt = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(format!("ws://{}/myko", daemon.public)),
    )
    .await;
    assert!(
        !matches!(attempt, Ok(Ok(_))),
        "a plain WebSocket upgraded on the TLS port"
    );
}

#[test]
fn tls_and_plain_together_are_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_marshal-daemon"))
        .args(["--tls", "--plain"])
        .env("MARSHAL_BIND", "127.0.0.1:0")
        .env("MARSHAL_HOOK_BIND", "127.0.0.1:0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't both be given"));
}
