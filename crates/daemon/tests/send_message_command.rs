//! Integration tests for the `SendMessage` server command (pull model).
//!
//! Spins up a real `CellServer` (no WS listener), pre-populates the
//! Session store, then invokes `SendMessage::execute` directly through a
//! synthetic `CommandContext`. Under the pull model the handler should:
//!   - resolve the sender from the WS `client_id` OR an explicit
//!     `as_session` (self-identify, for connectionless HTTP-MCP callers),
//!   - reject an unknown recipient and an unresolvable sender with a clear
//!     `CommandError` (these are the only hard failures),
//!   - **persist the `Message` regardless of whether the recipient is
//!     online** — delivery is decoupled from acceptance; an offline
//!     recipient pulls it on its next hook turn,
//!   - report `delivered_live` = whether a best-effort live push to a
//!     connected recipient landed (always `false` in these fixtures, which
//!     have no live `client_registry`).

use std::sync::Arc;

use hyphae::{Gettable, Materialize};
use marshal_entities::{
    LivePushStatus, Message, SendMessage, SendMessageResult, Session, SessionId, WakeStatus,
    WakeWatcher, WakeWatcherId,
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

fn setup() -> MykoServerContext {
    marshal_entities::link();
    daemon::link();

    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    let server: &'static MykoServer = Box::leak(Box::new(server));
    let _ = server;
    ctx
}

fn session(id: &str, client_id: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: client_id.map(|c| ClientId(Arc::from(c))),
        agent_id: None,
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
    }
}

fn set_session(ctx: &MykoServerContext, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
}

/// Build a CommandContext as if the WS server had just dispatched a
/// command from the given client connection (`None` = no connection
/// identity, the HTTP-MCP / hook path).
fn cmd_ctx(ctx: &MykoServerContext, caller_client_id: Option<&str>) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        caller_client_id.map(Arc::<str>::from),
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

fn message_count(ctx: &MykoServerContext) -> usize {
    ctx.registry
        .get(Message::ENTITY_NAME_STATIC)
        .map(|store| store.entries().materialize().get().len())
        .unwrap_or(0)
}

fn only_message(ctx: &MykoServerContext) -> Message {
    let store = ctx
        .registry
        .get(Message::ENTITY_NAME_STATIC)
        .expect("Message store exists");
    let entries = store.entries().materialize().get();
    assert_eq!(entries.len(), 1, "expected exactly one Message");
    myko::utils::downcast_item::<Message>(&entries.into_iter().next().unwrap().1)
        .expect("entry is a Message")
}

#[test]
fn offline_recipient_succeeds_and_persists_for_pull() {
    // Pull model: an offline recipient (no live client) is NOT an error.
    // The message persists so the recipient pulls it on its next hook turn.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        response_required: true,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("offline recipient is success under the pull model");

    assert!(
        !result.delivered_live,
        "no live client → not delivered live"
    );
    assert_eq!(result.live_push, LivePushStatus::Unavailable);
    assert_eq!(result.wake, WakeStatus::Unobserved);
    assert_eq!(message_count(&ctx), 1, "message must persist for pull");
    assert_eq!(only_message(&ctx).body, "hi");
    assert!(only_message(&ctx).response_required);
    let read = marshal_entities::ReadMessages {
        as_session: Some(SessionId(Arc::from("recipient"))),
        ..serde_json::from_value(serde_json::json!({})).unwrap()
    }
    .execute(cmd_ctx(&ctx, None))
    .expect("read persisted requirement");
    assert!(read.messages[0].response_required);
    assert_eq!(read.messages[0].body, "hi");
}

/// A waiter holding `/hook/wait` for `id`, as the hook listener records it.
fn set_waiter(ctx: &MykoServerContext, id: &str) {
    let watcher = WakeWatcher {
        id: WakeWatcherId(Arc::from(id)),
        until: chrono::Utc::now().timestamp_millis() + 60_000,
        session_id: None,
    };
    let ev = MEvent::from_item(&watcher, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![ev])
        .expect("apply WakeWatcher SET");
}

fn send_to_recipient(ctx: &MykoServerContext) -> SendMessageResult {
    SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    }
    .execute(cmd_ctx(ctx, Some("c-sender")))
    .expect("send")
}

