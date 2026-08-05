---
description: "Delegate a ticket to an external worker daemon: mirror it to a GitHub issue, apply the eligibility label, record the binding on the board, and claim it on the daemon's behalf."
argument-hint: "<ticket-id> [--label autofix|tryFix] [--agent minesweeper]"
---

# /kanban:delegate — hand a ticket to an external worker

Mirror a board ticket to a GitHub issue so an issue-polling daemon (e.g. minesweeper) picks it up. This is the
**manual, per-ticket path**: when the project's `minesweeper` config toggle is on, all of this happens automatically
the moment a ready ticket enters `doing` (a claim or a browser drag), and the browser's New-ticket modal has a
**Hand to minesweeper** checkbox doing the same for a single ticket at creation, toggle or no toggle. Use this skill
for tickets that already exist on a toggle-off board, or for a one-off with a different label or agent.

Arguments given: `$ARGUMENTS`
- The ticket id is required (e.g. `K-7`).
- `--label` is the eligibility label the daemon watches for. Default: `autofix`.
- `--agent` is who the claim names as owner. Default: `minesweeper`.

## Steps

1. **Check eligibility** — `kanban_board` (remember the `version`). The ticket must be `ready`, in `todo`, unblocked,
   unclaimed, and not already bound to an external item. The daemon is dependency-blind — the board's job is to only
   ever feed it unblocked work, so refuse (and say why) if any of this fails.
2. **Mirror** — create the issue in this repo:
   `gh issue create --title "<id>: <ticket title>" --body "<ticket body>" --label <label>`
   The body should carry the full spec plus a footer line `Mirrored from kanban ticket <id>.` — **verbatim**: that
   footer is the automatic hook's dedup key, and matching it is what stops the toggle-on path from ever mirroring this
   ticket a second time. If the label doesn't exist yet, create it (`gh label create <label>`) and mention that you did.
3. **Bind** — `kanban_bind_external` with `provider: "github"`, `kind: "issue"`, and the new issue's number. From here
   on the binary knows this ticket is worked elsewhere: it will never get a worktree or branch locally. Binding
   **before** claiming also keeps the automatic hook quiet — a bound ticket entering doing is never re-mirrored.
4. **Claim for the daemon** — `kanban_claim` with the agent name. The card moves to `doing` and shows who has it.
5. **Report** — the issue URL and the label applied, plus how the ticket comes home: when the toggle is on, the serve
   poller discovers the daemon's PR through the issue's closing reference (`Fixes #n`), moves the card to `review`
   with the PR recorded, and lands it in `done` when the merge reaches the **local** main branch. With the toggle off
   there is no issue poll, so whoever notices the PR moves the card themselves —
   `kanban_move to=review branch=<the PR's head branch>` — and the branch-based PR poller takes it from there.
   External tickets are never auto-landed from local branch state — without a PR to track, retiring the card stays
   the human's call.
