//! MAR-37: Marshal's commands behind myko#129's auth provider, tried
//! against muster's four attackers. The test authenticator
//! and authorizer here are fixtures, not a design for Marshal's own. Each test
//! asserts what #129 does today, including the gaps, so a gap shows up as a
//! passing assertion that's labelled as one.

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use marshal_entities::{SendMessage, SessionId};
use myko::{
    server::Persister,
    wire::{MykoMessage, command::WrappedCommand},
};
use myko_auth::{
    AuthError, AuthenticatedIdentity, Authenticator, AuthorizationRequest, Authorizer, Credentials,
    MykoAuthProvider, OperationKind, Principal, PrincipalKind,
};
use myko_server::{
    BlackholePersister, GatewayAccess, MykoServer,
    router::{route_connection, route_connection_with_auth},
};
use serde_json::Value;
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Tokens are the subject's own name; `revoked` stands in for an unpaired machine.
#[derive(Default)]
struct Tokens {
    revoked: Mutex<HashSet<String>>,
}

impl Authenticator for Tokens {
    fn authenticate(
        &self,
        credentials: Credentials,
    ) -> BoxFuture<'_, Result<AuthenticatedIdentity, AuthError>> {
        Box::pin(async move {
            let subject = credentials
                .bearer
                .filter(|token| token == "alice" || token == "bob")
                .ok_or(AuthError::Unauthenticated)?;
            Ok(AuthenticatedIdentity {
                principal: Principal {
                    issuer: "mar-37-fixture".into(),
                    subject,
                    kind: PrincipalKind::Agent,
                },
                client_id: None,
                scopes: vec![],
                delegation: vec![],
                claims: Value::Null,
                expires_at: SystemTime::now()
                    .checked_add(Duration::from_secs(300))
                    .ok_or(AuthError::Unavailable)?,
            })
        })
    }

    fn validate_identity(&self, identity: &AuthenticatedIdentity) -> Result<(), AuthError> {
        let revoked = self
            .revoked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if revoked.contains(&identity.principal.subject) {
            Err(AuthError::Revoked)
        } else {
            Ok(())
        }
    }
}

/// `OwnSessionsOnly` is the check Marshal would add: a command's `asSession`
/// must be the caller's own. `AllowAll` is a provider with no such check.
#[derive(Clone, Copy)]
enum Policy {
    AllowAll,
    OwnSessionsOnly,
}

impl Authorizer for Policy {
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        if let (
            Self::OwnSessionsOnly,
            AuthorizationRequest::Execute {
                kind: OperationKind::Command,
                arguments,
                ..
            },
        ) = (self, request)
            && let Some(as_session) = arguments.get("asSession").and_then(Value::as_str)
            && as_session != identity.principal.subject
        {
            return Err(AuthError::Forbidden);
        }
        Ok(())
    }
}

struct Daemon {
    addr: SocketAddr,
    tokens: Arc<Tokens>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A myko server with Marshal's entities linked in, behind #129's provider
/// (`policy` is `Some`) or with no provider at all (`None`, today's default).
async fn daemon(policy: Option<Policy>) -> TestResult<Daemon> {
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = Arc::new(
        MykoServer::builder()
            .with_default_persister(blackhole)
            .build(),
    );
    let tokens = Arc::new(Tokens::default());
    let provider = policy.map(|policy| {
        Arc::new(MykoAuthProvider::new(
            tokens.clone() as Arc<dyn Authenticator>,
            Arc::new(policy),
        ))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let ctx = Arc::new(server.ctx());
            let info = server.server_info();
            let provider = provider.clone();
            tokio::spawn(async move {
                let _ = match provider {
                    Some(provider) => {
                        route_connection_with_auth(
                            stream,
                            peer,
                            ctx,
                            info,
                            GatewayAccess::Full,
                            provider,
                        )
                        .await
                    }
                    None => route_connection(stream, peer, ctx, info).await,
                };
            });
        }
    });
    Ok(Daemon { addr, tokens, task })
}

/// Connect as a browser page on another site would, or as a shim with a token.
async fn connect(
    daemon: &Daemon,
    token: Option<&str>,
    origin: Option<&str>,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{}/myko", daemon.addr)
        .into_client_request()
        .expect("request");
    if let Some(token) = token {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("header"),
        );
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().expect("header"));
    }
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

fn refused_with(result: Result<Socket, tokio_tungstenite::tungstenite::Error>) -> Option<u16> {
    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Some(response.status().as_u16())
        }
        _ => None,
    }
}

/// Send `SendMessage` claiming to be `as_session`, and return the reply's
/// event name and error message (empty for a response).
async fn send_as(socket: &mut Socket, as_session: &str, tx: &str) -> TestResult<(String, String)> {
    let mut command = serde_json::to_value(SendMessage {
        to_session_id: SessionId(Arc::from("carol")),
        response_required: false,
        body: "MAR-37 impersonation probe".into(),
        as_session: Some(SessionId(Arc::from(as_session))),
        idempotency_key: None,
    })?;
    command
        .as_object_mut()
        .ok_or("command is not an object")?
        .insert("tx".into(), tx.into());
    let message = MykoMessage::Command(WrappedCommand {
        command_id: "SendMessage".into(),
        command,
    });
    socket
        .send(Message::Text(serde_json::to_string(&message)?.into()))
        .await?;
    timeout(Duration::from_secs(10), reply_to(socket, tx)).await?
}