#[test]
fn a_connected_channels_off_recipient_without_a_waiter_has_no_wake_path() {
    // A Claude session's MCP shim without the channels flag is connected but
    // can't wake the session, and the roster says `wake: none`.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    let mut recipient = session("recipient", Some("c-recipient"));
    recipient.channels_enabled = Some(false);
    set_session(&ctx, &recipient);

    let result = send_to_recipient(&ctx);
    assert_eq!(result.live_push, LivePushStatus::Unavailable);
    assert_eq!(result.wake, WakeStatus::Unobserved);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn a_recipient_with_a_waiter_expects_a_wake() {
    // Connected without channels, or a pull row: either way a waiter holding
    // `/hook/wait` will hand it the message.
    for client in [Some("c-recipient"), None] {
        let ctx = setup();
        set_session(&ctx, &session("sender", Some("c-sender")));
        let mut recipient = session("recipient", client);
        recipient.channels_enabled = Some(false);
        set_session(&ctx, &recipient);
        set_waiter(&ctx, "recipient");

        let result = send_to_recipient(&ctx);
        assert_eq!(result.wake, WakeStatus::Expected, "client {client:?}");
    }
}

#[test]
fn a_pull_recipient_with_a_waiter_expects_a_wake() {
    // A session kept after its connection dropped has no client but
    // still holds `/hook/wait`, so the message will wake it.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));
    let watcher = WakeWatcher {
        id: WakeWatcherId(Arc::from("recipient")),
        until: chrono::Utc::now().timestamp_millis() + 60_000,
        session_id: None,
    };
    let ev = MEvent::from_item(&watcher, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![ev])
        .expect("apply WakeWatcher SET");

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd.execute(cmd_ctx(&ctx, Some("c-sender"))).expect("send");

    assert_eq!(result.live_push, LivePushStatus::Unavailable);
    assert_eq!(result.wake, WakeStatus::Expected);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn stale_binding_succeeds_and_persists() {
    // A recipient whose `client_id` points at a connection that is no
    // longer live (post-bounce). The best-effort push finds no live client
    // and reports `delivered_live = false`, but the message still persists.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", Some("c-stale")));

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("stale binding is not a hard failure under the pull model");

    assert!(!result.delivered_live);
    assert_eq!(result.live_push, LivePushStatus::Failed);
    assert_eq!(result.wake, WakeStatus::Unobserved);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn self_identified_sender_via_as_session_succeeds() {
    // The HTTP-MCP path: no connection `client_id`; the caller names itself
    // via `as_session`. Sender resolves from that, message persists, and
    // it's attributed to the self-identified session.
    let ctx = setup();
    set_session(&ctx, &session("sender", None));
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "from http".into(),
        as_session: Some(SessionId(Arc::from("sender"))),
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, None))
        .expect("self-identified send succeeds");

    assert!(!result.delivered_live);
    assert_eq!(result.live_push, LivePushStatus::Unavailable);
    assert_eq!(result.wake, WakeStatus::Unobserved);
    let msg = only_message(&ctx);
    assert_eq!(msg.from_session_id, SessionId(Arc::from("sender")));
    assert_eq!(msg.body, "from http");
}

#[test]
fn legacy_result_deserializes_with_explicit_unknown_status() {
    let result: SendMessageResult = serde_json::from_value(serde_json::json!({
        "messageId": "legacy-message",
        "toSessionId": "recipient",
        "sentAt": 123,
        "deliveredLive": false
    }))
    .expect("new client must read an older daemon result");

    assert_eq!(result.live_push, LivePushStatus::Unknown);
    assert_eq!(result.wake, WakeStatus::Unobserved);
    assert!(!result.delivered_live);
    assert_eq!(result.effective_live_push(), LivePushStatus::Unknown);
    assert_eq!(result.effective_wake(), WakeStatus::Unobserved);

    let live_result: SendMessageResult = serde_json::from_value(serde_json::json!({
        "messageId": "legacy-live-message",
        "toSessionId": "recipient",
        "sentAt": 124,
        "deliveredLive": true
    }))
    .expect("new client must normalize an older live result");
    assert_eq!(live_result.effective_live_push(), LivePushStatus::Delivered);
    assert_eq!(live_result.effective_wake(), WakeStatus::NotNeeded);
}

#[test]
fn unknown_recipient_errors_and_does_not_persist() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("does-not-exist")),
        body: "?".into(),
        as_session: None,
        idempotency_key: None,
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect_err("missing recipient should error");

    assert!(
        err.message.contains("does-not-exist"),
        "error should name the missing session id, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0, "no Message should be persisted");
}

