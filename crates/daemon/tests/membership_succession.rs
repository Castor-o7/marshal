//! MAR-19 with MAR-9: the sweeper drops an ended session's room memberships,
//! but keeps its adhoc ones while a later session may still take it over, so
//! the takeover can move them. In a test binary of its own, because
//! `succession::observe` replaces the daemon-wide view of the roster.

use std::{collections::HashSet, sync::Arc};

use daemon::{cleanup::sweep_memberships, nickname_assign::ensure_assigned, succession};
use hyphae::{Gettable, Materialize};
use marshal_entities::{
    AutoSource, HostInfo, Room, RoomId, RoomKind, RoomMember, RoomMemberId, Session, SessionId,
};
use myko::{
    command::CommandContext,
    core::item::Eventable,
    entities::client::ClientId,
    server::{MykoServerContext, Persister},
    utils::downcast_item,
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

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn session(cwd: &str, identity: Option<&str>, client: &str) -> Session {
    Session {
        id: SessionId(Arc::from(Uuid::new_v4().to_string().as_str())),
        client_id: Some(ClientId(Arc::from(client))),
        agent_id: None,
        pid: 1,
        cwd: cwd.into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        activity_at: None,
        kind: None,
        connected_at: now() - 60_000,
        last_activity_at: Some(now() - 60_000),
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

fn start(ctx: &MykoServerContext, s: &Session) -> String {
    ctx.set(s).expect("session SET");
    ensure_assigned(&cmd(ctx), s.id.0.as_ref()).expect("named")
}

fn room(ctx: &MykoServerContext, id: &str, kind: RoomKind) {
    ctx.set(&Room {
        id: RoomId(Arc::from(id)),
        name: id.to_string(),
        description: None,
        kind,
        created_at: 1,
    })
    .expect("room SET");
}

fn join(ctx: &MykoServerContext, room: &str, s: &Session) {
    ctx.set(&RoomMember {
        id: RoomMemberId(Arc::from(
            RoomMember::make_id(room, s.id.0.as_ref()).as_str(),
        )),
        room_id: RoomId(Arc::from(room)),
        session_id: s.id.clone(),
        joined_at: 100,
    })
    .expect("membership SET");
}

/// The sweeper reaps the session: its row goes, and the daemon sees it go.
fn reap(ctx: &MykoServerContext, s: &Session) {
    ctx.del_by_id(Session::ENTITY_NAME_STATIC, s.id.0.as_ref())
        .expect("session DEL");
    succession::record_removed(s.id.0.as_ref(), now());
}

fn member(ctx: &MykoServerContext, room: &str, s: &Session) -> bool {
    let store = ctx.registry.get(RoomMember::ENTITY_NAME_STATIC).unwrap();
    store.entries().materialize().get().iter().any(|(_, item)| {
        downcast_item::<RoomMember>(item)
            .is_some_and(|m| m.room_id.0.as_ref() == room && m.session_id == s.id)
    })
}

fn everyone(ctx: &MykoServerContext) {
    room(
        ctx,
        "everyone",
        RoomKind::Auto {
            source: AutoSource::Everyone,
        },
    );
}

/// A successor registers while its predecessor's client dropped only moments
/// ago, so the takeover is left for the sweeper's late check. The predecessor
/// is reaped and its memberships swept before that check runs: the adhoc one
/// is kept for the takeover, which moves it, and goes once it has.
#[test]
fn a_reaped_predecessor_keeps_its_adhoc_room_for_a_late_takeover() {
    let ctx = setup();
    everyone(&ctx);
    let cwd = format!("/work/{}", Uuid::new_v4());
    let adhoc = format!("design-{}", Uuid::new_v4());
    room(&ctx, &adhoc, RoomKind::Adhoc);

    let old = session(&cwd, None, "c-old");
    let nick = start(&ctx, &old);
    join(&ctx, "everyone", &old);
    join(&ctx, &adhoc, &old);
    // Its client has just dropped: inside the grace, so it's pending, not dead.
    succession::observe(
        &[(old.id.0.clone(), Some(Arc::from("c-old")))],
        &HashSet::new(),
        now(),
    );

    let new = session(&cwd, None, "c-new");
    assert_ne!(
        start(&ctx, &new),
        nick,
        "inside the grace, the successor gets a fresh nickname first"
    );

    reap(&ctx, &old);
    sweep_memberships(&ctx);
    assert!(
        !member(&ctx, "everyone", &old),
        "an auto-room membership goes at once"
    );
    assert!(
        member(&ctx, &adhoc, &old),
        "the adhoc one is kept while a later session may take it over"
    );

    succession::recheck_pending(&cmd(&ctx), now());
    assert!(
        member(&ctx, &adhoc, &new),
        "the late takeover moved the adhoc room to the successor"
    );

    sweep_memberships(&ctx);
    assert!(
        !member(&ctx, &adhoc, &old),
        "once taken over, the predecessor's own membership goes"
    );
    assert!(member(&ctx, &adhoc, &new), "the successor's stays");
}

/// A session that opted out of identity can't be taken over, so nothing waits
/// for a successor: its adhoc membership goes with its row.
#[test]
fn a_session_no_one_can_take_over_leaves_its_adhoc_rooms_at_once() {
    let ctx = setup();
    let adhoc = format!("design-{}", Uuid::new_v4());
    room(&ctx, &adhoc, RoomKind::Adhoc);
    let s = session(&format!("/work/{}", Uuid::new_v4()), Some("none"), "c-s");
    start(&ctx, &s);
    join(&ctx, &adhoc, &s);

    reap(&ctx, &s);
    sweep_memberships(&ctx);
    assert!(!member(&ctx, &adhoc, &s));
}
