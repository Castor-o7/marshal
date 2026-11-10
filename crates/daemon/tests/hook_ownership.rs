//! MAR-81: with a binding installed, a hook for a session that another
//! principal registered is refused (403) on every route that acts for a
//! session, before anything is written; the owner's own hooks work; and with
//! no caller (plain mode) nothing changes.

use std::sync::Arc;

use daemon::auth::session_binding::{self, SessionBinding};
use daemon::hooks;
use myko::server::{MykoServerContext, Persister};
use myko_auth::{
    AuthenticatedIdentity, AuthorizationRequest, Authorizer, Principal, PrincipalKind,
};
use myko_server::{BlackholePersister, MykoServer};

struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(
        &self,
        _: &AuthenticatedIdentity,
        _: AuthorizationRequest<'_>,
    ) -> Result<(), myko_auth::AuthError> {
        Ok(())
    }
}

fn ctx() -> Arc<MykoServerContext> {
    marshal_entities::link();
    daemon::link();
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    Arc::new(server.ctx())
}

fn principal(name: &str) -> Principal {
    Principal {
        issuer: "marshal-pairing".into(),
        subject: name.into(),
        kind: PrincipalKind::Agent,
    }
}

fn hook(
    ctx: &Arc<MykoServerContext>,
    caller: Option<&Principal>,
    path: &str,
    body: &str,
) -> hooks::HookOutcome {
    hooks::with_caller(caller.cloned(), || {
        hooks::dispatch(path, "host=test&harness=claude", body.as_bytes(), ctx)
    })
    .expect("a known route")
}

#[test]
fn another_principals_hooks_for_a_session_are_refused_on_every_route() {
    let ctx = ctx();
    let laptop = principal("laptop");
    let desk = principal("desk");
    // One binding per process: both tests in this file share it, with
    // different session ids.
    SessionBinding::new(AllowAll, Some(&principal("(local)"))).install();

    let sid = "owned-by-laptop";
    let body = format!(r#"{{"session_id":"{sid}","cwd":"/tmp/laptop"}}"#);
    let joined = hook(&ctx, Some(&laptop), "/hook/session-start", &body);
    assert_eq!(joined.status, 200, "the owner joins: {}", joined.body);

    // Every route that acts for the session, from the other principal.
    let routes = [
        ("/hook/session-start", body.clone()),
        ("/hook/session-register", body.clone()),
        ("/hook/prompt-submit", body.clone()),
        ("/hook/offers", format!(r#"{{"session_id":"{sid}"}}"#)),
        (
            "/hook/ack",
            format!(r#"{{"session_id":"{sid}","message_ids":["m1"]}}"#),
        ),
        (
            "/hook/bridge-watch",
            format!(r#"{{"session_id":"{sid}","watching":true}}"#),
        ),
        ("/hook/session-end", format!(r#"{{"session_id":"{sid}"}}"#)),
    ];
    for (path, body) in &routes {
        let out = hook(&ctx, Some(&desk), path, body);
        assert_eq!(
            out.status, 403,
            "{path} from another principal: {}",
            out.body
        );
        assert!(
            out.body.contains("belongs to another machine"),
            "{path}: {}",
            out.body
        );
    }
    // Refused before any write: the row is the owner's, and still there.
    let registry = ctx.registry.get_or_create("Session");
    assert!(
        registry.snapshot().iter().any(|(id, _)| id.as_ref() == sid),
        "the owner's row went after the refused hooks"
    );
    assert!(session_binding::held_by_another(sid, &desk));
    assert!(!session_binding::held_by_another(sid, &laptop));

    // The owner's own hooks work on every route.
    for (path, body) in &routes {
        let out = hook(&ctx, Some(&laptop), path, body);
        assert_eq!(out.status, 200, "{path} from the owner: {}", out.body);
    }
}

#[test]
fn with_no_caller_the_hooks_are_as_they_were() {
    let ctx = ctx();
    SessionBinding::new(AllowAll, Some(&principal("(local)"))).install();
    let sid = "plain-mode-session";
    let body = format!(r#"{{"session_id":"{sid}","cwd":"/tmp/plain"}}"#);
    for path in [
        "/hook/session-start",
        "/hook/prompt-submit",
        "/hook/offers",
        "/hook/session-end",
    ] {
        let out = hook(&ctx, None, path, &body);
        assert_eq!(out.status, 200, "{path} with no caller: {}", out.body);
    }
}
