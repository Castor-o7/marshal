# Notes on myko#129 for MAR-37

Fork-only. This is a read of ignition-is-go/myko#129 (trevorsargent's auth PR) at `fcdbc52b`,
checked against the four attackers in muster's threat model. The comment below is for Josh to
post on the PR if he wants to, after changing it however he likes. Nothing has been posted.

## Where it stands (checked 2026-11-09)

- Still open, not a draft, and mergeable. The head is still `fcdbc52b` (2026-11-06), 9 commits, +15,116 / -174.
  Nothing has been pushed since this read, so the notes and draft below still match the code.
- No reviews, no review decision, and no comments on the PR yet.
- It's based on myko main at `bd43bea9` (v6.11.6), 9 commits ahead and 0 behind. Main hasn't moved
  since.
- The other auth PR, myko#26 (optional per-command OIDC/JWT), is still open and untouched since
  2026-06-19.

Code claims were checked by reading the PR's code. No tests were run. Claims marked *(docs)* come
from `docs/auth/design.md` and weren't traced in the code.

## What it gives Marshal

| Attacker | Covered by #129? | What Marshal would still add |
|---|---|---|
| A web page in a browser | Once auth is on, yes: a page can't present a bearer token, so it's refused. With auth off, nothing changes | A token for the shim and local tools; refusing a browser `Origin` as a second line; the hook listener, which is outside myko |
| One session claiming to be another | No, but the hook is there: `Authorizer::authorize` sees the identity and each command's arguments | A table of which sessions each token owns, and a check that `asSession` is one of them |
| Another LAN device | The token, yes. TLS, no: the server accepts plain TCP, and TLS is left to whoever deploys it | TLS with a pinned certificate; connection and rate limits |
| A stolen machine | Revoking one credential works, through `validate_identity` returning `Revoked`. The only authenticator shipped is Auth0 | A local pairing and token store, and `unpair` built on `validate_identity` |

With no provider configured, existing behaviour is unchanged (`server/src/router.rs:293-330`).
Nothing is public by default.

## Draft comment for the PR

> Thanks for this. I've been reading it with Marshal in mind (ignition-is-go/marshal), where I'd
> like to build opt-in auth on it rather than on a patched myko-server. The extension points look
> right for that, especially `authorize` seeing the command arguments. A few questions and one
> possible bug, from reading `fcdbc52b` (I haven't run the suites):
>
> 1. **Possible bug:** in `ws_handler.rs` (around line 1851), when the recheck before running a
>    queued command fails, the function returns without sending a `CommandError`, so the client's
>    transaction never gets a reply.
> 2. Could `authorize` get the connection's context (client id, peer address)? Marshal needs it to
>    check that a connection acts only as the sessions it registered.
> 3. Anonymous admission turns on for the whole server if any linked crate registers a public item
>    (`public_access.rs:50-52`, `auth/src/lib.rs:186-188`). Could that be explicit on the provider?
> 4. Changing a policy revision disconnects every client (`auth/src/lib.rs:279`). Is there a way to
>    revoke one principal in the built-in store without that? *(partly docs)*
> 5. Would an opt-in `Origin` allowlist on the WebSocket and MCP routes fit here? Bearer tokens
>    cover browsers today, but not anonymous mode.
> 6. Is a local, non-Auth0 authenticator (static or pairing tokens) in scope, or left to apps?
> 7. Would you want the server to refuse a bearer token over plain `ws://`, or offer TLS itself?
>
> Happy to help test any of this.

## Also worth knowing

- An earlier version of these notes said the PR's base was myko 6.0.0. That was wrong: 6.0.0 is
  only the root `[workspace.package]` version. The crates themselves are at 6.11.6, both at the PR's
  head and on main. Marshal pins `=6.11.5`. The only code change from 6.11.5 to 6.11.6 is #127's
  `relationship_manager.rs` fix, so taking #129 means bumping the pin one patch, not a rebase.
  Building Marshal against #129's head hasn't been tried.
- `validate_identity` runs several times per message, so a token-store lookup behind it must be cheap.