#[test]
fn nickname_resolves_server_side() {
    // Resolution moved into the command (was shim-only) so EVERY harness —
    // opencode plugin, raw HTTP-MCP, shim — can address by nickname. No
    // SessionNickname is assigned here, so `nickname_for` falls back to the
    // deterministic handle; address by that and it must route to the session.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient-xyz", None));

    let nick = marshal_entities::nickname("recipient-xyz");
    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from(nick.as_str())),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("a unique nickname resolves");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("recipient-xyz")),
        "nickname `{nick}` should resolve to its session id",
    );
    assert_eq!(message_count(&ctx), 1);
    // Agent-addressed (id/nickname/prefix) mail must NOT be marked
    // human-addressed — otherwise the recipient would be told to relay ordinary
    // peer chatter to a human.
    assert_eq!(
        only_message(&ctx).to_operator,
        None,
        "nickname-addressed mail is agent-to-agent, not human-addressed",
    );
}

#[test]
fn direct_message_ids_are_uuid_v7() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));

    let result = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
        idempotency_key: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("send succeeds");

    let id = Uuid::parse_str(result.message_id.0.as_ref()).expect("message id is a UUID");
    assert_eq!(id.get_version_num(), 7);
}

#[test]
fn a_retry_with_the_same_key_stores_one_message() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));
    let send = |body: &str, key: &str| {
        SendMessage {
            response_required: false,
            to_session_id: SessionId(Arc::from("recipient")),
            body: body.into(),
            as_session: None,
            idempotency_key: Some(key.into()),
        }
        .execute(cmd_ctx(&ctx, Some("c-sender")))
    };

    let first = send("hi", "k1").expect("first send succeeds");
    let retry = send("hi", "k1").expect("a retry succeeds");
    assert_eq!(retry.message_id, first.message_id);
    assert_eq!(retry.sent_at, first.sent_at);
    assert_eq!(retry.live_push, LivePushStatus::Unknown);
    assert_eq!(only_message(&ctx).body, "hi");

    assert!(
        send("different", "k1").is_err(),
        "a reused key can't carry a new message"
    );
    send("hi", "k2").expect("a new key is a new message");
    assert_eq!(message_count(&ctx), 2);
}

fn keyed(to: &str, body: &str, response_required: bool, key: &str) -> SendMessage {
    SendMessage {
        response_required,
        to_session_id: SessionId(Arc::from(to)),
        body: body.into(),
        as_session: None,
        idempotency_key: Some(key.into()),
    }
}

