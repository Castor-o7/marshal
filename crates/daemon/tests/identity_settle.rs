//! MUS-92: a restart whose predecessor still looked live when the new session
//! registered. In its own test binary, because the sweeper's observations
//! (`succession::observe`) are process-wide.

use std::sync::Arc;

use daemon::{nickname_assign::ensure_assigned, succession};
use marshal_entities::{HostInfo, Session, SessionId};
use myko::command::CommandContext;
use myko::{
    entities::client::ClientId,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};
use uuid::Uuid;

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

fn session(cwd: &str, identity: Option<&str>, client: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(Uuid::new_v4().to_string().as_str())),
        client_id: client.map(|c| ClientId(Arc::from(c))),
        agent_id: None,
        pid: 1,
        cwd: cwd.into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        activity_at: None,
        kind: None,
        connected_at: chrono::Utc::now().timestamp_millis(),
        last_activity_at: Some(chrono::Utc::now().timestamp_millis()),
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
        identity: identity.map(str::to_string),
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

fn unique_cwd() -> String {
    format!("/work/{}", Uuid::new_v4())
}

#[test]
fn a_predecessor_that_looked_live_at_registration_is_still_taken_over() {
    let ctx = setup();
    let cwd = unique_cwd();
    let client = Uuid::new_v4().to_string();
    let cid: Arc<str> = Arc::from(client.as_str());
    let old = session(&cwd, None, Some(&client));
    let t0 = chrono::Utc::now().timestamp_millis();
    // The old session's client is still connected when the new one starts,
    // as when a new Claude starts before the old one's socket has closed.
    succession::observe(
        &[(old.id.0.clone(), Some(cid.clone()))],
        &[cid.clone()].into_iter().collect(),
        t0,
    );
    let nick = start(&ctx, &old);
    let new = session(&cwd, None, None);
    let fresh = start(&ctx, &new);
    assert_ne!(
        fresh, nick,
        "a live predecessor isn't taken over at registration"
    );

    // Then the old client drops, and the sweeper sees it.
    let t1 = chrono::Utc::now().timestamp_millis();
    succession::observe(
        &[
            (old.id.0.clone(), Some(cid.clone())),
            (new.id.0.clone(), None),
        ],
        &Default::default(),
        t1,
    );
    // Past the grace (30 s by default), the late check takes it over.
    succession::recheck_pending(&cmd(&ctx), t1 + 31_000);
    assert_eq!(
        ensure_assigned(&cmd(&ctx), new.id.0.as_ref()).unwrap(),
        nick,
        "once its predecessor counts as dead, the new session takes its nickname"
    );

    // The other side, in the same test because the sweeper's state is shared:
    // a sibling that's live when the new session starts, and stays live past
    // the settle window (60 s by default), keeps its nickname even if it ends
    // later.
    let cwd = unique_cwd();
    let client = Uuid::new_v4().to_string();
    let cid: Arc<str> = Arc::from(client.as_str());
    let sibling = session(&cwd, None, Some(&client));
    let t0 = chrono::Utc::now().timestamp_millis();
    succession::observe(
        &[(sibling.id.0.clone(), Some(cid.clone()))],
        &[cid.clone()].into_iter().collect(),
        t0,
    );
    let sibling_nick = start(&ctx, &sibling);
    let later = session(&cwd, None, None);
    let later_nick = start(&ctx, &later);
    let t1 = t0 + 61_000;
    succession::observe(
        &[
            (sibling.id.0.clone(), Some(cid.clone())),
            (later.id.0.clone(), None),
        ],
        &Default::default(),
        t1,
    );
    succession::recheck_pending(&cmd(&ctx), t1 + 31_000);
    assert_eq!(
        ensure_assigned(&cmd(&ctx), later.id.0.as_ref()).unwrap(),
        later_nick,
        "a sibling that outlived the settle window isn't taken over"
    );
    assert_ne!(later_nick, sibling_nick);
}
