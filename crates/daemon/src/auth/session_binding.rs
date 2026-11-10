//! MAR-80: with auth on, a connection acts only as its own sessions.
//!
//! myko#129's provider authenticates a connection to a principal (a paired
//! machine's token, once MAR-24 is in) and asks an [`Authorizer`] about each
//! command and client event. [`SessionBinding`] wraps whatever authorizer the
//! provider installs and adds one rule: a principal may act only as the
//! sessions it registered. It's a library type. Nothing here touches the
//! daemon's startup, so with no provider (today's plain mode) nothing changes.
//!
//! What it checks, once the inner authorizer has allowed the request:
//!
//! - `RegisterSession`, and a raw `Session` SET (the shim's fallback for an
//!   old daemon): the session's id is claimed for the caller. An id another
//!   principal holds is refused, so a session can't be taken over across
//!   principals. The same principal re-registering its id (a restart, MAR-9)
//!   is fine.
//! - A command naming a session must name one the caller holds: `asSession`
//!   on SendMessage, BroadcastMessage, ReadMessages, AckMessages, JoinRoom and
//!   LeaveRoom, and `id` on every `SetSession*` setter.
//! - A client event writing another session's row: a `Session` DEL, a
//!   `Message` (as its sender), a `MessageRead`, a `RoomMember` or a
//!   `WakeWatcher`.
//! - `SessionIdentity`, `SessionNickname` and `Succession` are the daemon's
//!   own: no client may write them. (Declaring them `server_owned` upstream
//!   would refuse that in plain mode too. That's a MAR-15 question.)
//!
//! A refusal is [`AuthError::Forbidden`], which myko answers with a
//! `command-error` "permission denied" and leaves the connection open.
//!
//! Revocation passes through: `revision` and `session_revision` are the
//! inner authorizer's, so a bump there (MAR-24's unpair, a policy edit)
//! still reaches open connections and live subscriptions through the
//! wrapper. Reads (queries, views, reports) and subscriptions aren't checked
//! here; what a principal may read is the inner authorizer's business.
//!
//! Sessions registered through the hook listener (Codex, and Claude's
//! `/hook/session-start`) never pass through myko, so [`bind_local`] binds
//! them to a configured *local principal*: the daemon host's own token. Any
//! process on the daemon's machine holding that token can then act for every
//! hook-registered session. That's the trust plain mode gives all of
//! loopback today, made explicit, so on a shared machine the token file must
//! be owner-only (0600). With no local principal configured, hook-registered
//! sessions are unowned, and under auth nothing can act for them: it fails
//! closed. The hook listener itself (port 6156) is outside myko and isn't
//! covered here. That's MAR-81.
//!
//! A binding goes when its session does: the hook listener's session-end,
//! the cleanup sweep, `marshal-daemon forget` and a client's own `Session`
//! DEL all call [`release`], so the table can't grow without bound.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use marshal_entities::{
    Message, MessageRead, RoomMember, Session, SessionIdentity, SessionNickname, Succession,
    WakeWatcher,
};
use myko::core::item::Eventable;
use myko_auth::{
    AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer, EventChange, OperationKind,
    Principal,
};
use serde_json::Value;

/// A principal as one string: issuer and subject, so two issuers' `alice`
/// stay apart.
#[must_use]
pub fn principal_key(principal: &Principal) -> String {
    format!("{}\u{0}{}", principal.issuer, principal.subject)
}

/// Which principal holds each session id.
#[derive(Debug, Default)]
pub struct Table {
    owners: Mutex<HashMap<Arc<str>, String>>,
}

