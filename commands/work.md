---
description: "Work the board: claim the next eligible ticket — implement it (ready) or flesh out its spec (stub) — move it across, repeat; when the board runs dry, idle and re-poll. Running this is the opt-in — never claim tickets outside a loop the user started."
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

## Picking the mode

Your first `kanban_board` read carries `max_workers` and `idle_time` (both from `.kanban/config.json`; absent means
1 worker and a 300-second idle).

- `max_workers` = 1 → **The loop** below: one ticket at a time, worked by you.
- `max_workers` = N > 1 → **The parallel loop** below: up to N tickets in flight at once, each worked by a subagent.
- A ticket-id argument or `--one` caps useful parallelism at 1: use the sequential loop regardless of config.

## The loop

Repeat until the user stops you (or you've done the one requested ticket — a ticket-id argument or `--one` ends the
loop after it):

1. **Pick** — call `kanban_board` (remember the `version`), then `kanban_next`. Its `action` field says what the ticket
   needs: `implement` (a ready ticket — steps 2–8) or `refine` (a stub — see **Refining a stub** below). If nothing is
   eligible, go idle instead of ending the loop — see **Idling** below.
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

## The parallel loop (max_workers > 1)

You become the orchestrator: you own every board mutation, subagents do the work. Keep at most `max_workers`
tickets in flight; a refinement counts as one worker, an implementation counts as one worker.

1. **Pick and claim yourself** — `kanban_board`, then `kanban_next`, then `kanban_claim`, exactly as in the
   sequential loop. Never let subagents race `kanban_next`: claim first, then delegate. (Claims are CAS-guarded by
   `expected_version` and refused when already claimed, so even a race only costs a re-read and retry.)
2. **Delegate** — launch one subagent per claimed ticket via the Agent tool, passing the ticket id, its full `body`
   as the spec, and the action. Launch independent subagents in a single message so they run concurrently. Every
   subagent starts in the **main checkout** — never inside another ticket's worktree.
   - `implement` → the subagent runs `kanban_worktree_start` (tell it to supply a short kebab-case `slug`), `cd`s into
     the reported worktree and stays there, works the spec, commits logical chunks, `kanban_note`s progress, runs
     the tests/build, and calls `kanban_worktree_finish` once everything is committed. It reports back: branch name,
     what landed, and whether verification passed. It does NOT move the card — closing out is yours.
   - `refine` → the subagent researches the codebase (no worktree, no commits, no board writes) and returns the
     fleshed-out spec text, a sharper title if it found one, and any splits. You call `kanban_refine` with what it
     returned — subagents never hold board-version state.
3. **Close out as results arrive** — re-read `kanban_board` for a fresh version, then: reported success →
   `kanban_move` to `done` (with `--push`, push the branch and open the PR first); reported failure or an unusable
   result → `kanban_note` what happened and `kanban_release` the ticket. If the subagent died leaving the worktree
   dirty, leave the worktree for the human — never `force_discard`.
4. **Top up** — after each close-out, pick and claim the next eligible ticket while others are still running.
   Between close-outs, while tickets are in flight and fewer than `max_workers` are running, don't only wait for a
   completion: re-poll the board on a fixed 60-second cadence. Workers are active, so the human is likely at the
   board creating tickets — new work should start promptly, and a re-poll costs one cheap `kanban_board` +
   `kanban_next` read. Wait out each interval the same way **Idling** does (your harness's wait or scheduling
   mechanism; plain Bash `sleep 60` as the fallback). `idle_time` (default 300 s) stays the empty-board cadence:
   waiting with workers in flight is a different situation from a dry board.
   - Each re-poll is the normal pick step: fresh `kanban_board` (new version), `kanban_next`, claim and delegate
     up to the cap, exactly as steps 1–2. Nothing eligible → keep waiting for completions on the same cadence.
   - At capacity (in-flight = `max_workers`), don't re-poll — nothing could be claimed anyway. The next close-out
     frees a slot and resumes the cadence.
   The loop-end condition is unchanged: when `kanban_next` reports nothing eligible AND every in-flight ticket is
   closed out, go idle — see **Idling** below.

The store is safe under concurrency (advisory lock, version CAS, one worktree per ticket, per-ticket branches) —
what needs discipline is the policy above: one claimer, one board-writer, subagents in their own worktrees.

## Idling

Running dry doesn't end the loop: the human keeps feeding the board, so wait and look again. When nothing is
eligible (and, in the parallel loop, nothing is in flight):

1. **Report, briefly** — why the remaining todo tickets don't qualify (draft/review status? blocked? claimed?
   external?) and that you're idling for `idle_time` seconds.
2. **Wait `idle_time` seconds** — the value from your latest `kanban_board` read. Use whatever wait or scheduling
   mechanism your harness provides for sitting out a delay; a plain Bash `sleep <idle_time>` is the fallback when
   nothing better exists.
3. **Re-poll** — a fresh `kanban_board` (which also picks up any config change), then `kanban_next`. Work whatever
   became eligible, or idle again.

Only the user ends an idling loop — by interrupting or saying stop. The exceptions never reach idling at all: a
ticket-id argument or `--one` means one ticket, so finish it, report, and end.

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
