//! MAR-46 (from muster's MUS-105): `marshal-daemon forget` removes a session's
//! roster row, holds its hooks off so a stale waiter can't put it straight
//! back, and refuses a target that names several sessions.

use std::sync::Arc;

use marshal_entities::{GetAllSessions, SessionNickname, SessionNicknameId};
use myko::{
    command::CommandContext,
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

fn query_ctx(ctx: &MykoServerContext) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        None,
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from("test"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn on_roster(ctx: &MykoServerContext) -> Vec<String> {
    let mut ids: Vec<String> = query_ctx(ctx)
        .exec_query(GetAllSessions {})
        .unwrap_or_default()
        .iter()
        .map(|s| s.id.0.to_string())
        .collect();
    ids.sort();
    ids
}

/// What a hook does: register (or refresh) the session.
fn hook(ctx: &Arc<MykoServerContext>, sid: &str) {
    let body = format!(r#"{{"session_id":"{sid}","cwd":"/repo"}}"#);
    daemon::hooks::dispatch(
        "/hook/session-start",
        "harness=claude",
        body.as_bytes(),
        ctx,
    );
}

fn nickname(ctx: &MykoServerContext, sid: &str, nick: &str) {
    let item = SessionNickname {
        id: SessionNicknameId(Arc::from(sid)),
        nickname: nick.into(),
    };
    let event = MEvent::from_item(&item, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event]).expect("apply SET");
}

#[test]
fn forget_removes_a_session_holds_its_hooks_off_and_refuses_an_ambiguous_target() {
    let ctx = setup();
    let state = tempfile::tempdir().unwrap();
    let dir = state.path();
    let ghost = "aaaaaaaa-0000-4000-8000-000000000001";
    let twin_a = "bbbbbbbb-0000-4000-8000-000000000002";
    let twin_b = "cccccccc-0000-4000-8000-000000000003";
    for sid in [ghost, twin_a, twin_b] {
        hook(&ctx, sid);
    }
    nickname(&ctx, ghost, "mellow-ghost");
    nickname(&ctx, twin_a, "same-name");
    nickname(&ctx, twin_b, "same-name");
    assert_eq!(on_roster(&ctx), [ghost, twin_a, twin_b]);

    // Forget by nickname: the row goes, and the answer says so.
    let token = daemon::controls::request_forget(dir, "mellow-ghost").unwrap();
    daemon::controls::apply(dir, &ctx);
    assert_eq!(on_roster(&ctx), [twin_a, twin_b]);
    let (ok, text) = daemon::controls::forget_result(dir, &token).expect("an answer");
    assert!(ok, "{text}");
    assert!(text.contains(ghost), "{text}");

    // A stale waiter's hook can't put it back while the forget holds.
    assert!(daemon::controls::is_forgotten(ghost));
    hook(&ctx, ghost);
    assert_eq!(on_roster(&ctx), [twin_a, twin_b]);
    // ...and the next refresh keeps holding it, by id.
    daemon::controls::apply(dir, &ctx);
    assert!(daemon::controls::is_forgotten(ghost));

    // A nickname two sessions share is refused, naming both, and removes nothing.
    let token = daemon::controls::request_forget(dir, "same-name").unwrap();
    daemon::controls::apply(dir, &ctx);
    let (ok, text) = daemon::controls::forget_result(dir, &token).expect("an answer");
    assert!(!ok, "{text}");
    assert!(text.contains(twin_a) && text.contains(twin_b), "{text}");
    assert_eq!(on_roster(&ctx), [twin_a, twin_b]);
    assert!(!daemon::controls::is_forgotten(twin_a));
}