impl Table {
    fn lock(&self) -> MutexGuard<'_, HashMap<Arc<str>, String>> {
        self.owners.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `who` takes `session`, unless another principal already holds it. An
    /// anonymous caller (`None`) can't hold anything.
    fn claim(&self, who: Option<&str>, session: &str) -> Result<(), AuthError> {
        let who = who.ok_or(AuthError::Forbidden)?;
        let mut owners = self.lock();
        match owners.get(session) {
            Some(owner) if owner != who => Err(AuthError::Forbidden),
            _ => {
                owners.insert(Arc::from(session), who.to_owned());
                Ok(())
            }
        }
    }

    fn must_own(&self, who: Option<&str>, session: &str) -> Result<(), AuthError> {
        let who = who.ok_or(AuthError::Forbidden)?;
        if self.lock().get(session).is_some_and(|owner| owner == who) {
            Ok(())
        } else {
            Err(AuthError::Forbidden)
        }
    }

    /// Forget who held `session`. True when someone did.
    pub fn release(&self, session: &str) -> bool {
        self.lock().remove(session).is_some()
    }

    /// The key (see [`principal_key`]) of whoever holds `session`.
    #[must_use]
    pub fn owner_of(&self, session: &str) -> Option<String> {
        self.lock().get(session).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// An [`Authorizer`] that lets a principal act only as its own sessions, on
/// top of whatever `inner` allows.
pub struct SessionBinding<A> {
    inner: A,
    local: Option<String>,
    table: Arc<Table>,
}

impl<A: Authorizer> SessionBinding<A> {
    /// Wrap `inner`. `local` is the daemon host's own principal, which
    /// hook-registered sessions are bound to; `None` leaves them unowned.
    pub fn new(inner: A, local: Option<&Principal>) -> Self {
        Self {
            inner,
            local: local.map(principal_key),
            table: Arc::default(),
        }
    }

    #[must_use]
    pub fn table(&self) -> &Arc<Table> {
        &self.table
    }

    /// Make this the binding that the hook listener and the daemon's cleanup
    /// use, through the free functions [`bind_local`] and [`release`]. One
    /// per process: a later install replaces an earlier one.
    pub fn install(&self) {
        *installed() = Some(Installed {
            table: self.table.clone(),
            local: self.local.clone(),
        });
    }

    /// Bind a hook-registered session to the local principal. False when no
    /// local principal is configured, or another principal holds the id.
    pub fn bind_local(&self, session: &str) -> bool {
        bind_to_local(&self.table, self.local.as_deref(), session)
    }

    fn check(&self, who: Option<&str>, request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        match request {
            AuthorizationRequest::Execute {
                kind: OperationKind::Command,
                operation,
                arguments,
            } => self.check_command(who, operation, arguments),
            AuthorizationRequest::ClientEvent {
                entity_type,
                change,
                proposed,
            } => self.check_event(who, entity_type, change, proposed),
            _ => Ok(()),
        }
    }

    fn check_command(
        &self,
        who: Option<&str>,
        operation: &str,
        arguments: &Value,
    ) -> Result<(), AuthError> {
        let field = |name: &str| arguments.get(name).and_then(Value::as_str);
        if operation == "RegisterSession" {
            if let Some(id) = arguments.pointer("/session/id").and_then(Value::as_str) {
                self.table.claim(who, id)?;
            }
            return Ok(());
        }
        if operation.starts_with("SetSession")
            && let Some(id) = field("id")
        {
            self.table.must_own(who, id)?;
        }
        if let Some(as_session) = field("asSession") {
            self.table.must_own(who, as_session)?;
        }
        Ok(())
    }

    fn check_event(
        &self,
        who: Option<&str>,
        entity_type: &str,
        change: EventChange,
        proposed: &Value,
    ) -> Result<(), AuthError> {
        let field = |name: &str| proposed.get(name).and_then(Value::as_str);
        if entity_type == Session::ENTITY_NAME_STATIC {
            // A Session with no id is myko's to refuse.
            let Some(id) = field("id") else { return Ok(()) };
            return match change {
                EventChange::Set => {
                    // A paired machine's row carries its pairing's name as its
                    // host, or no host (MAR-85); the command and hook paths set
                    // it, and a raw SET may not say otherwise.
                    if let Some(machine) = who.and_then(paired_machine)
                        && let Some(declared) = proposed
                            .get("host")
                            .and_then(|h| h.get("name"))
                            .and_then(Value::as_str)
                        && declared != machine
                    {
                        log::warn!(
                            "[auth] a paired machine's Session SET declared another host name; \
                             refused"
                        );
                        return Err(AuthError::Forbidden);
                    }
                    self.table.claim(who, id)
                }
                EventChange::Delete => {
                    self.table.must_own(who, id)?;
                    self.table.release(id);
                    Ok(())
                }
            };
        }
        if daemon_owned(entity_type) {
            return Err(AuthError::Forbidden);
        }
        let session = if entity_type == Message::ENTITY_NAME_STATIC {
            field("fromSessionId")
        } else if entity_type == MessageRead::ENTITY_NAME_STATIC
            || entity_type == RoomMember::ENTITY_NAME_STATIC
        {
            field("sessionId")
        } else if entity_type == WakeWatcher::ENTITY_NAME_STATIC {
            // A bridge's record names the session it wakes; a held wait's is
            // the session's own id.
            field("sessionId").or_else(|| field("id"))
        } else {
            None
        };
        match session {
            Some(session) => self.table.must_own(who, session),
            None => Ok(()),
        }
    }
}

impl<A: Authorizer> Authorizer for SessionBinding<A> {
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        self.inner.authorize(identity, request)?;
        let who = principal_key(&identity.principal);
        self.check(Some(&who), request)
    }

    fn authorize_anonymous(&self, request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        self.inner.authorize_anonymous(request)?;
        self.check(None, request)
    }

    // The provider reads these on the outermost authorizer to revoke open
    // connections and recheck subscriptions, so they must be the inner's.
    fn revision(&self) -> u64 {
        self.inner.revision()
    }

    fn session_revision(&self, identity: Option<&AuthenticatedIdentity>) -> u64 {
        self.inner.session_revision(identity)
    }
}

/// The daemon's entities that no client may write: the nickname sagas and
/// succession keep them.
fn daemon_owned(entity_type: &str) -> bool {
    entity_type == SessionIdentity::ENTITY_NAME_STATIC
        || entity_type == SessionNickname::ENTITY_NAME_STATIC
        || entity_type == Succession::ENTITY_NAME_STATIC
}

/// The machine behind a principal key, when it's a paired machine's: the
/// subject of a `marshal-pairing` principal other than the local one.
fn paired_machine(key: &str) -> Option<&str> {
    let (issuer, subject) = key.split_once('\u{0}')?;
    (issuer == crate::pairing::ISSUER && subject != crate::pairing::LOCAL_NAME).then_some(subject)
}

fn bind_to_local(table: &Table, local: Option<&str>, session: &str) -> bool {
    let Some(local) = local else {
        log::warn!(
            "[auth] session {session} registered through the hook listener is unowned: no local \
             principal is configured, so under auth nothing can act for it"
        );
        return false;
    };
    bind_to(table, local, session)
}

/// Claim `session` for the principal `key`. False, and a line that names
/// neither the holder nor its other sessions, when another principal holds it.
fn bind_to(table: &Table, key: &str, session: &str) -> bool {
    match table.claim(Some(key), session) {
        Ok(()) => true,
        Err(_) => {
            log::warn!(
                "[auth] session {session} registered through the hook listener is held by another \
                 principal, so it isn't bound to this one"
            );
            false
        }
    }
}

struct Installed {
    table: Arc<Table>,
    local: Option<String>,
}

static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);

