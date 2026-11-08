//! MUS-3: what the delivery conformance suite doesn't cover of succession.
//! The suite restarts real Claude and Codex fakes; these drive the daemon's
//! naming directly, for roles, the opt-out and room memberships.

use std::sync::Arc;

use daemon::{nickname_assign::ensure_assigned, succession};
use hyphae::{Gettable, Materialize};
use marshal_entities::{
    GetAllRoomMembers, GetAllSessionIdentitys, GetAllSuccessions, HostInfo, Message, Room, RoomId,
    RoomKind, RoomMember, RoomMemberId, SendMessage, Session, SessionId,
};
use myko::command::{CommandContext, CommandHandler};
use myko::{
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

/// The session ends: its row goes, and the daemon sees it go.
fn end(ctx: &MykoServerContext, s: &Session) {
    ctx.del_by_id(Session::ENTITY_NAME_STATIC, s.id.0.as_ref())
        .expect("session DEL");
    succession::record_removed(s.id.0.as_ref(), chrono::Utc::now().timestamp_millis());
}

fn unique_cwd() -> String {
    format!("/work/{}", Uuid::new_v4())
}

#[test]
fn a_restarted_session_takes_over_and_the_record_says_why() {
    let ctx = setup();
    let cwd = unique_cwd();
    let old = session(&cwd, None, None);
    let nick = start(&ctx, &old);
    end(&ctx, &old);
    let new = session(&cwd, None, None);
    assert_eq!(
        start(&ctx, &new),
        nick,
        "the restarted session keeps its nickname"
    );

    let successions = cmd(&ctx).exec_query(GetAllSuccessions {}).unwrap();
    let rec = successions
        .iter()
        .find(|s| s.id.0.as_ref() == old.id.0.as_ref())
        .expect("a succession record for the old id");
    assert_eq!(rec.to_session_id, new.id.0.as_ref());
    assert_eq!(rec.nickname, nick);
    assert!(rec.reason.contains("removed"), "reason: {}", rec.reason);
    assert!(
        rec.alias.is_none(),
        "taken over at registration, so no alias"
    );
}

#[test]
fn a_role_is_not_taken_over_while_its_holder_is_live() {
    let ctx = setup();
    let client = Uuid::new_v4().to_string();
    let holder = session(&unique_cwd(), Some("watchman"), Some(&client));
    let holder_nick = start(&ctx, &holder);
    // The holder's client is connected.
    let live = [Arc::<str>::from(client.as_str())].into_iter().collect();
    succession::observe(
        &[(holder.id.0.clone(), Some(Arc::from(client.as_str())))],
        &live,
        chrono::Utc::now().timestamp_millis(),
    );
    let claimant = session(&unique_cwd(), Some("watchman"), None);
    let claimant_nick = start(&ctx, &claimant);
    assert_ne!(
        claimant_nick, holder_nick,
        "a live holder keeps its role's nickname"
    );
}

#[test]
fn a_role_passes_to_a_successor_once_its_holder_has_ended() {
    let ctx = setup();
    // A role is matched across working directories.
    let holder = session(&unique_cwd(), Some("lamplighter"), None);
    let nick = start(&ctx, &holder);
    end(&ctx, &holder);
    let next = session(&unique_cwd(), Some("lamplighter"), None);
    assert_eq!(start(&ctx, &next), nick);
}

/// Send `to` a direct message as `from`, and return whom it was stored for.
fn send(ctx: &MykoServerContext, from: &Session, to: &str) -> String {
    let wanted = format!("to {to}, {}", Uuid::new_v4());
    SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from(to)),
        body: wanted.clone(),
        as_session: Some(from.id.clone()),
        idempotency_key: None,
    }
    .execute(cmd(ctx))
    .unwrap_or_else(|e| panic!("send to {to}: {}", e.message));
    let store = ctx
        .registry
        .get(Message::ENTITY_NAME_STATIC)
        .expect("Message store exists");
    store
        .entries()
        .materialize()
        .get()
        .into_iter()
        .filter_map(|(_, item)| downcast_item::<Message>(&item))
        .find(|m| m.body == wanted)
        .and_then(|m| m.to_session_id.clone())
        .expect("the message names its recipient")
        .0
        .to_string()
}

