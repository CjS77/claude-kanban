---
description: "Work the board: claim the next ready ticket, work it in its own worktree, move it across, repeat. Running this is the opt-in — never claim tickets outside a loop the user started."
argument-hint: "[ticket-id] [--one] [--push]"
---

# /kanban:work — the policy loop

You are working this project's Kanban board. The user starting this command is your authorisation to claim tickets
one after the next; outside a running loop you never claim spontaneously.

Arguments given: `$ARGUMENTS`
- A ticket id (e.g. `K-7`) means work exactly that ticket (it must still be ready, in todo, and unblocked).
- `--one` means stop after a single ticket instead of looping.
- `--push` means after finishing each ticket, push its branch and open a PR with `gh`. WITHOUT this flag nothing
  leaves the machine: no pushes, no PRs — report branch names and stop there.

## The loop

Repeat until `kanban_next` reports nothing eligible (or you've done the one requested ticket):

1. **Pick** — call `kanban_board` (remember the `version`), then `kanban_next`. If nothing is eligible, report why the
   remaining todo tickets don't qualify (draft/stub/review status? blocked? claimed? external?) and stop the loop.
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

## Rules

- Only `ready`, unblocked, unclaimed, non-external tickets. Never touch `draft` tickets at all.
- Every mutating kanban tool needs `expected_version` from your latest `kanban_board` read. On a version conflict,
  re-read the board and retry the operation against the new state.
- If a ticket turns out to be much bigger than its spec, don't silently balloon: `kanban_note` the discovery, create
  follow-up tickets with `kanban_create_ticket` (they land in `review` for the human to vet), and finish the
  original at its honest scope.
- If genuinely stuck, `kanban_note` why, `kanban_release` the ticket (it returns to the top of todo), clean up with
  `kanban_worktree_finish`, and move to the next ticket.
- At the end of the loop, summarise: tickets completed, branches created (and PRs, with `--push`), tickets released
  or split, and what the board looks like now.