#[test]
fn racing_attempts_with_the_same_key_store_one_message() {
    // A retry sent while the timed-out first attempt is still running: both
    // look for the first send at once, and only one may store and push.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));
    let start = std::sync::Barrier::new(8);
    let results: Vec<SendMessageResult> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    start.wait();
                    keyed("recipient", "hi", false, "race")
                        .execute(cmd_ctx(&ctx, Some("c-sender")))
                        .expect("every attempt succeeds")
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        results
            .windows(2)
            .all(|w| w[0].message_id == w[1].message_id),
        "one message id"
    );
    // A fresh send reports its push (`unavailable` here, with no live client);
    // one that found the first send reports `unknown`. Two fresh sends would
    // both store over the same id and both push, so the count can't show it.
    let fresh = results
        .iter()
        .filter(|r| r.live_push != LivePushStatus::Unknown)
        .count();
    assert_eq!(fresh, 1, "exactly one attempt stored and pushed");
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn a_retry_finds_its_send_after_the_recipient_left_the_roster() {
    // The retry isn't resolved again: the token may now name another session,
    // or none, and it's still the same send.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    let gone = session("recipient", None);
    set_session(&ctx, &gone);
    let first = keyed("recipient", "hi", false, "k1")
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("first send succeeds");
    let del = MEvent::from_item(&gone, MEventType::DEL, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![del]).expect("apply Session DEL");

    let retry = keyed("recipient", "hi", false, "k1")
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("the retry finds the stored send");
    assert_eq!(retry.message_id, first.message_id);
    assert_eq!(retry.to_session_id, first.to_session_id);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn a_key_reused_with_another_response_required_is_refused() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));
    keyed("recipient", "hi", false, "k1")
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("first send succeeds");
    let err = keyed("recipient", "hi", true, "k1")
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect_err("a changed response_required isn't the same message");
    assert!(err.message.contains("different message"), "{}", err.message);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn an_empty_or_overlong_key_is_refused() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));
    for key in [String::new(), "  ".into(), "k".repeat(129)] {
        let err = keyed("recipient", "hi", false, &key)
            .execute(cmd_ctx(&ctx, Some("c-sender")))
            .expect_err("the key is refused");
        assert!(err.message.contains("idempotencyKey"), "{}", err.message);
    }
    keyed("recipient", "hi", false, &"k".repeat(128))
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("a 128-byte key is accepted");
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn operator_token_routes_to_the_humans_most_recently_active_agent() {
    // Human-via-agent routing: address the PERSON by their operator identity
    // and the daemon picks which of their agents currently has the floor —
    // the most-recently-active one.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let mut idle = session("max-idle", Some("c-idle"));
    idle.operator = Some("operator@example.com".into());
    idle.last_activity_at = Some(1_000);
    let mut active = session("max-active", Some("c-active"));
    active.operator = Some("operator@example.com".into());
    active.last_activity_at = Some(9_000);
    set_session(&ctx, &idle);
    set_session(&ctx, &active);

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("operator@example.com")),
        body: "your call on the redeploy".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("operator identity resolves to the human's agent");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("max-active")),
        "should route to the most-recently-active of the operator's sessions",
    );
    let msg = only_message(&ctx);
    assert_eq!(msg.to_session_id, Some(SessionId(Arc::from("max-active"))));
    // ...and it's marked human-addressed so the receiving agent surfaces it to
    // its operator rather than treating it as ordinary peer chatter.
    assert_eq!(
        msg.to_operator.as_deref(),
        Some("operator@example.com"),
        "operator-tier routing must stamp `to_operator` (human-addressed)",
    );
}

#[test]
fn operator_routing_prefers_a_live_agent_and_accepts_the_op_prefix() {
    // A disconnected session active more recently vs a live one active less
    // recently — the LIVE agent wins (that's where a push can land now). Also
    // exercises the explicit `op:` disambiguation prefix.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let mut disconnected = session("max-disc", None); // client_id None ⇒ not live
    disconnected.operator = Some("operator@example.com".into());
    disconnected.last_activity_at = Some(9_000);
    let mut live = session("max-live", Some("c-live"));
    live.operator = Some("operator@example.com".into());
    live.last_activity_at = Some(5_000);
    set_session(&ctx, &disconnected);
    set_session(&ctx, &live);

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("op:operator@example.com")),
        body: "ping".into(),
        as_session: None,
        idempotency_key: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("op:-prefixed operator identity resolves");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("max-live")),
        "a live agent outranks a more-recently-active disconnected one",
    );
}

#[test]
fn caller_without_session_errors() {
    // Caller's client_id maps to no session and no `as_session` was given.
    let ctx = setup();
    set_session(&ctx, &session("recipient", Some("c-recipient")));

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
        as_session: None,
        idempotency_key: None,
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-orphan")))
        .expect_err("orphan caller should error");

    assert!(
        err.message.contains("c-orphan"),
        "error should name the unbound client id, got: {}",
        err.message,
    );
    // MAR-18: the shim retries this refusal by its code, so the daemon's
    // refusal must start with it.
    assert!(
        err.message.starts_with(marshal_entities::UNREGISTERED_CODE),
        "{}",
        err.message
    );
    assert!(marshal_entities::is_unregistered_refusal(&err.message));
    assert_eq!(message_count(&ctx), 0);
}

#[test]
fn unidentified_caller_errors() {
    // No client_id AND no as_session → cannot resolve a sender at all.
    let ctx = setup();
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
        as_session: None,
        idempotency_key: None,
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, None))
        .expect_err("no identity should error");

    assert!(
        err.message.to_lowercase().contains("assession")
            || err.message.to_lowercase().contains("connected client"),
        "error should explain the missing identity, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0);
}
