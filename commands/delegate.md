---
description: "Delegate a ticket to an external worker daemon: mirror it to a GitHub issue, apply the eligibility label, record the binding on the board, and claim it on the daemon's behalf."
argument-hint: "<ticket-id> [--label autofix|tryFix] [--agent minesweeper]"
---

# /kanban:delegate — hand a ticket to an external worker

Mirror a board ticket to a GitHub issue so an issue-polling daemon (e.g. minesweeper) picks it up. Manual, per-ticket
path: with the `minesweeper` toggle on this happens automatically when a ready ticket enters `doing`, and the
New-ticket modal's **Hand to minesweeper** checkbox does the same at creation.

Arguments: `$ARGUMENTS` — ticket id required; `--label` (default `autofix`); `--agent` (default `minesweeper`).

1. **Check eligibility** — `kanban_board` (note `version`): must be `ready`, in `todo`, unblocked, unclaimed, not bound.
   The daemon is dependency-blind — only feed it unblocked work; refuse (and say why) otherwise.
2. **Mirror** — `gh issue create --title "<id>: <ticket title>" --body "<ticket body>" --label <label>`. The body is the
   spec plus the **verbatim** footer `Mirrored from kanban ticket <id>.` — the dedup key that stops the toggle-on path
   mirroring it twice. Create the label first if missing.
3. **Bind** — `kanban_bind_external` (`github`/`issue`/number). The binary then never makes a worktree/branch for it;
   binding before claiming keeps the auto-hook quiet.
4. **Claim for the daemon** — `kanban_claim` with the agent name.
5. **Report** — issue URL and label, plus how it comes home: toggle on, the poller sees the PR via `Fixes #n`, moves the
   card to `review`, lands it at local-main merge. Toggle off, no issue poll: whoever spots the PR moves it
   (`kanban_move to=review branch=<head branch>`). External tickets are never auto-landed from local branch state.
