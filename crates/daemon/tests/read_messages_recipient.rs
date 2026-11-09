//! A sender can see whether mail it sent went unread when its recipient was
//! reaped. `ReadMessages` reports, on each direct message the caller
//! sent, whether the recipient is still on the roster and whether it read the
//! message. The message itself is kept for the recipient's return.

use std::sync::Arc;

use marshal_entities::{AckMessages, MessageView, ReadMessages, SendMessage, Session, SessionId};
use myko::{
    command::{CommandContext, CommandHandler},
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
    Box::leak(Box::new(server));
    ctx
}

fn session(id: &str, client: &str) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: Some(ClientId(Arc::from(client))),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        activity_at: None,
        kind: None,
        connected_at: 100,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
        agent_id: None,
        acks_pushes: None,
        harness: None,
        identity: None,
    }
}

fn apply(ctx: &MykoServerContext, s: &Session, change: MEventType) {
    let event = MEvent::from_item(s, change, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session event");
}

fn as_client(ctx: &MykoServerContext, client: &str, command: &str) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        Some(Arc::<str>::from(client)),
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from(command),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn send(ctx: &MykoServerContext, body: &str) -> String {
    SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: body.into(),
        as_session: None,
        idempotency_key: None,
    }
    .execute(as_client(ctx, "c-sender", "SendMessage"))
    .expect("send")
    .message_id
    .0
    .to_string()
}

fn read(ctx: &MykoServerContext, client: &str, sent: bool) -> Vec<MessageView> {
    ReadMessages {
        room: None,
        from: None,
        to_session: None,
        inbox: false,
        sent,
        unread: false,
        since: None,
        limit: None,
        as_session: None,
    }
    .execute(as_client(ctx, client, "ReadMessages"))
    .expect("read")
    .messages
}

#[test]
fn a_sender_sees_its_mail_went_unread_when_the_recipient_was_reaped() {
    let ctx = setup();
    let recipient = session("recipient", "c-recipient");
    apply(&ctx, &session("sender", "c-sender"), MEventType::SET);
    apply(&ctx, &recipient, MEventType::SET);
    let seen = send(&ctx, "you read this one");
    let unseen = send(&ctx, "this one went unread");
    AckMessages {
        message_ids: vec![marshal_entities::MessageId(Arc::from(seen.as_str()))],
        as_session: None,
    }
    .execute(as_client(&ctx, "c-recipient", "AckMessages"))
    .expect("ack");

    // While the recipient is live, the sender sees it on the roster.
    let live = read(&ctx, "c-sender", true);
    assert!(
        live.iter()
            .all(|m| m.recipient.as_ref().is_some_and(|r| r.on_roster))
    );

    apply(&ctx, &recipient, MEventType::DEL);

    let after = read(&ctx, "c-sender", true);
    let status = |id: &str| {
        after
            .iter()
            .find(|m| m.message_id.0.as_ref() == id)
            .and_then(|m| m.recipient.clone())
            .expect("a direct message the caller sent carries its recipient's status")
    };
    let gone_unread = status(&unseen);
    assert!(
        !gone_unread.on_roster && !gone_unread.read,
        "reaped with it unread"
    );
    let gone_read = status(&seen);
    assert!(
        !gone_read.on_roster && gone_read.read,
        "reaped after reading it"
    );
}

#[test]
fn only_the_sender_sees_a_recipient_status() {
    let ctx = setup();
    apply(&ctx, &session("sender", "c-sender"), MEventType::SET);
    apply(&ctx, &session("recipient", "c-recipient"), MEventType::SET);
    send(&ctx, "hello");
    let inbox = read(&ctx, "c-recipient", false);
    assert_eq!(inbox.len(), 1);
    assert!(
        inbox[0].recipient.is_none(),
        "the recipient's own view has no read receipt"
    );
}
