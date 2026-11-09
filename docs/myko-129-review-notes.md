# Notes on myko#129 for MAR-37

Fork-only. This is a read of ignition-is-go/myko#129 (trevorsargent's auth PR), first at `fcdbc52b`
and again at `f6028125`, checked against the four attackers in muster's threat model. The comment
below is for Josh to post on the PR if he wants to, after changing it however he likes. Nothing has
been posted.

## Where it stands (checked 2026-11-09, evening)

- Still open, not a draft. The head moved today from `fcdbc52b` to `f6028125` (5 new commits,
  2026-11-09): live permission administration, per-identity policy revisions, shared typed user
  references, MCP notifications without request ids, MCP OAuth resource discovery, and a refactor
  that centralizes OIDC verification and provider login.
- No reviews and no comments on the PR yet.
- It's based on myko main at `bd43bea9` (v6.11.6), 14 commits ahead and 0 behind.
- The other auth PR, myko#26 (optional per-command OIDC/JWT), is still open and untouched since
  2026-06-19.

Code claims were checked by reading the PR's code at `f6028125`. Claims marked *(docs)* come from
`docs/auth/design.md` and weren't traced in the code. Marshal itself was built and tested against
the PR (next section), but no auth provider was switched on, so that run shows #129 doesn't break
Marshal. It doesn't show that the attackers are stopped.

## Building Marshal against #129

Castor-o7/marshal#78 (a draft that is never merged) pins Marshal's four myko crates to the PR by
`rev` and runs the fork's CI on GitHub's runners, so the PR's build scripts and macros never run on
Murph.

- At `fcdbc52b`: build, clippy and workspace tests (`check`), the conformance suite, the Windows
  cross-check and the native Windows wake test all passed with no code changes in Marshal. Run
  42678226247.
- `cargo deny` failed at first, for two reasons. Both are about the draft's setup, not #129's code:
  1. **`myko-auth` is a new crate under AGPL-3.0-or-later.** That's the same licence as every other
     myko crate, but `deny.toml` lists its AGPL exceptions one crate at a time (so that no other AGPL
     dependency slips in), and `myko-auth` isn't on the list. **Taking #129 means adding it, which
     is Josh's call.** The draft adds it so the rest of the audit can run.
  2. The draft's `rev`-only git pins have no `version`, so cargo-deny counts them as wildcard
     dependencies. Adding `version = "6.11.6"` next to the `rev` fixes that.
  With both fixed (`2dffeeb`), `cargo deny check` passed too.
- At `f6028125` (draft head `3508c8a`, with `Cargo.lock` now recording the git crates): all five
  checks passed, `cargo deny` included. Run 42679474846.

## What it gives Marshal

| Attacker | Covered by #129? | What Marshal would still add |
|---|---|---|
| A web page in a browser | Once auth is on, yes: a page can't present a bearer token, so it's refused. With auth off, nothing changes | A token for the shim and local tools; refusing a browser `Origin` as a second line; the hook listener, which is outside myko |
| One session claiming to be another | No, but the hook is there: `Authorizer::authorize` sees the identity and each command's arguments | A table of which sessions each token owns, and a check that `asSession` is one of them |
| Another LAN device | The token, yes. TLS, no: the server accepts plain TCP, and TLS is left to whoever deploys it | TLS with a pinned certificate; connection and rate limits |
| A stolen machine | Revoking one credential works, through `validate_identity` returning `Revoked`. The only authenticator shipped is Auth0 (still so at `f6028125`) | A local pairing and token store, and `unpair` built on `validate_identity` |

With no provider configured, existing behaviour is unchanged (`server/src/router.rs:312-361` at
`f6028125`). Nothing is public by default. New at `f6028125`: when MCP OAuth discovery is configured,
the MCP route always requires a token and never admits anonymously, and the discovery metadata URLs
must be `https` (or `http` on loopback only).

## Draft comment for the PR

> Thanks for this. I've been reading it with Marshal in mind (ignition-is-go/marshal), where I'd
> like to build opt-in auth on it rather than on a patched myko-server. The extension points look
> right for that, especially `authorize` seeing the command arguments, and today's per-identity
> `session_revision` and `with_live_permissions` answer what I was going to ask about revoking one
> principal without disconnecting everyone. Marshal builds and passes its tests and conformance suite
> against `f6028125` with no changes (I haven't switched a provider on yet). A few questions and one
> possible bug:
>
> 1. **Possible bug:** in `ws_handler.rs` (around line 1970 at `f6028125`), when the recheck before
>    running a queued command fails, the function returns without sending a `CommandError`, so the
>    client's transaction never gets a reply.
> 2. Could `authorize` get the connection's context (client id, peer address)? Marshal needs it to
>    check that a connection acts only as the sessions it registered.
> 3. Anonymous admission turns on for the whole server if any linked crate registers a public item
>    (`public_access.rs:49-51`, `auth/src/lib.rs:227`). Could that be explicit on the provider?
> 4. Would an opt-in `Origin` allowlist on the WebSocket and MCP routes fit here? Bearer tokens
>    cover browsers today, but not anonymous mode.
> 5. Is a local, non-Auth0 authenticator (static or pairing tokens) in scope, or left to apps?
> 6. Would you want the server to refuse a bearer token over plain `ws://`, or offer TLS itself?
>
> Happy to help test any of this.

## Also worth knowing

- An earlier version of these notes said the PR's base was myko 6.0.0. That was wrong: 6.0.0 is
  only the root `[workspace.package]` version. The crates themselves are at 6.11.6, both at the PR's
  head and on main. Marshal pins `=6.11.5`. The only code change from 6.11.5 to 6.11.6 is #127's
  `relationship_manager.rs` fix, so taking #129 means bumping the pin one patch, not a rebase.
- A point in the first draft of the comment (a policy revision change disconnects every client) is
  dropped: at `f6028125` an `Authorizer` can return a per-identity `session_revision`, and
  `with_live_permissions` rechecks live subscriptions instead of disconnecting.
- `validate_identity` runs several times per message, so a token-store lookup behind it must be cheap.
