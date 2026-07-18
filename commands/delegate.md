---
description: "Delegate a ticket to an external worker daemon: mirror it to a GitHub issue, apply the eligibility label, record the binding on the board, and claim it on the daemon's behalf."
argument-hint: "<ticket-id> [--label autofix|tryFix] [--agent minesweeper]"
---

# /kanban:delegate — hand a ticket to an external worker

Mirror a board ticket to a GitHub issue so an issue-polling daemon (e.g. minesweeper) picks it up. The `gh` calls live
here in this skill — the kanban binary itself never touches the network.

Arguments given: `$ARGUMENTS`
- The ticket id is required (e.g. `K-7`).
- `--label` is the eligibility label the daemon watches for. Default: `autofix`.
- `--agent` is who the claim names as owner. Default: `minesweeper`.

## Steps

1. **Check eligibility** — `kanban_board` (remember the `version`). The ticket must be `ready`, in `todo`, unblocked,
   unclaimed, and not already bound to an external item. The daemon is dependency-blind — the board's job is to only
   ever feed it unblocked work, so refuse (and say why) if any of this fails.
2. **Mirror** — create the issue in this repo:
   `gh issue create --title "<ticket title>" --body "<ticket body>" --label <label>`
   The body should carry the full spec plus a footer line `Mirrored from kanban ticket <id>.` If the label doesn't
   exist yet, create it (`gh label create <label>`) and mention that you did.
3. **Bind** — `kanban_bind_external` with `provider: "github"`, `kind: "issue"`, and the new issue's number. From here
   on the binary knows this ticket is worked elsewhere: it will never get a worktree or branch locally.
4. **Claim for the daemon** — `kanban_claim` with the agent name. The card moves to `doing` and shows who has it.
5. **Report** — the issue URL and the label applied, plus how the ticket comes home: once the daemon's work is
   code-complete (its PR is open), whoever notices moves the card to `review`, recording the daemon's head branch —
   `kanban_move to=review branch=<the PR's head branch>`. From there the serve poller tracks the PR by that branch
   and lands the card in `done` when the merge reaches the **local** main branch. External tickets are never
   auto-landed from local branch state — without a PR to track, retiring the card stays the human's call.
