//! `/hook/bridge-watch` (MAR-12, from muster's MUS-77): a Codex app-server bridge saying it can
//! wake a thread, so the roster shows the wake path (`waiting`) and a send to it says `expected`.

use std::sync::Arc;

use marshal_entities::{
    GetAllWakeWatchers, SendMessage, Session, SessionId, WakeStatus, WakeWatcher,
};
use myko::{
    command::{CommandContext, CommandHandler},
    core::item::Eventable,
    entities::client::ClientId,
    request::RequestContext,
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, MykoServer};
use uuid::Uuid;

fn setup() -> Arc<MykoServerContext> {
    marshal_entities::link();
    daemon::link();
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    Box::leak(Box::new(server));
    Arc::new(ctx)
}

fn session(id: &str, client: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: client.map(|c| ClientId(Arc::from(c))),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
        connected_at: 100,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
        acks_pushes: None,
        agent_id: None,
    }
}

fn set<T: Eventable>(ctx: &MykoServerContext, item: &T) {
    let event = MEvent::from_item(item, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event]).expect("apply SET");
}

fn cmd_ctx(ctx: &MykoServerContext, client: &str) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        Some(Arc::<str>::from(client)),
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from("SendMessage"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn watch(ctx: &Arc<MykoServerContext>, sid: &str, watching: bool) -> String {
    let body = format!(r#"{{"session_id":"{sid}","watching":{watching}}}"#);
    daemon::hooks::dispatch("/hook/bridge-watch", "harness=codex", body.as_bytes(), ctx)
        .expect("a known route")
        .body
}

fn watched(ctx: &MykoServerContext, sid: &str) -> bool {
    let now = chrono::Utc::now().timestamp_millis();
    cmd_ctx(ctx, "c-sender")
        .exec_query(GetAllWakeWatchers {})
        .unwrap_or_default()
        .iter()
        .any(|w: &Arc<WakeWatcher>| w.session() == sid && w.until > now)
}

fn send_wake(ctx: &MykoServerContext, to: &str) -> WakeStatus {
    SendMessage {
        to_session_id: SessionId(Arc::from(to)),
        response_required: false,
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    }
    .execute(cmd_ctx(ctx, "c-sender"))
    .expect("sent")
    .wake
}

#[test]
fn a_bridge_watched_thread_shows_its_wake_path_until_the_bridge_stops() {
    let ctx = setup();
    set(&ctx, &session("sender", Some("c-sender")));
    // A Codex thread: registered by its hooks, with no MCP client and no waiter.
    set(&ctx, &session("thread-1", None));

    assert!(!watched(&ctx, "thread-1"));
    assert_ne!(send_wake(&ctx, "thread-1"), WakeStatus::Expected);

    assert_eq!(watch(&ctx, "thread-1", true), "watching");
    assert!(
        watched(&ctx, "thread-1"),
        "the roster now has a WakeWatcher"
    );
    assert_eq!(send_wake(&ctx, "thread-1"), WakeStatus::Expected);

    // A session that isn't on the roster gets no record.
    assert_eq!(watch(&ctx, "nobody", true), "not watching");
    assert!(!watched(&ctx, "nobody"));

    // The bridge stops: the record goes.
    assert_eq!(watch(&ctx, "thread-1", false), "not watching");
    assert!(!watched(&ctx, "thread-1"));
    assert_ne!(send_wake(&ctx, "thread-1"), WakeStatus::Expected);

    // Watched again, it's back.
    assert_eq!(watch(&ctx, "thread-1", true), "watching");
    assert!(watched(&ctx, "thread-1"));
}

fn raw(ctx: &Arc<MykoServerContext>, body: &str) -> String {
    daemon::hooks::dispatch("/hook/bridge-watch", "harness=codex", body.as_bytes(), ctx)
        .expect("a known route")
        .body
}

fn hold_wait(ctx: &MykoServerContext, sid: &str) {
    set(
        ctx,
        &WakeWatcher {
            id: marshal_entities::WakeWatcherId(Arc::from(sid)),
            until: chrono::Utc::now().timestamp_millis() + 120_000,
            session_id: None,
        },
    );
}

#[test]
fn a_body_without_an_explicit_watching_bool_changes_nothing() {
    let ctx = setup();
    set(&ctx, &session("thread-2", None));
    hold_wait(&ctx, "thread-2");
    for body in [
        r#"{"session_id":"thread-2"}"#,
        r#"{"session_id":"thread-2","watching":"false"}"#,
        r#"{"session_id":"thread-2","watching":0}"#,
        r#"{"session_id":"thread-2","watching":null}"#,
        "not json",
    ] {
        assert_eq!(raw(&ctx, body), "ignored", "{body}");
        assert!(watched(&ctx, "thread-2"), "{body} must not end the record");
    }
}

#[test]
fn a_claude_sessions_record_is_left_to_its_own_waiter() {
    // A Claude session's row is bound to its shim's connection; its
    // WakeWatcher is its hook waiter's, which a bridge must not touch.
    let ctx = setup();
    set(&ctx, &session("claude-1", Some("c-claude")));
    hold_wait(&ctx, "claude-1");
    assert_eq!(watch(&ctx, "claude-1", false), "not watching");
    assert!(watched(&ctx, "claude-1"), "the waiter's record stays");

    set(&ctx, &session("claude-2", Some("c-claude-2")));
    assert_eq!(watch(&ctx, "claude-2", true), "not watching");
    assert!(!watched(&ctx, "claude-2"), "no record is made for it");
}

fn bridge(ctx: &Arc<MykoServerContext>, sid: &str, name: &str, watching: bool) -> String {
    raw(
        ctx,
        &format!(r#"{{"session_id":"{sid}","watching":{watching},"bridge":"{name}"}}"#),
    )
}

fn records_for(ctx: &MykoServerContext, sid: &str) -> Vec<String> {
    let mut ids: Vec<String> = cmd_ctx(ctx, "c-sender")
        .exec_query(GetAllWakeWatchers {})
        .unwrap_or_default()
        .iter()
        .filter(|w| w.session() == sid)
        .map(|w| w.id.0.to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_bridge_that_handed_over_leadership_ends_only_its_own_record() {
    // MAR-4: the old leader's `false` arrives after the new leader's `true`.
    let ctx = setup();
    set(&ctx, &session("thread-3", None));
    assert_eq!(bridge(&ctx, "thread-3", "new-leader", true), "watching");
    assert_eq!(
        bridge(&ctx, "thread-3", "old-leader", false),
        "not watching"
    );
    assert!(watched(&ctx, "thread-3"), "the new leader's record stays");
    assert_eq!(
        records_for(&ctx, "thread-3"),
        ["thread-3#bridge:new-leader"]
    );
}

#[test]
fn a_bridge_ending_its_watch_leaves_a_held_waits_record() {
    let ctx = setup();
    set(&ctx, &session("thread-4", None));
    hold_wait(&ctx, "thread-4");
    assert_eq!(bridge(&ctx, "thread-4", "b1", true), "watching");
    assert_eq!(
        records_for(&ctx, "thread-4").len(),
        2,
        "one record per path"
    );
    assert_eq!(bridge(&ctx, "thread-4", "b1", false), "not watching");
    assert_eq!(
        records_for(&ctx, "thread-4"),
        ["thread-4"],
        "the wait's stays"
    );
}

#[test]
fn a_session_that_ends_loses_every_wake_path() {
    let ctx = setup();
    set(&ctx, &session("thread-5", None));
    hold_wait(&ctx, "thread-5");
    assert_eq!(bridge(&ctx, "thread-5", "b1", true), "watching");
    daemon::hooks::dispatch(
        "/hook/session-end",
        "",
        br#"{"session_id":"thread-5"}"#,
        &ctx,
    )
    .expect("a known route");
    assert!(records_for(&ctx, "thread-5").is_empty());
}

#[test]
fn an_invalid_bridge_name_shares_the_plain_bridge_record() {
    let ctx = setup();
    set(&ctx, &session("thread-6", None));
    let long = "x".repeat(65);
    for name in [long.as_str(), "has space", "a#b"] {
        assert_eq!(bridge(&ctx, "thread-6", name, true), "watching", "{name}");
    }
    assert_eq!(records_for(&ctx, "thread-6"), ["thread-6#bridge:bridge"]);
}

#[test]
fn a_session_id_with_a_hash_is_not_registered() {
    // `#` names a bridge's wake-path record, so it's no session's id.
    let ctx = setup();
    daemon::hooks::dispatch(
        "/hook/session-register",
        "harness=codex",
        br#"{"session_id":"thread-7#bridge:b1","cwd":"/repo"}"#,
        &ctx,
    );
    let sessions = cmd_ctx(&ctx, "c-sender")
        .exec_query(marshal_entities::GetAllSessions {})
        .unwrap_or_default();
    assert!(!sessions.iter().any(|s| s.id.0.contains('#')));
}
