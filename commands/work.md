---
description: "Work the board: claim the next eligible ticket — implement it (ready) or flesh out its spec (stub) — move it across, repeat. Running this is the opt-in — never claim tickets outside a loop the user started."
argument-hint: "[ticket-id] [--one] [--push]"
---

# /kanban:work — the policy loop

You are working this project's Kanban board. The user starting this command is your authorisation to claim tickets
one after the next; outside a running loop you never claim spontaneously.

Arguments given: `$ARGUMENTS`
- A ticket id (e.g. `K-7`) means work exactly that ticket (it must still be ready or stub, in todo, and unblocked).
- `--one` means stop after a single ticket instead of looping.
- `--push` means after finishing each ticket, push its branch and open a PR with `gh`. WITHOUT this flag nothing
  leaves the machine: no pushes, no PRs — report branch names and stop there.

## The loop

Repeat until `kanban_next` reports nothing eligible (or you've done the one requested ticket):

1. **Pick** — call `kanban_board` (remember the `version`), then `kanban_next`. Its `action` field says what the ticket
   needs: `implement` (a ready ticket — steps 2–8) or `refine` (a stub — see **Refining a stub** below). If nothing is
   eligible, report why the remaining todo tickets don't qualify (draft/review status? blocked? claimed? external?)
   and stop the loop.
2. **Claim** — `kanban_claim` the ticket. A pure board mutation; git is untouched.
3. **Start** — `kanban_worktree_start`. Supply a `slug` yourself: a short kebab-case digest of the title
   (2–3 words, e.g. "Add authorization based on OAuth from Google" → `google-oauth`) beats the mechanical default.
4. **Work** — `cd` into the reported worktree path and stay there for the ticket's lifetime. Read the ticket's `body` as
   the spec. Commit after each logical chunk — the worktree may live on volatile /tmp, and commits are what survive; but
   don't spam micro-commits. If a subtask emerges mid-ticket, work it in this same worktree on this same branch —
   never create a worktree from inside a worktree.
5. **Note** — `kanban_note` progress at meaningful moments: what landed, what's left, anything surprising. The human
   watches these appear live on the card.
6. **Verify** — run the project's tests/build before calling anything done. A ticket whose tests fail is not done:
   note the failure and either fix it or release the ticket with a note explaining the blocker.
7. **Finish** — `kanban_worktree_finish` (it refuses if you left uncommitted changes — commit them first; never
   `force_discard` without explicit human approval). Never pass `merge` unless the user asked for it.
8. **Close out** — `kanban_move` the ticket to `done`. Report the branch name prominently: integrating it is the
   user's explicit next step. With `--push`: `git push -u origin <branch>` and `gh pr create` (title from the ticket,
   body summarising the work and linking the ticket id), then include the PR URL in the report.

## Refining a stub

A stub is a spec to write, not code to build. When `kanban_next` says `action: "refine"`:

1. `kanban_claim` it — the card sits pink in `doing` while you write, so the human sees refinement in flight.
2. **No worktree.** Refinement produces a spec, not commits; stay in the main checkout and touch nothing.
3. Research the codebase until you can write a precise, implementable spec: what to change, where, how to verify.
4. `kanban_refine` with the fleshed-out `body` (and a sharper `title` if you found one). If the stub is really several
   units of work, pass `split_tickets`/`split_epics` in the same call — it is atomic. The tool lands everything in
   `review`, returns the card to the top of `todo`, and drops your claim.
5. Continue the loop. Don't implement what you just specced — the human vets `review` tickets and promotes to `ready`.

## Rules

- Only `ready` (implement) or `stub` (refine), unblocked, unclaimed, non-external tickets. Never touch `draft`
  tickets at all.
- Every mutating kanban tool needs `expected_version` from your latest `kanban_board` read. On a version conflict,
  re-read the board and retry the operation against the new state.
- If a ticket turns out to be much bigger than its spec, don't silently balloon: `kanban_note` the discovery, create
  follow-up tickets with `kanban_create_ticket` (they land in `review` for the human to vet), and finish the
  original at its honest scope.
- If genuinely stuck, `kanban_note` why, `kanban_release` the ticket (it returns to the top of todo), clean up with
  `kanban_worktree_finish`, and move to the next ticket.
- At the end of the loop, summarise: tickets completed, branches created (and PRs, with `--push`), tickets released
  or split, and what the board looks like now.
