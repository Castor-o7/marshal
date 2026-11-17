//! MAR-83: with auth on, succession stays within a principal. The daemon's
//! naming is driven directly, as identity_succession.rs does, with a
//! `SessionBinding` installed. The install is process-wide, which is why
//! this is its own test binary.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use daemon::{
    auth::session_binding::{self, SessionBinding},
    nickname_assign::ensure_assigned,
    succession,
};
use marshal_entities::{GetAllSuccessions, HostInfo, Session, SessionId};
use myko::{
    command::CommandContext,
    core::item::Eventable,
    server::{MykoServerContext, Persister},
};
use myko_auth::{
    AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer, OperationKind, Principal,
    PrincipalKind,
};
use myko_server::{BlackholePersister, MykoServer};
use serde_json::{Value, json};
use uuid::Uuid;

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

fn identity(subject: &str) -> AuthenticatedIdentity {
    AuthenticatedIdentity {
        user_id: None,
        principal: Principal {
            issuer: "mar-83-fixture".into(),
            subject: subject.into(),
            kind: PrincipalKind::Agent,
        },
        client_id: None,
        scopes: vec![],
        delegation: vec![],
        claims: Value::Null,
        expires_at: SystemTime::now() + Duration::from_secs(300),
    }
}

/// `principal` registers `session`, as its connection's RegisterSession would.
fn claim(binding: &SessionBinding<AllowAll>, principal: &str, session: &Session) {
    let arguments = json!({ "session": { "id": session.id.0.as_ref() } });
    binding
        .authorize(
            &identity(principal),
            AuthorizationRequest::Execute {
                kind: OperationKind::Command,
                operation: "RegisterSession",
                arguments: &arguments,
            },
        )
        .expect("the principal registers its own session");
}

fn setup() -> MykoServerContext {
    marshal_entities::link();
    daemon::link();
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    Box::leak(Box::new(server));
    ctx
}

fn session(cwd: &str) -> Session {
    let now = chrono::Utc::now().timestamp_millis();
    Session {
        id: SessionId(Arc::from(Uuid::new_v4().to_string().as_str())),
        client_id: None,
        agent_id: None,
        pid: 1,
        cwd: cwd.into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        activity_at: None,
        kind: None,
        connected_at: now,
        last_activity_at: Some(now),
        last_tool: None,
        last_tool_at: None,
        operator: Some("op@example.com".into()),
        host: Some(HostInfo {
            name: "a1".into(),
            os: String::new(),
            arch: String::new(),
        }),
        project: None,
        channels_enabled: None,
        acks_pushes: None,
        harness: Some("claude".into()),
        identity: None,
    }
}

fn cmd(ctx: &MykoServerContext) -> CommandContext {
    succession::internal_ctx(ctx)
}

/// Register a session and name it, as the session-start hook does.
fn start(ctx: &MykoServerContext, s: &Session) -> String {
    ctx.set(s).expect("session SET");
    ensure_assigned(&cmd(ctx), s.id.0.as_ref()).expect("named")
}

/// The session ends: its row goes, the daemon sees it go, and the sweep
/// releases its binding, all before any successor asks.
fn end(ctx: &MykoServerContext, s: &Session) {
    ctx.del_by_id(Session::ENTITY_NAME_STATIC, s.id.0.as_ref())
        .expect("session DEL");
    succession::record_removed(s.id.0.as_ref(), chrono::Utc::now().timestamp_millis());
    session_binding::release(s.id.0.as_ref());
}

fn succeeded(ctx: &MykoServerContext, predecessor: &Session) -> Option<String> {
    cmd(ctx)
        .exec_query(GetAllSuccessions {})
        .expect("successions")
        .iter()
        .find(|s| s.id.0.as_ref() == predecessor.id.0.as_ref())
        .map(|s| s.to_session_id.clone())
}

#[test]
fn succession_stays_within_a_principal() {
    let binding = SessionBinding::new(AllowAll, None);
    binding.install();
    let ctx = setup();
    let cwd = format!("/work/{}", Uuid::new_v4());

    // Alpha's session, named, then dead and released.
    let old = session(&cwd);
    claim(&binding, "alpha", &old);
    let nick = start(&ctx, &old);
    end(&ctx, &old);

    // Beta's session with the same key doesn't take it over.
    let intruder = session(&cwd);
    claim(&binding, "beta", &intruder);
    let intruder_nick = start(&ctx, &intruder);
    assert_ne!(
        intruder_nick, nick,
        "a different principal keeps its own name"
    );
    assert_eq!(succeeded(&ctx, &old), None, "no succession is recorded");
    end(&ctx, &intruder);

    // Alpha's restart does, with the record saying so.
    let restarted = session(&cwd);
    claim(&binding, "alpha", &restarted);
    assert_eq!(
        start(&ctx, &restarted),
        nick,
        "the same principal's restart takes over"
    );
    assert_eq!(
        succeeded(&ctx, &old).as_deref(),
        Some(restarted.id.0.as_ref())
    );

    // A predecessor whose principal is unknown, as after a daemon restart, is
    // nobody's to succeed: unknown counts as different.
    let other_cwd = format!("/work/{}", Uuid::new_v4());
    let orphan = session(&other_cwd);
    let orphan_nick = start(&ctx, &orphan);
    end(&ctx, &orphan);
    let hopeful = session(&other_cwd);
    claim(&binding, "alpha", &hopeful);
    assert_ne!(start(&ctx, &hopeful), orphan_nick);
    assert_eq!(succeeded(&ctx, &orphan), None);
}