async fn reply_to(socket: &mut Socket, tx: &str) -> TestResult<(String, String)> {
    while let Some(frame) = socket.next().await {
        let Message::Text(text) = frame? else {
            continue;
        };
        let reply: Value = serde_json::from_str(&text)?;
        let event = reply["event"].as_str().unwrap_or_default();
        if (event == "ws:m:command-error" || event == "ws:m:command-response")
            && reply["data"]["tx"].as_str() == Some(tx)
        {
            let error = reply["data"]["message"].as_str().unwrap_or_default();
            return Ok((event.to_owned(), error.to_owned()));
        }
    }
    Err("socket closed before a reply".into())
}

async fn closes_within(socket: &mut Socket, wait: Duration) -> bool {
    timeout(wait, async {
        loop {
            match socket.next().await {
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .is_ok()
}

/// Attacker 1, a web page. Today (no provider) a page on any site can open
/// the socket. With #129's provider on, it's refused before the upgrade,
/// because a page has no token. Nothing checks `Origin` either way.
#[tokio::test(flavor = "multi_thread")]
async fn a_web_page_is_refused_once_a_provider_is_on() -> TestResult {
    let open = daemon(None).await?;
    assert!(
        connect(&open, None, Some("https://evil.example"))
            .await
            .is_ok(),
        "today's default: no provider, so a cross-site page is admitted"
    );
    let guarded = daemon(Some(Policy::AllowAll)).await?;
    assert_eq!(
        refused_with(connect(&guarded, None, Some("https://evil.example")).await),
        Some(401)
    );
    Ok(())
}

/// Attacker 3, another device on the LAN. A guessed token is refused. A good
/// token is accepted over plain `ws://`: #129 leaves TLS to the deployment.
#[tokio::test(flavor = "multi_thread")]
async fn a_lan_device_needs_a_real_token_but_plain_ws_is_accepted() -> TestResult {
    let guarded = daemon(Some(Policy::AllowAll)).await?;
    assert_eq!(
        refused_with(connect(&guarded, Some("guess"), None).await),
        Some(401)
    );
    assert!(
        connect(&guarded, Some("alice"), None).await.is_ok(),
        "gap: a bearer token over plain ws:// is accepted"
    );
    Ok(())
}

/// Attacker 2, one session claiming to be another. Without an authorizer that
/// compares `asSession` with the caller, alice's command reaches Marshal's
/// handler as bob. `authorize` sees the arguments, so the check fits there.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_can_claim_another_unless_the_authorizer_checks_as_session() -> TestResult {
    let permissive = daemon(Some(Policy::AllowAll)).await?;
    let mut alice = connect(&permissive, Some("alice"), None).await?;
    let (_, error) = send_as(&mut alice, "bob", "impersonate-1").await?;
    // Marshal's own resolver answered, so #129 let the command through as bob.
    // With bob on the roster, the send would have gone out from bob.
    assert!(
        error.contains("asSession 'bob' is not on the roster"),
        "gap: with no asSession check, the handler ran as bob (it said: {error:?})"
    );

    let checked = daemon(Some(Policy::OwnSessionsOnly)).await?;
    let mut alice = connect(&checked, Some("alice"), None).await?;
    let (event, error) = send_as(&mut alice, "bob", "impersonate-2").await?;
    assert_eq!(
        (event.as_str(), error.as_str()),
        ("ws:m:command-error", "permission denied")
    );
    let (_, own) = send_as(&mut alice, "alice", "own-session").await?;
    assert!(
        own.contains("asSession 'alice' is not on the roster"),
        "alice may act as alice, so her own command reaches the handler (it said: {own:?})"
    );
    assert!(
        !closes_within(&mut alice, Duration::from_millis(300)).await,
        "a denied command doesn't end the connection"
    );
    Ok(())
}

/// Attacker 4, a stolen machine. Revoking its credential ends its open
/// connection within the session's recheck interval, and a new connection with
/// the same token is refused.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_stolen_machines_token_closes_it_and_keeps_it_out() -> TestResult {
    let guarded = daemon(Some(Policy::AllowAll)).await?;
    let mut bob = connect(&guarded, Some("bob"), None).await?;
    let mut alice = connect(&guarded, Some("alice"), None).await?;
    guarded
        .tokens
        .revoked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert("bob".into());
    assert!(closes_within(&mut bob, Duration::from_secs(3)).await);
    assert!(
        !closes_within(&mut alice, Duration::from_millis(500)).await,
        "only the revoked credential's connection ends"
    );
    assert_eq!(
        refused_with(connect(&guarded, Some("bob"), None).await),
        Some(401)
    );
    Ok(())
}