#[test]
fn a_declared_role_is_reached_after_a_restart() {
    // MAR-9's done-when: a role declared by environment (`MARSHAL_IDENTITY`)
    // can be reached after its holder restarts, by the role, by its nickname,
    // and by the session id peers knew before the restart.
    let ctx = setup();
    let sender = session(&unique_cwd(), Some("none"), None);
    start(&ctx, &sender);
    let before = session(&unique_cwd(), Some("watchman"), None);
    let nick = start(&ctx, &before);
    assert_eq!(send(&ctx, &sender, "watchman"), before.id.0.as_ref());

    end(&ctx, &before);
    let after = session(&unique_cwd(), Some("watchman"), None);
    assert_eq!(start(&ctx, &after), nick, "the role keeps its nickname");

    let after_id = after.id.0.as_ref();
    assert_eq!(send(&ctx, &sender, "watchman"), after_id, "by role");
    assert_eq!(send(&ctx, &sender, &nick), after_id, "by nickname");
    assert_eq!(
        send(&ctx, &sender, before.id.0.as_ref()),
        after_id,
        "by the id from before the restart"
    );
}

#[test]
fn identity_none_opts_out() {
    let ctx = setup();
    let cwd = unique_cwd();
    let old = session(&cwd, None, None);
    let nick = start(&ctx, &old);
    end(&ctx, &old);
    let new = session(&cwd, Some("none"), None);
    assert_ne!(
        start(&ctx, &new),
        nick,
        "MARSHAL_IDENTITY=none never takes over"
    );
    let identities = cmd(&ctx).exec_query(GetAllSessionIdentitys {}).unwrap();
    assert!(
        !identities
            .iter()
            .any(|i| i.id.0.as_ref() == new.id.0.as_ref()),
        "and it has no identity to be taken over by"
    );
}

#[test]
fn a_successor_gets_its_predecessors_adhoc_rooms() {
    let ctx = setup();
    let cwd = unique_cwd();
    let old = session(&cwd, None, None);
    start(&ctx, &old);
    let room = format!("design-{}", Uuid::new_v4());
    ctx.set(&Room {
        id: RoomId(Arc::from(room.as_str())),
        name: room.clone(),
        description: None,
        kind: RoomKind::Adhoc,
        created_at: 1,
    })
    .expect("room SET");
    ctx.set(&RoomMember {
        id: RoomMemberId(Arc::from(
            RoomMember::make_id(&room, old.id.0.as_ref()).as_str(),
        )),
        room_id: RoomId(Arc::from(room.as_str())),
        session_id: old.id.clone(),
        joined_at: 100,
    })
    .expect("membership SET");
    end(&ctx, &old);
    let new = session(&cwd, None, None);
    start(&ctx, &new);

    let members = cmd(&ctx).exec_query(GetAllRoomMembers {}).unwrap();
    let moved = members
        .iter()
        .find(|m| m.room_id.0.as_ref() == room && m.session_id == new.id)
        .expect("the successor is a member of its predecessor's ad hoc room");
    assert_eq!(
        moved.joined_at, 100,
        "it sees the room's history from when its predecessor joined"
    );
    // Pin the store too, in case the query view is behind it.
    let store = ctx.registry.get(RoomMember::ENTITY_NAME_STATIC).unwrap();
    assert!(store.entries().materialize().get().iter().any(|(_, item)| {
        downcast_item::<RoomMember>(item).is_some_and(|m| m.session_id == new.id)
    }));
}

