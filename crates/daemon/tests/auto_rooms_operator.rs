//! MAR-50 (from muster's MUS-40): a session whose operator changes leaves its old `op:` room, so
//! a broadcast there no longer reaches it. Runs `DispatchAutoRooms` against an in-memory daemon
//! cell, as the saga does on each Session SET.

use std::collections::BTreeSet;
use std::sync::Arc;

use daemon::auto_rooms::DispatchAutoRooms;
use hyphae::{Gettable, Materialize};
use marshal_entities::{RoomMember, Session};
use myko::{
    command::{CommandContext, CommandHandler},
    core::item::Eventable,
    request::RequestContext,
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
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

/// Built from JSON so only the fields this test cares about are named.
fn session(id: &str, operator: Option<&str>) -> Session {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "pid": 0,
        "cwd": "/repo",
        "connectedAt": 100,
        "operator": operator,
    }))
    .expect("a session")
}

/// SETs the session, then runs the auto-rooms command for it, as the saga would.
fn set_and_dispatch(ctx: &MykoServerContext, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
    let request = RequestContext::internal(
        Uuid::new_v4().to_string().into(),
        ctx.host_id,
        "auto-rooms-test",
    );
    let command = CommandContext::new(
        Arc::from("DispatchAutoRooms"),
        Arc::new(request),
        Arc::new(ctx.clone()),
    );
    DispatchAutoRooms {
        session_id: s.id.0.as_ref().to_string(),
    }
    .execute(command)
    .expect("auto rooms");
}

/// The rooms `session_id` is a member of.
fn rooms_of(ctx: &MykoServerContext, session_id: &str) -> BTreeSet<String> {
    let Some(store) = ctx.registry.get(RoomMember::ENTITY_NAME_STATIC) else {
        return BTreeSet::new();
    };
    store
        .entries()
        .materialize()
        .get()
        .into_iter()
        .filter_map(|(_, item)| myko::utils::downcast_item::<RoomMember>(&item))
        .filter(|m| m.session_id.0.as_ref() == session_id)
        .map(|m| m.room_id.0.to_string())
        .collect()
}

#[test]
fn a_session_whose_operator_changes_is_only_in_its_current_op_room() {
    let ctx = setup();
    set_and_dispatch(&ctx, &session("s", Some("josh")));
    set_and_dispatch(&ctx, &session("other", Some("josh")));
    assert_eq!(
        rooms_of(&ctx, "s"),
        BTreeSet::from(["everyone".to_owned(), "op:josh".to_owned()])
    );

    // The daemon begins attesting the operator as an address: the session moves rooms.
    set_and_dispatch(&ctx, &session("s", Some("josh@example.test")));
    assert_eq!(
        rooms_of(&ctx, "s"),
        BTreeSet::from(["everyone".to_owned(), "op:josh@example.test".to_owned()])
    );
    // Another session still under the old operator keeps its room.
    assert!(rooms_of(&ctx, "other").contains("op:josh"));

    // A SET that names no operator leaves its rooms as they are: nothing to compare against.
    set_and_dispatch(&ctx, &session("s", None));
    assert!(rooms_of(&ctx, "s").contains("op:josh@example.test"));
}
