# What the fork could offer upstream (MAR-66)

**Nothing here has been sent upstream.** This is a list for Josh to choose from: the fork's merged
changes that could go to ignition-is-go/marshal as small additive PRs. Default changes aren't
offered as PRs; they go in MAR-15's single issue
([upstream-defaults-issue-draft.md](upstream-defaults-issue-draft.md)), per "How work lands" in
the [roadmap](roadmap.md).

Read on 2026-11-09 against upstream `main` at `ba6c699` (v0.58.1) and fork `main` at `4e84a5f`.

**How "applies" was checked:** in a throwaway worktree at upstream `main`, the story's own commits
were cherry-picked in order with `git cherry-pick --no-commit`, and then the worktree was reset.
Those are the PR's own non-merge commits, not counting any already on upstream. PRs #2, #5, #7,
#8, #9, #10, #11, #12, #13, #14, #15, #17, #18 and #19 were squash-merged, so the commits named for
them are on the PR's branch rather than on fork `main`, where each PR is one squash commit. A PR
upstream would be cut from the commits named here.

- **Clean** means no conflicts at all.
- **Clean bar the roadmap** means the only conflict was in `docs/roadmap.md`, which upstream
  doesn't have. That hunk is fork bookkeeping and is dropped from any upstream PR.
- **After X** means the story conflicts by itself, but applies cleanly once X's commits are picked
  first. It's stacked on X.
- **Size** is the story's own diff on the fork, without the roadmap.

Only MAR-58 + MAR-64 was also built on upstream (`cargo check -p marshal-shim`, which passed). Nothing
else was built or tested on upstream, so each PR still needs a build and its tests there.

## Ready on their own

Each of these applies to upstream `main` by itself.

| Story | Upstream-shaped commit(s) | Applies | Size | Notes |
|---|---|---|---|---|
| MAR-39: the README's default bind | `7f4255c` | Clean | +2 −2, 1 file | Docs only. Its roadmap entry says it can go as it stands. |
| MAR-63: Windows `claude_hook` tests read a non-blocking stream | `b547777` | Clean | +7, 1 file | Test only. Upstream has the same flaky test. Branched from fork `main`, not upstream, but touches only `crates/shim/tests/claude_hook.rs`. |
| MAR-33: `claude_push_ack_e2e` reads the agent id until the view catches up | `a348841` | Clean | +45 −28, 1 file | Test only. |
| MAR-44: a Claude waiter stands down when its session's id changes | `17d2dee` | Clean | +56 −1, 2 files | Bug fix. |
| MAR-50: a session leaves its old `op:` room | `9bbb999` | Clean | +139, 2 files | Bug fix: a broadcast to the old operator's room reached it. |
| MAR-49: one error names every missing tool argument | `7628e76` | Clean | +271 −53, 2 files | Branched from upstream `main`. |
| MAR-45: trust a Claude session manifest only if it's live | `b1be494` | Clean | +338 −38, 1 file | Bug fix. |
| MAR-58 then MAR-64: the roster says when a session's activity last changed | `5fd49be`, then `9b7a92c` | Clean, and builds | +371 −37, 20 files | `5fd49be` is on upstream `main`, and `9b7a92c` is on it. A new optional `activity_at` on `Session`. Leave out `f520b3d`, which edits the fork-only `marshal-shim roster`. |
| MAR-36: `marshal-shim doctor`, a read-only health check | `a78154c`, `fd012f6` | Clean | +592 −2, 2 files | `a78154c` is on upstream `main`. |
| MAR-60: doctor checks a project's own Claude Code wiring | `8d7494c` | After MAR-36 | +279 −44, 1 file | |
| MAR-1: safe retry of a timed-out send (idempotency key) | `60c5a64`, `72145b0` | Clean | +557 −46, 16 files | An optional `idempotencyKey` on `SendMessage`. Upstream behaves as before without it. |
| MAR-7: Codex threads re-register after a daemon restart | `f37208c`, `330ae95` | Clean | +277 −20, 2 files | |
| MAR-14: the delivery conformance suite | `024912d`, then `3d8c6d5` | Clean | +2856, 15 files | The roadmap calls it the most useful to upstream. `024912d` is the suite. `3d8c6d5` adds the fork's CI job against Postgres, written for GitHub-hosted runners, so upstream would want that job written for their own runners. |
| MAR-20: `current_task` follows the branch | `6467108` | Clean | +94 −5, 1 file | A change of behaviour, not only an addition. Josh to say whether it's a default for MAR-15 instead. |

## Stacked on other offers

Each of these applies cleanly once the stories named are picked first.

