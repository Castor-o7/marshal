//! MAR-80: with myko#129's provider on, a connection acts only as its own
//! sessions. A myko server with Marshal's entities runs behind the provider,
//! with `SessionBinding` as its authorizer, and two principals try to act for
//! each other's sessions. Plain mode (no provider) is checked unchanged.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use daemon::auth::session_binding::SessionBinding;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use marshal_entities::{
    GetAllSessionNicknames, GetAllSessions, RegisterSession, SendMessage, Session, SessionId,
};
use myko::{
    core::item::Eventable,
    server::Persister,
    wire::{MEvent, MEventType, MykoMessage, command::WrappedCommand},
};
use myko_auth::{
    AuthError, AuthenticatedIdentity, Authenticator, AuthorizationRequest, Authorizer, Credentials,
    MykoAuthProvider, Principal, PrincipalKind,
};
use myko_server::{
    BlackholePersister, GatewayAccess, MykoServer,
    router::{route_connection, route_connection_with_auth},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const ISSUER: &str = "mar-80-fixture";

/// Tokens are the principal's own name.
struct Tokens;

impl Authenticator for Tokens {
    fn authenticate(
        &self,
        credentials: Credentials,
    ) -> BoxFuture<'_, Result<AuthenticatedIdentity, AuthError>> {
        Box::pin(async move {
            let subject = credentials
                .bearer
                .filter(|token| ["alice", "bob", "murph-local"].contains(&token.as_str()))
                .ok_or(AuthError::Unauthenticated)?;
            Ok(AuthenticatedIdentity {
                user_id: None,
                principal: principal(&subject),
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

    fn validate_identity(&self, _identity: &AuthenticatedIdentity) -> Result<(), AuthError> {
        Ok(())
    }
}

fn principal(subject: &str) -> Principal {
    Principal {
        issuer: ISSUER.into(),
        subject: subject.into(),
        kind: PrincipalKind::Agent,
    }
}

/// The provider's own policy, standing in for whatever MAR-24 installs:
/// everything a token holder asks for is allowed, so what's refused below is
/// the binding's doing.
struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(
        &self,
        _identity: &AuthenticatedIdentity,
        _request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}

struct Daemon {
    addr: SocketAddr,
    server: Arc<MykoServer>,
    binding: Option<Arc<SessionBinding<AllowAll>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A myko server with Marshal's entities linked in. With `auth`, it sits
/// behind #129's provider with a `SessionBinding` (and `local` as the
/// daemon host's principal); without, it's today's plain mode.
async fn daemon(auth: bool, local: Option<&str>) -> TestResult<Daemon> {
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = Arc::new(
        MykoServer::builder()
            .with_default_persister(blackhole)
            .build(),
    );
    let binding =
        auth.then(|| Arc::new(SessionBinding::new(AllowAll, local.map(principal).as_ref())));
    let provider = binding.clone().map(|binding| {
        Arc::new(MykoAuthProvider::new(
            Arc::new(Tokens) as Arc<dyn Authenticator>,
            binding as Arc<dyn Authorizer>,
        ))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepting = server.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let ctx = Arc::new(accepting.ctx());
            let info = accepting.server_info();
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
    Ok(Daemon {
        addr,
        server,
        binding,
        task,
    })
}

async fn connect(daemon: &Daemon, token: Option<&str>) -> TestResult<Socket> {
    let mut request = format!("ws://{}/myko", daemon.addr).into_client_request()?;
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse()?);
    }
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

fn session(id: &str, pid: u32) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: None,
        agent_id: None,
        pid,
        cwd: format!("/work/{id}"),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        activity_at: None,
        kind: None,
        connected_at: 1,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
        acks_pushes: None,
        harness: None,
        identity: None,
    }
}

/// Send a command and return the reply's event name and error message
/// (empty for a response).
async fn command(
    socket: &mut Socket,
    command_id: &str,
    mut arguments: Value,
    tx: &str,
) -> TestResult<(String, String)> {
    arguments
        .as_object_mut()
        .ok_or("command is not an object")?
        .insert("tx".into(), tx.into());
    let message = MykoMessage::Command(WrappedCommand {
        command_id: command_id.into(),
        command: arguments,
    });
    socket
        .send(Message::Text(serde_json::to_string(&message)?.into()))
        .await?;
    timeout(Duration::from_secs(10), reply_to(socket, tx)).await?
}

async fn register(socket: &mut Socket, id: &str, pid: u32) -> TestResult<(String, String)> {
    let arguments = serde_json::to_value(RegisterSession {
        session: session(id, pid),
    })?;
    command(
        socket,
        "RegisterSession",
        arguments,
        &format!("register-{id}-{pid}"),
    )
    .await
}

async fn set_activity(socket: &mut Socket, id: &str, tx: &str) -> TestResult<(String, String)> {
    command(
        socket,
        "SetSessionActivity",
        json!({ "id": id, "activity": "busy" }),
        tx,
    )
    .await
}

async fn send_as(
    socket: &mut Socket,
    as_session: &str,
    to: &str,
    tx: &str,
) -> TestResult<(String, String)> {
    let arguments = serde_json::to_value(SendMessage {
        to_session_id: SessionId(Arc::from(to)),
        response_required: false,
        body: "MAR-80 probe".into(),
        as_session: Some(SessionId(Arc::from(as_session))),
        idempotency_key: None,
    })?;
    command(socket, "SendMessage", arguments, tx).await
}

/// A raw client event, as the shim's registration fallback sends one. There
/// is no reply: a refused event is logged server-side and not applied.
async fn send_event(socket: &mut Socket, item: &Session, change: MEventType) -> TestResult {
    let event = MEvent::from_item(item, change, &uuid::Uuid::new_v4().to_string());
    socket
        .send(Message::Text(
            serde_json::to_string(&MykoMessage::Event(event))?.into(),
        ))
        .await?;
    Ok(())
}

/// A raw client event for an entity the fixture can't build directly: a
/// Session event with its type and item swapped on the wire.
async fn send_event_as(socket: &mut Socket, entity_type: &str, item: Value) -> TestResult {
    let event = MEvent::from_item(
        &session("retyped", 0),
        MEventType::SET,
        &uuid::Uuid::new_v4().to_string(),
    );
    let mut wire = serde_json::to_value(MykoMessage::Event(event))?;
    fn retype(value: &mut Value, entity_type: &str, item: &Value) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map.iter_mut() {
                    if key == "item" {
                        *inner = item.clone();
                    } else if inner.as_str() == Some(Session::ENTITY_NAME_STATIC) {
                        *inner = Value::String(entity_type.to_owned());
                    } else {
                        retype(inner, entity_type, item);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| retype(v, entity_type, item)),
            _ => {}
        }
    }
    retype(&mut wire, entity_type, &item);
    socket
        .send(Message::Text(serde_json::to_string(&wire)?.into()))
        .await?;
    Ok(())
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

fn ok() -> (String, String) {
    ("ws:m:command-response".into(), String::new())
}

fn denied() -> (String, String) {
    ("ws:m:command-error".into(), "permission denied".into())
}

fn sessions(daemon: &Daemon) -> Vec<Arc<Session>> {
    daemon::succession::internal_ctx(&daemon.server.ctx())
        .exec_query(GetAllSessions {})
        .expect("sessions")
}

fn pid_of(daemon: &Daemon, id: &str) -> Option<u32> {
    sessions(daemon)
        .iter()
        .find(|s| s.id.0.as_ref() == id)
        .map(|s| s.pid)
}

/// Today's default: no provider, so any connection acts as any session.
#[tokio::test(flavor = "multi_thread")]
async fn plain_mode_is_unchanged() -> TestResult {
    let plain = daemon(false, None).await?;
    let mut alice = connect(&plain, None).await?;
    let mut bob = connect(&plain, None).await?;
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    assert_eq!(set_activity(&mut bob, "s1", "plain-1").await?, ok());
    assert_eq!(
        register(&mut bob, "s1", 2).await?,
        ok(),
        "a re-register by anyone"
    );
    assert_eq!(pid_of(&plain, "s1"), Some(2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_principal_acts_only_as_its_own_sessions_and_keeps_its_connection() -> TestResult {
    let guarded = daemon(true, None).await?;
    let mut alice = connect(&guarded, Some("alice")).await?;
    let mut bob = connect(&guarded, Some("bob")).await?;
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    assert_eq!(register(&mut bob, "s2", 2).await?, ok());

    assert_eq!(set_activity(&mut alice, "s1", "own-1").await?, ok());
    assert_eq!(set_activity(&mut alice, "s2", "theirs-1").await?, denied());
    assert_eq!(
        send_as(&mut alice, "s2", "s1", "as-theirs").await?,
        denied()
    );
    assert_eq!(send_as(&mut alice, "s1", "s2", "as-own").await?, ok());
    // The refusals ended nothing: alice's next own command goes through.
    assert_eq!(set_activity(&mut alice, "s1", "own-2").await?, ok());
    assert_eq!(set_activity(&mut bob, "s2", "bob-own").await?, ok());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_not_taken_over_by_another_principal() -> TestResult {
    let guarded = daemon(true, None).await?;
    let mut alice = connect(&guarded, Some("alice")).await?;
    let mut bob = connect(&guarded, Some("bob")).await?;
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    assert_eq!(
        register(&mut bob, "s1", 2).await?,
        denied(),
        "RegisterSession"
    );
    // The shim's fallback, a raw Session SET, is refused the same way. Bob's
    // own command afterwards shows the event was handled before it.
    send_event(&mut bob, &session("s1", 3), MEventType::SET).await?;
    assert_eq!(register(&mut bob, "s2", 2).await?, ok());
    assert_eq!(pid_of(&guarded, "s1"), Some(1), "s1 is still alice's row");
    // Alice's own restart re-registers it.
    assert_eq!(register(&mut alice, "s1", 4).await?, ok());
    assert_eq!(pid_of(&guarded, "s1"), Some(4));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemons_own_entities_are_refused_to_a_client() -> TestResult {
    let guarded = daemon(true, None).await?;
    let mut alice = connect(&guarded, Some("alice")).await?;
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    send_event_as(
        &mut alice,
        "SessionNickname",
        json!({ "id": "s1", "nickname": "evil-name" }),
    )
    .await?;
    assert_eq!(set_activity(&mut alice, "s1", "flush").await?, ok());
    let nicknames = daemon::succession::internal_ctx(&guarded.server.ctx())
        .exec_query(GetAllSessionNicknames {})
        .expect("nicknames");
    assert!(
        nicknames.iter().all(|n| n.nickname != "evil-name"),
        "a client wrote a nickname: {nicknames:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_goes_when_its_session_ends() -> TestResult {
    let guarded = daemon(true, None).await?;
    let binding = guarded.binding.clone().expect("binding");
    let mut alice = connect(&guarded, Some("alice")).await?;
    let mut bob = connect(&guarded, Some("bob")).await?;
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    assert!(binding.table().owner_of("s1").is_some());
    // Bob can't end it; alice can, and that releases it.
    send_event(&mut bob, &session("s1", 1), MEventType::DEL).await?;
    assert_eq!(register(&mut bob, "s2", 2).await?, ok());
    assert_eq!(pid_of(&guarded, "s1"), Some(1));
    send_event(&mut alice, &session("s1", 1), MEventType::DEL).await?;
    assert_eq!(set_activity(&mut alice, "s1", "after-del").await?, denied());
    assert!(
        binding.table().owner_of("s1").is_none(),
        "the DEL released it"
    );
    assert_eq!(pid_of(&guarded, "s1"), None);
    assert_eq!(
        register(&mut bob, "s1", 5).await?,
        ok(),
        "the id is free again"
    );
    Ok(())
}

/// Codex sessions, and Claude's hook-registered ones, never pass through
/// myko. They belong to the configured local principal, so the daemon host's
/// own processes act for them and a remote principal can't. The local
/// principal is still just a principal: it can't act for a session a remote
/// one registered over myko. With no local principal, hook-registered
/// sessions are nobody's.
#[tokio::test(flavor = "multi_thread")]
async fn hook_registered_sessions_belong_to_the_local_principal() -> TestResult {
    let guarded = daemon(true, Some("murph-local")).await?;
    let binding = guarded.binding.clone().expect("binding");
    // As the hook listener does: the row goes in, then the binding.
    let ctx = daemon::succession::internal_ctx(&guarded.server.ctx());
    marshal_entities::update_registered_session(&ctx, &SessionId(Arc::from("codex-1")), |_| {
        session("codex-1", 7)
    })
    .map_err(|e| format!("{e:?}"))?;
    assert!(binding.bind_local("codex-1"));
    let mut local = connect(&guarded, Some("murph-local")).await?;
    let mut alice = connect(&guarded, Some("alice")).await?;
    assert_eq!(
        set_activity(&mut local, "codex-1", "local-own").await?,
        ok()
    );
    assert_eq!(
        set_activity(&mut alice, "codex-1", "remote").await?,
        denied()
    );
    assert_eq!(register(&mut alice, "s1", 1).await?, ok());
    assert_eq!(
        set_activity(&mut local, "s1", "local-theirs").await?,
        denied()
    );
    assert!(!binding.bind_local("s1"), "binding doesn't steal a held id");

    let unconfigured = daemon(true, None).await?;
    let binding = unconfigured.binding.clone().expect("binding");
    let ctx = daemon::succession::internal_ctx(&unconfigured.server.ctx());
    marshal_entities::update_registered_session(&ctx, &SessionId(Arc::from("codex-2")), |_| {
        session("codex-2", 8)
    })
    .map_err(|e| format!("{e:?}"))?;
    assert!(!binding.bind_local("codex-2"));
    let mut local = connect(&unconfigured, Some("murph-local")).await?;
    assert_eq!(
        set_activity(&mut local, "codex-2", "orphan").await?,
        denied()
    );
    Ok(())
}