fn installed() -> MutexGuard<'static, Option<Installed>> {
    INSTALLED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bind a session the hook listener registered to the installed binding's
/// local principal. With no binding installed (plain mode) it's a no-op, and
/// false.
pub fn bind_local(session: &str) -> bool {
    let guard = installed();
    let Some(Installed { table, local }) = guard.as_ref() else {
        return false;
    };
    bind_to_local(table, local.as_deref(), session)
}

/// Bind a session the hook listener registered to `principal`, the verified
/// owner of the token the request carried (MAR-81). With no binding installed
/// (plain mode) it's a no-op, and false.
pub fn bind(session: &str, principal: &Principal) -> bool {
    let guard = installed();
    let Some(Installed { table, .. }) = guard.as_ref() else {
        return false;
    };
    bind_to(table, &principal_key(principal), session)
}

/// Whether `session` is held by a principal other than `principal` (MAR-81):
/// the check a hook makes before acting for a session it didn't register.
/// Unbound, or no binding installed, is false.
pub fn held_by_another(session: &str, principal: &Principal) -> bool {
    installed().as_ref().is_some_and(|Installed { table, .. }| {
        table
            .owner_of(session)
            .is_some_and(|owner| owner != principal_key(principal))
    })
}

/// Forget who held a session that has gone: ended, swept or forgotten. With
/// no binding installed it's a no-op, and false.
pub fn release(session: &str) -> bool {
    installed()
        .as_ref()
        .is_some_and(|Installed { table, .. }| table.release(session))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use myko_auth::PrincipalKind;
    use serde_json::json;

    use super::*;

    /// An inner authorizer that allows everything, anonymous callers
    /// included, so each refusal below is the binding's own.
    struct AllowAll;

    impl Authorizer for AllowAll {
        fn authorize(
            &self,
            _identity: &AuthenticatedIdentity,
            _request: AuthorizationRequest<'_>,
        ) -> Result<(), AuthError> {
            Ok(())
        }

        fn authorize_anonymous(&self, _request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
            Ok(())
        }
    }

    fn principal(subject: &str) -> Principal {
        Principal {
            issuer: "test".into(),
            subject: subject.into(),
            kind: PrincipalKind::Agent,
        }
    }

    fn identity(subject: &str) -> AuthenticatedIdentity {
        AuthenticatedIdentity {
            user_id: None,
            principal: principal(subject),
            client_id: None,
            scopes: vec![],
            delegation: vec![],
            claims: Value::Null,
            expires_at: SystemTime::now() + Duration::from_secs(60),
        }
    }

    fn command<'a>(operation: &'a str, arguments: &'a Value) -> AuthorizationRequest<'a> {
        AuthorizationRequest::Execute {
            kind: OperationKind::Command,
            operation,
            arguments,
        }
    }

    fn event<'a>(
        entity_type: &'a str,
        change: EventChange,
        proposed: &'a Value,
    ) -> AuthorizationRequest<'a> {
        AuthorizationRequest::ClientEvent {
            entity_type,
            change,
            proposed,
        }
    }

    fn register(id: &str) -> Value {
        json!({ "session": { "id": id, "pid": 1, "cwd": "/w" } })
    }

    #[test]
    fn a_principal_holds_what_it_registers_and_nothing_else() {
        let binding = SessionBinding::new(AllowAll, None);
        let alice = identity("alice");
        let bob = identity("bob");
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        assert_eq!(
            binding.table().owner_of("s1").as_deref(),
            Some(principal_key(&alice.principal).as_str())
        );
        let own = json!({ "id": "s1", "activity": "busy" });
        assert!(
            binding
                .authorize(&alice, command("SetSessionActivity", &own))
                .is_ok()
        );
        assert_eq!(
            binding.authorize(&bob, command("SetSessionActivity", &own)),
            Err(AuthError::Forbidden)
        );
        let as_s1 = json!({ "toSessionId": "s9", "body": "hi", "asSession": "s1" });
        assert!(
            binding
                .authorize(&alice, command("SendMessage", &as_s1))
                .is_ok()
        );
        assert_eq!(
            binding.authorize(&bob, command("SendMessage", &as_s1)),
            Err(AuthError::Forbidden)
        );
        // A command naming no session passes to the handler, which resolves
        // the caller from its connection.
        let plain = json!({ "toSessionId": "s9", "body": "hi" });
        assert!(
            binding
                .authorize(&bob, command("SendMessage", &plain))
                .is_ok()
        );
    }

    #[test]
    fn a_session_is_not_taken_over_across_principals_but_its_own_restart_is_fine() {
        let binding = SessionBinding::new(AllowAll, None);
        let alice = identity("alice");
        let bob = identity("bob");
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        assert_eq!(
            binding.authorize(&bob, command("RegisterSession", &register("s1"))),
            Err(AuthError::Forbidden)
        );
        let row = json!({ "id": "s1", "pid": 2 });
        assert_eq!(
            binding.authorize(
                &bob,
                event(Session::ENTITY_NAME_STATIC, EventChange::Set, &row)
            ),
            Err(AuthError::Forbidden)
        );
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        assert!(
            binding
                .authorize(
                    &alice,
                    event(Session::ENTITY_NAME_STATIC, EventChange::Set, &row)
                )
                .is_ok()
        );
    }

    #[test]
    fn client_events_name_a_session_the_caller_holds() {
        let binding = SessionBinding::new(AllowAll, None);
        let alice = identity("alice");
        let bob = identity("bob");
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        let cases = [
            (
                Message::ENTITY_NAME_STATIC,
                json!({ "id": "m1", "fromSessionId": "s1" }),
            ),
            (
                MessageRead::ENTITY_NAME_STATIC,
                json!({ "id": "r1", "messageId": "m1", "sessionId": "s1" }),
            ),
            (
                RoomMember::ENTITY_NAME_STATIC,
                json!({ "id": "rm1", "sessionId": "s1" }),
            ),
            (
                WakeWatcher::ENTITY_NAME_STATIC,
                json!({ "id": "s1", "until": 1 }),
            ),
            (
                WakeWatcher::ENTITY_NAME_STATIC,
                json!({ "id": "bridge:s1", "sessionId": "s1", "until": 1 }),
            ),
        ];
        for (entity, proposed) in &cases {
            assert!(
                binding
                    .authorize(&alice, event(entity, EventChange::Set, proposed))
                    .is_ok(),
                "{entity}: its holder writes it"
            );
            assert_eq!(
                binding.authorize(&bob, event(entity, EventChange::Set, proposed)),
                Err(AuthError::Forbidden),
                "{entity}: another principal doesn't"
            );
        }
        // An entity that names no session is the inner authorizer's business.
        let room = json!({ "id": "room1", "name": "everyone" });
        assert!(
            binding
                .authorize(&bob, event("Room", EventChange::Set, &room))
                .is_ok()
        );
    }

    #[test]
    fn the_daemons_own_entities_are_refused_to_every_client() {
        let binding = SessionBinding::new(AllowAll, None);
        let alice = identity("alice");
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        for entity in [
            SessionIdentity::ENTITY_NAME_STATIC,
            SessionNickname::ENTITY_NAME_STATIC,
            Succession::ENTITY_NAME_STATIC,
        ] {
            let row = json!({ "id": "s1" });
            assert_eq!(
                binding.authorize(&alice, event(entity, EventChange::Set, &row)),
                Err(AuthError::Forbidden),
                "{entity}"
            );
            assert_eq!(
                binding.authorize(&alice, event(entity, EventChange::Delete, &row)),
                Err(AuthError::Forbidden),
                "{entity}"
            );
        }
    }

    #[test]
    fn an_anonymous_caller_holds_nothing() {
        let binding = SessionBinding::new(AllowAll, None);
        assert_eq!(
            binding.authorize_anonymous(command("RegisterSession", &register("s1"))),
            Err(AuthError::Forbidden)
        );
        let as_s1 = json!({ "asSession": "s1" });
        assert_eq!(
            binding.authorize_anonymous(command("ReadMessages", &as_s1)),
            Err(AuthError::Forbidden)
        );
        assert!(binding.table().is_empty());
    }

    #[test]
    fn a_binding_goes_with_its_session() {
        let binding = SessionBinding::new(AllowAll, None);
        let alice = identity("alice");
        let bob = identity("bob");
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        let row = json!({ "id": "s1" });
        // Another principal can't end it.
        assert_eq!(
            binding.authorize(
                &bob,
                event(Session::ENTITY_NAME_STATIC, EventChange::Delete, &row)
            ),
            Err(AuthError::Forbidden)
        );
        assert!(
            binding
                .authorize(
                    &alice,
                    event(Session::ENTITY_NAME_STATIC, EventChange::Delete, &row)
                )
                .is_ok()
        );
        assert!(
            binding.table().is_empty(),
            "the holder's own DEL releases it"
        );
        // The daemon's own paths (session-end, the sweep, forget) release too.
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s2")))
                .is_ok()
        );
        assert!(binding.table().release("s2"));
        assert!(!binding.table().release("s2"));
        assert!(
            binding
                .authorize(&bob, command("RegisterSession", &register("s2")))
                .is_ok()
        );
    }

    #[test]
    fn hook_registered_sessions_belong_to_the_local_principal_or_to_nobody() {
        let local = principal("murph-local");
        let binding = SessionBinding::new(AllowAll, Some(&local));
        let local_conn = identity("murph-local");
        let alice = identity("alice");
        assert!(binding.bind_local("codex-1"));
        let own = json!({ "id": "codex-1", "activity": "busy" });
        assert!(
            binding
                .authorize(&local_conn, command("SetSessionActivity", &own))
                .is_ok()
        );
        assert_eq!(
            binding.authorize(&alice, command("SetSessionActivity", &own)),
            Err(AuthError::Forbidden),
            "a remote principal can't act for a hook-registered session"
        );
        // The local principal is just a principal: it can't act for a session
        // a remote one registered over myko.
        assert!(
            binding
                .authorize(&alice, command("RegisterSession", &register("s1")))
                .is_ok()
        );
        let theirs = json!({ "id": "s1", "activity": "busy" });
        assert_eq!(
            binding.authorize(&local_conn, command("SetSessionActivity", &theirs)),
            Err(AuthError::Forbidden)
        );
        assert!(
            !binding.bind_local("s1"),
            "nor does binding steal a held id"
        );

        let unconfigured = SessionBinding::new(AllowAll, None);
        assert!(!unconfigured.bind_local("codex-2"));
        let orphan = json!({ "id": "codex-2", "activity": "busy" });
        assert_eq!(
            unconfigured.authorize(&local_conn, command("SetSessionActivity", &orphan)),
            Err(AuthError::Forbidden),
            "with no local principal, a hook-registered session is nobody's"
        );
    }

    #[test]
    fn revocation_passes_through_the_wrapper() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Bumping(AtomicU64);
        impl Authorizer for Bumping {
            fn authorize(
                &self,
                _identity: &AuthenticatedIdentity,
                _request: AuthorizationRequest<'_>,
            ) -> Result<(), AuthError> {
                Ok(())
            }
            fn revision(&self) -> u64 {
                self.0.load(Ordering::SeqCst)
            }
            fn session_revision(&self, identity: Option<&AuthenticatedIdentity>) -> u64 {
                // Not derived from revision(), so a forwarded value is told
                // from the trait's default.
                self.revision() + if identity.is_some() { 1000 } else { 2000 }
            }
        }
        let binding = SessionBinding::new(Bumping(AtomicU64::new(7)), None);
        let alice = identity("alice");
        assert_eq!(binding.revision(), 7);
        assert_eq!(binding.session_revision(Some(&alice)), 1007);
        assert_eq!(binding.session_revision(None), 2007);
        // An unpair or a policy edit bumps the inner; the wrapper shows it.
        binding.inner.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(binding.revision(), 8);
        assert_eq!(binding.session_revision(Some(&alice)), 1008);
    }

    // The one test of the process-wide install, so it can't race another.
    #[test]
    fn the_installed_binding_serves_the_hook_listener_and_cleanup() {
        assert!(
            !bind_local("before-install"),
            "plain mode: nothing installed"
        );
        assert!(!release("before-install"));
        let local = principal("murph-local");
        let binding = SessionBinding::new(AllowAll, Some(&local));
        binding.install();
        assert!(bind_local("codex-3"));
        assert_eq!(
            binding.table().owner_of("codex-3").as_deref(),
            Some(principal_key(&local).as_str())
        );
        assert!(release("codex-3"));
        assert!(binding.table().is_empty());
        *installed() = None;
    }

    #[test]
    fn hook_registered_sessions_can_belong_to_a_paired_principal() {
        let laptop = Principal {
            issuer: "marshal-pairing".into(),
            subject: "laptop".into(),
            kind: PrincipalKind::Agent,
        };
        let desk = Principal {
            issuer: "marshal-pairing".into(),
            subject: "desk".into(),
            kind: PrincipalKind::Agent,
        };
        let binding = SessionBinding::new(AllowAll, None);
        binding.install();
        assert!(
            bind("s-laptop", &laptop),
            "a free session binds to the paired principal"
        );
        assert!(bind("s-laptop", &laptop), "binding it again is fine");
        assert!(!bind("s-laptop", &desk), "another principal can't take it");
        assert!(!bind_local("s-laptop"), "nor the local one");
        assert_eq!(
            binding.table().owner_of("s-laptop").as_deref(),
            Some(principal_key(&laptop).as_str())
        );
        assert!(held_by_another("s-laptop", &desk));
        assert!(!held_by_another("s-laptop", &laptop));
        assert!(!held_by_another("s-free", &desk), "unbound is nobody's");
        assert!(release("s-laptop"));
        assert!(bind("s-laptop", &desk), "released, it's free again");
    }

    #[test]
    fn a_paired_machines_session_set_may_not_declare_another_host() {
        fn identity(subject: &str) -> AuthenticatedIdentity {
            AuthenticatedIdentity {
                user_id: None,
                principal: Principal {
                    issuer: crate::pairing::ISSUER.into(),
                    subject: subject.into(),
                    kind: PrincipalKind::Agent,
                },
                client_id: None,
                scopes: vec![],
                delegation: vec![],
                claims: json!({}),
                expires_at: SystemTime::now() + Duration::from_secs(60),
            }
        }
        let binding = SessionBinding::new(AllowAll, Some(&identity("(local)").principal));
        let set = |proposed: serde_json::Value| AuthorizationRequest::ClientEvent {
            entity_type: Session::ENTITY_NAME_STATIC,
            change: EventChange::Set,
            proposed: &proposed,
        };
        let laptop = identity("laptop");
        assert_eq!(
            binding.authorize(
                &laptop,
                set(json!({"id": "s-attest", "host": {"name": "forged"}}))
            ),
            Err(AuthError::Forbidden),
            "a forged host name under a paired principal"
        );
        assert!(
            binding
                .authorize(
                    &laptop,
                    set(json!({"id": "s-attest", "host": {"name": "laptop"}}))
                )
                .is_ok(),
            "its own name"
        );
        assert!(
            binding
                .authorize(&laptop, set(json!({"id": "s-attest"})))
                .is_ok(),
            "no host at all is fine; the command path fills it"
        );
        let local = identity("(local)");
        assert!(
            binding
                .authorize(
                    &local,
                    set(json!({"id": "s-local", "host": {"name": "anything"}}))
                )
                .is_ok(),
            "the local principal declares what it likes"
        );
    }
}
