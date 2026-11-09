# The web UI's git crates and their licences (MAR-41)

**Fork-only research. Nothing here has been sent upstream.** Josh raises licensing with
ignition-is-go himself; the ask below is a draft for him to change and send, or not. Read on
2026-11-09 by cobalt-glen, read-only: no issues, comments or PRs were opened upstream.

## What we depend on

marshal-ui is the only crate that pulls these in, and it does so through one dependency:

```
marshal-ui → mullion → css-styled → css-styled-derive → css-spec-data
                                  ↘ css-spec-data
```

| Crate | Repository | Rev pinned in our `Cargo.lock` | Upstream `main` when read |
|-------|------------|--------------------------------|---------------------------|
| mullion 1.2.0 | [ignition-is-go/mullion](https://github.com/ignition-is-go/mullion) | [`e8bfe03`](https://github.com/ignition-is-go/mullion/tree/e8bfe0344b2d1bebff2bdd1fa65a86319c71df20) | `5a5d172` (1.4.0) |
| css-styled 0.1.0 | [ignition-is-go/css-styled](https://github.com/ignition-is-go/css-styled) | [`3c48c5a`](https://github.com/ignition-is-go/css-styled/tree/3c48c5ac14df679511288e32480e87895ecba241) | `6360b30` |
| css-styled-derive 0.1.0 | same repository, `css-styled-derive/` | `3c48c5a` | `6360b30` |
| css-spec-data 0.1.0 | same repository, `css-spec-data/` | `3c48c5a` | `6360b30` |

`crates/ui/Cargo.toml` pins mullion by rev. mullion's own manifest takes css-styled from git with no
rev, so our `Cargo.lock` is what fixes css-styled at `3c48c5a`. mullion's optional
`leptos-command-palette` (also git) isn't enabled, so it isn't in our graph.

## What upstream says about licensing

Read at both the pinned revs and upstream `main`; the answer is the same at all four.

| Check | mullion | css-styled (all three crates) |
|-------|---------|-------------------------------|
| `LICENSE` / `LICENSE-*` / `COPYING` file | none | none |
| `license` or `license-file` in `Cargo.toml` | none (crate manifest; no `[workspace.package]`) | none (workspace root or any of the three crate manifests) |
| GitHub's detected licence | none (`license: null`) | none (`license: null`) |
| README | ends with `## License` / `MIT` | ends with `## License` / `MIT` |
| Issues or PRs mentioning a licence | none | none |

Both repositories are public. Most commits are by Trevor Sargent ("Trev"). mullion has a second
contributor ("max") and release commits by `github-actions[bot]`. No copyright holder is named
anywhere.

**So the MAR-16 wording ("declare no licence") is slightly too strong.** The authors do state an
intent: MIT, in a README line. What's missing is what makes that usable and checkable:

- **No licence text and no copyright notice.** MIT's one condition is that the copyright notice and
  the permission notice ship with copies. With no notice upstream, there's nothing to ship, and no
  named holder granting the permission.
- **No machine-readable field.** cargo-deny (and crates.io, and any downstream audit) reads
  `license` in `Cargo.toml`, not READMEs. That's why the check can't pass them, not just a
  configuration choice on our side.

This isn't legal advice. Josh decides whether the README line is enough for Marshal's purposes; the
gap that keeps them out of `cargo deny` is mechanical and only upstream can close it.

## What `deny.toml` does today

- `[graph] exclude = ["mullion", "css-styled", "css-styled-derive", "css-spec-data"]`. The check
  never visits them, so neither their licences nor anything only they pull in is checked.
- That exclusion also hides two build-time findings that come only through these crates:
  css-spec-data's build script fetches the W3C spec with `ureq` (bringing `webpki-roots` 0.26), and
  css-styled-derive's macro uses `lightningcss` → `parcel_sourcemap` → `rkyv` 0.7
  (RUSTSEC-2026-0235; its fix needs `parcel_sourcemap` on rkyv 0.8).
- `[sources]` allows only crates.io; `unknown-git = "deny"`. The two git repositories aren't listed,
  on purpose, because the exclusion means the check never reaches them.

Removing the exclusion (option A or B) is MAR-41's "Done when".

## Option A: ask upstream to add the licence

Small for upstream: one file and one manifest line per repository. When it lands:

1. Bump the mullion rev in `crates/ui/Cargo.toml` (or `cargo update -p mullion -p css-styled`) to
   the commits that add it.
2. Remove the `[graph] exclude` block.
3. Add `allow-git = ["https://github.com/ignition-is-go/mullion", "https://github.com/ignition-is-go/css-styled"]`
   under `[sources]`.
4. Handle what the check then sees: RUSTSEC-2026-0235 needs an `ignore` entry with the
   build-time-only reason (or the rkyv 0.8 fix upstream), and `webpki-roots` 0.26 should already
   fall under the duplicate-versions warning. Confirm with `cargo deny check` before claiming it
   passes.

This keeps the UI exactly as it is. Draft ask, for Josh to edit, one per repository (or one message
to Trev covering both):

> **Add a LICENSE file and a `license` field?**
>
> Hi, we use mullion (and through it css-styled) in Marshal's web UI. Thanks for them! The READMEs
> say MIT, but there's no LICENSE file and no `license` field in the `Cargo.toml`s, so dependency
> checkers like cargo-deny can't see the licence, and there's no copyright notice for downstream
> users to keep with copies.
>
> Would you be open to adding:
>
> - a `LICENSE` (or `LICENSE-MIT`) file with the MIT text and a copyright line, and
> - `license = "MIT"` in each crate's `Cargo.toml` (for css-styled: `css-styled`,
>   `css-styled-derive`, `css-spec-data`, or once in `[workspace.package]` with
>   `license.workspace = true` in each)?
>
> Happy to send the PR if that's easier. Just tell us who the copyright line should name.

## Option B: drop or replace the crates

All four go together: css-styled and its parts come only through mullion, so the UI only has to stop
using mullion.

**How much we use.** Everything is in `crates/ui/src/lib.rs` (406 lines). We use `MullionRoot`, the
activity types (`ActivityDef`, `ActivityIcon`, `ActivityId`, `ActivityNode`, `Category`,
`CategoryId`) for six panels (roster, messages, rooms, session detail, sandbox, console), and
`PaneNode`/`PaneId`/`PaneEvent::TreeChanged` to save and restore the layout in localStorage. The
styling comes from pulse-leptos-ui's `PulseMullionStyle` / `PULSE_MULLION_CLASS` (crates.io), which
would have to go or be redone.

**What we'd replace.** Rust lines at the pinned revs:

| Crate | Files | Lines | What it does for us |
|-------|-------|-------|---------------------|
| mullion | 23 | 8,446 | Splittable, resizable, dockable pane tree with activity switching and drag-and-drop |
| css-styled | 8 | 767 | Scoped-CSS traits used by mullion's components |
| css-styled-derive | 7 | 1,754 | Compile-time CSS validation macros (via lightningcss) |
| css-spec-data | 5 | 827 | Fetches and caches W3C CSS spec data at build time |

Rough sizes, for our own code:

- **B1, fixed layout (small, about a day).** Replace the pane workspace with a tab or sidebar
  switcher over the six panels: perhaps 150 to 300 lines in marshal-ui, and the layout persistence
  shrinks to "last tab". The cost is the feature: no split or side-by-side panes.
- **B2, own split panes (medium, several days).** A two-level split tree with resize handles
  (`leptos-resize-handle` is on crates.io, and its licence needs checking first) plus localStorage
  persistence: perhaps 800 to 1,500 lines, without mullion's drag-to-dock or activity bar. It brings
  new UI code to maintain and test.
- **B3, a published alternative.** None checked here. Any candidate would need a crates.io licence
  that `deny.toml` already allows, and leptos 0.8 CSR support.

Either B option also drops css-spec-data's network fetch during the build, and rkyv 0.7.

## Recommendation for Josh

Option A first. It's a five-minute change upstream, the authors already state MIT, and it keeps a
UI we've built on. Option B1 is the fallback if upstream doesn't answer or doesn't want to. B2
only makes sense if split panes turn out to matter to operators.
