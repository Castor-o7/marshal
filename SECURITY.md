# Security

This is the Castor fork of [ignition-is-go/marshal](https://github.com/ignition-is-go/marshal).
Upstream's security posture is its own; what's below describes the fork's opt-in gate (MAR-87).

## Reporting a vulnerability

Report privately through this repository's
[security advisory form](https://github.com/Castor-o7/marshal/security/advisories/new), not in a
public issue: what you found, how to reproduce it, and what it affects. Something that also
affects upstream Marshal is reported to upstream by the fork's maintainer.

## Security model

Marshal coordinates agents on machines one operator owns, on a private network. It isn't meant
for the open internet. By default the daemon has no authentication and binds loopback; the gate
below is opt-in, and turning it on changes nothing for a daemon that doesn't.

- **TLS, opt-in.** `marshal-daemon --tls` serves TLS on both ports with a self-signed
  certificate made at first start. Clients pin its SHA-256 fingerprint; there's no certificate
  authority.
- **Pairing.** Each other machine pairs once with a one-time code from
  `marshal-daemon pair-code <name>` and keeps a token of its own. `marshal-daemon unpair <name>`
  forgets the token and closes that machine's connections within about a second. Only hashes are
  stored.
- **The daemon's host.** Its own shims and tools read a local token the daemon writes to the
  operator's config directory, readable by that user only. That host should be one only you use.
- **Session binding.** A connection, and a machine's hooks, may act only as the sessions that
  machine registered; the daemon's own records (nicknames, identities, succession) can't be
  written by any client. One paired machine can't end or send as another's session. It can still
  read: every valid principal sees the whole roster and every inbox (see the gaps below).
- **Attested host.** A paired machine's sessions carry the pairing's name as their host, whatever
  the client says. Pasted pairing codes and tokens are redacted from messages.

What this doesn't cover, and where each gap closes, is in
[docs/threat-model.md](docs/threat-model.md). In short: reads aren't scoped, so any paired machine can
read every session's mail and the roster; after a daemon restart the ownership table is empty
until each session's next hook; there are no connection limits, send allowance or sender
allow-list yet; the daemon host's own sessions are one principal; and none of it has run outside
loopback tests yet.