/// A nickname given up in a late takeover still reaches its session as an
/// alias, so it isn't handed to a new session until the alias lapses:
/// resolution tries live nicknames first, and the newcomer would get mail
/// meant for the successor.
#[test]
fn a_live_alias_isnt_given_to_a_new_session() {
    use marshal_entities::{Succession, SuccessionId, nickname};
    let ctx = setup();
    let newcomer = session(&unique_cwd(), Some("none"), None);
    // The name the newcomer would get on its own is held as a live alias.
    let held = nickname(newcomer.id.0.as_ref());
    let now = chrono::Utc::now().timestamp_millis();
    ctx.set(&Succession {
        id: SuccessionId(Arc::from(Uuid::new_v4().to_string().as_str())),
        to_session_id: Uuid::new_v4().to_string(),
        nickname: "someone-else".into(),
        key: "cwd:claude|/elsewhere|op@example.com|a1".into(),
        reason: "test".into(),
        at: now,
        alias: Some(held.clone()),
        alias_until: Some(now + 600_000),
        told: false,
        handed_over: Vec::new(),
    })
    .expect("succession SET");
    assert_ne!(
        start(&ctx, &newcomer),
        held,
        "a name still reaching another session as an alias isn't reused"
    );

    // Once the alias has lapsed, the name is free again.
    let later = session(&unique_cwd(), Some("none"), None);
    let free = nickname(later.id.0.as_ref());
    ctx.set(&Succession {
        id: SuccessionId(Arc::from(Uuid::new_v4().to_string().as_str())),
        to_session_id: Uuid::new_v4().to_string(),
        nickname: "another".into(),
        key: "cwd:claude|/elsewhere2|op@example.com|a1".into(),
        reason: "test".into(),
        at: now,
        alias: Some(free.clone()),
        alias_until: Some(now - 1),
        told: false,
        handed_over: Vec::new(),
    })
    .expect("succession SET");
    assert_eq!(start(&ctx, &later), free);
}

/// MUS-47: the predecessor's unread direct mail is the successor's after the
/// takeover, and the record lists it. Mail it had read stays with it.
#[test]
fn unread_mail_is_handed_to_the_successor() {
    use marshal_entities::{Message, MessageId, MessageRead, MessageReadId};
    let ctx = setup();
    let cwd = unique_cwd();
    let old = session(&cwd, None, None);
    start(&ctx, &old);
    let mail = |body: &str| Message {
        id: MessageId(Arc::from(Uuid::new_v4().to_string().as_str())),
        from_session_id: SessionId(Arc::from("sender")),
        to_session_id: Some(old.id.clone()),
        to_room_id: None,
        to_operator: None,
        response_required: false,
        body: body.into(),
        sent_at: 1,
    };
    let unread = mail("unread");
    let read = mail("read");
    ctx.set(&unread).expect("message SET");
    ctx.set(&read).expect("message SET");
    ctx.set(&MessageRead {
        id: MessageReadId(Arc::from(
            MessageRead::make_id(read.id.0.as_ref(), old.id.0.as_ref()).as_str(),
        )),
        message_id: read.id.clone(),
        session_id: old.id.clone(),
        read_at: 2,
    })
    .expect("read SET");
    end(&ctx, &old);
    let new = session(&cwd, None, None);
    start(&ctx, &new);

    let messages = cmd(&ctx)
        .exec_query(marshal_entities::GetAllMessages {})
        .unwrap();
    let to = |m: &Message| {
        messages
            .iter()
            .find(|x| x.id == m.id)
            .and_then(|x| x.to_session_id.clone())
    };
    assert_eq!(to(&unread), Some(new.id.clone()), "unread mail moves");
    assert_eq!(to(&read), Some(old.id.clone()), "read mail stays");
    let rec = cmd(&ctx)
        .exec_query(GetAllSuccessions {})
        .unwrap()
        .into_iter()
        .find(|s| s.id.0.as_ref() == old.id.0.as_ref())
        .expect("succession record");
    assert_eq!(rec.handed_over, vec![unread.id.0.to_string()]);
}