| Story | Commit(s) | Applies | Size | Notes |
|---|---|---|---|---|
| MAR-12: a Codex bridge's wake path on the roster | `273a00b`, `09132e0` | After MAR-7 | +530 −34, 6 files | |
| MAR-4: one wake path ending doesn't clear another's | `00793ed` | After MAR-1, MAR-31, MAR-7 and MAR-12 | +306 −29, 9 files | Needs MAR-31 for its `roster_read.rs` hunk, since that file is MAR-31's roster command. |
| MAR-47: say what a send with no live channel means | `15206d8` | After MAR-1 | +50 −1, 1 file | |
| MAR-46: `marshal-daemon forget` removes a stuck session | `069c6eb` | After MAR-1, MAR-31, MAR-7, MAR-12 and MAR-4 | +758, 7 files | Not tried on a shorter stack. |
| MAR-13: the OpenCode plugin declares its channel | `d4feeed` | After MAR-14 | +104 −4, 5 files | |
| MAR-10: plugins ack a push only after the harness takes it | `b87fc81`, `b4fb02b`, `5483337` (#33, #36, #37) | Code clean; conflicts in `.github/workflows/ci.yml` | +1349 −196, 23 files | The CI conflict is the fork's runner changes (MAR-2, MAR-34). The plugin test jobs would need adding to upstream's own workflow by hand. |
| MAR-48: push plugins get mail that waited out a reconnect | `c9a9989` | Conflicts by itself in `ci.yml` and the plugins. Not tried past MAR-10's `ci.yml` conflict | +849 −22, 16 files | Built on MAR-10. |
| MAR-16: cargo-deny dependency audit in CI | `d010cc4` | `deny.toml` clean; conflicts in `ci.yml` | +117 −25, 3 files | As for MAR-10: the job needs adding to upstream's workflow by hand. |
| MAR-53: keep messages across a restart without Postgres (opt-in) | `9056813`, `00b907e` | Still conflicts in `docs/delivery-conformance.md` after MAR-39, MAR-14, MAR-13 and MAR-19 | +1278 −19, 7 files | Opt-in, so not a default. Its code applied. The doc conflict is probably text a story not in that stack changed first, such as MAR-9 or MAR-42, but I didn't check which. |
| Launcher: MAR-26, 27, 28, 32, 30 and 29 (`spawn`, `spawn-team`, list, attach, stop, windows) | `9af71ab` `75b04fd` `f96b377` `b2d858b` `0422bf2` `07b2674` `b479e98` `da3e8cb` `5468eed` | After MAR-1, MAR-31, MAR-7, MAR-12 and MAR-4 | +2759 −144 | Goal 2. A new feature, so a larger offer: one PR per story, in that order, would be reviewable. |
| MAR-57: `marshal-shim roster` answers `[]` at once | `6444358` | After MAR-31 | +94 −21, 2 files | Only matters if upstream takes MAR-31's roster command (see below). |
| MAR-21 and MAR-56: tests pick ports without races, and never touch the real HOME or daemon | `b81b064` `37bf335` `5ab6f64`; `de36b1e` `217d3b4` | Not tried as a stack | +556 −163, 14 files; +313 −51, 19 files | Test infrastructure that touches tests from most stories above (conformance, spawn, terminal). Offer after them. |

## Already upstream, or in flight there

| Story | Upstream | Notes |
|---|---|---|
| MAR-3: never offer a message twice | #169, open | The fork took upstream's fix (`a1630ae`, clean). Nothing to offer. |
| MAR-31: terminal `tell`, `all` and `roster` | #168, open | The fork's port (`3af92d7` `8d53df3` `cc70949` `188dbc7`, clean) overlaps #168. Better as a comment on #168 than as a second PR. MAR-17's list ([muster-terminal-gap.md](muster-terminal-gap.md)) is what #168 doesn't cover. |

## Default changes: for MAR-15's issue, not PRs

Each of these is a change of default that MAR-15's draft lists or depends on.

| Story | Commit(s) | Applies | Notes |
|---|---|---|---|
| MAR-6: keep a session through a drop while its waiter is held | `007022b` | Clean bar the roadmap | Listed in MAR-15. |
| MAR-43: hold a shim's first write until it's registered | `4dd6bb8` | Clean bar the roadmap | Listed in MAR-15. |
| MAR-18: a stable code for the unregistered refusal | `9f0f950` | After MAR-43 | Additive by itself, but it names MAR-43's refusal, so it goes with it. |
| MAR-9: nickname across a restart | `0c5955c` | Conflicts by itself | Listed in MAR-15. Not tried on a stack. |
| MAR-42: unread mail follows the successor | `17dd317` | Conflicts by itself. Built on MAR-9, not tried on it | Listed in MAR-15. |
| MAR-54 and MAR-62: Codex rows can count as dead, and its tests | `fcba54c` `1ca20bf`; `84160d3` | Conflict by themselves. Built on MAR-9 and MAR-42 | Succession, built on MAR-9 and MAR-42. |
| MAR-19: drop ended sessions from rooms | `d80d43d` `8495391` `79d79f8` | After MAR-14 | Listed in MAR-15. |

## Fork-only: never offered

MAR-2 (GitHub-hosted runners), MAR-34 (self-hosted workflows run only upstream), MAR-22 (a CI time limit, with no commit of its own found), MAR-59 (the cutover kit), MAR-11 (dropped), and the fork's docs: the roadmap, the gate
drafts, the myko#129 notes and this file.
