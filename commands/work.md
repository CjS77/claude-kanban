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

Orthogonally, a ticket may name the `model` and `effort` its work deserves — see **Model and effort** below. That
decides *how* a ticket is dispatched, not which loop you are in.

## The loop

Repeat until the user stops you (or you've done the one requested ticket — a ticket-id argument or `--one` ends the
loop after it):

1. **Pick** — call `kanban_board` (remember the `version`), then `kanban_next`. Its `action` field says what the ticket
   needs: `implement` (a ready ticket — steps 2–8) or `refine` (a stub — see **Refining a stub** below). If nothing is
   eligible, go idle instead of ending the loop — see **Idling** below. `kanban_next` first auto-lands any review
   tickets whose branches have reached local main, so **use the `version` it returns** for the claim — the sweep may
   have advanced the board.
2. **Claim** — `kanban_claim` the ticket. A pure board mutation; git is untouched.
   **Then check `model` and `effort` on the ticket.** If either is set, you cannot honour it yourself — you can't change
   your own model or effort mid-session — so hand the ticket to a subagent instead of working steps 3–7: see **Model and
   effort** below, then close it out at step 8 as usual. If both are absent (the common case), carry straight on.
3. **Start** — `kanban_worktree_start`. Supply a `slug` yourself: a short kebab-case digest of the title
   (2–3 words, e.g. "Add authorization based on OAuth from Google" → `google-oauth`) beats the mechanical default.
4. **Work** — `cd` into the reported worktree path and stay there for the ticket's lifetime. Read the ticket's `body` as
   the spec. Commit after each logical chunk — the worktree may live on volatile /tmp, and commits are what survive; but
   don't spam micro-commits. Subtasks that emerge mid-ticket come in two kinds — never confuse them:
   - **Companion** (extra work you'll do *now*, as part of this ticket's session): create the ticket WITHOUT
     `depends_on` this one (claiming a blocked ticket is refused, and the work rides this same branch anyway), claim
     it, work it **in this same worktree on this same branch** — never create a worktree from inside a worktree —
     and close it out with `kanban_move to=review branch=<this branch>`. The `branch` argument is what lets the board
     land it: a companion never gets its own worktree, so nothing else records where its code lives.
   - **Deferred follow-up** (real future work): create it WITH `depends_on` this ticket and leave it in todo. It stays
     blocked until this ticket's code actually lands in main; only then does a fresh worktree off main contain what it
     needs. Don't work it now.
5. **Note** — `kanban_note` progress at meaningful moments: what landed, what's left, anything surprising. The human
   watches these appear live on the card.
6. **Verify** — run the project's tests/build before calling anything done. A ticket whose tests fail is not done:
   note the failure and either fix it or release the ticket with a note explaining the blocker.
7. **Finish** — `kanban_worktree_finish` (it refuses if you left uncommitted changes — commit them first; never
   `force_discard` without explicit human approval). Never pass `merge` unless the user asked for it.
8. **Close out** — `kanban_move` the ticket to `review`. Done is not yours to declare: the board lands review tickets
   in `done` automatically once their branch (or PR) is merged into the **local** main branch, and dependencies
   unblock only then. Report the branch name prominently: integrating it is the user's explicit next step. With
   `--push`: `git push -u origin <branch>` and `gh pr create` (title from the ticket, body summarising the work and
   linking the ticket id), then include the PR URL in the report — you don't record the PR on the board, the server's
   poller discovers it by branch.

## The parallel loop (max_workers > 1)

You become the orchestrator: you own every board mutation, subagents do the work. Keep at most `max_workers`
tickets in flight; a refinement counts as one worker, an implementation counts as one worker.

1. **Pick and claim yourself** — `kanban_board`, then `kanban_next`, then `kanban_claim`, exactly as in the
   sequential loop. Never let subagents race `kanban_next`: claim first, then delegate. (Claims are CAS-guarded by
   `expected_version` and refused when already claimed, so even a race only costs a re-read and retry.)
2. **Delegate** — launch one subagent per claimed ticket via the Agent tool, passing the ticket id, its full `body`
   as the spec, and the action. Pick the subagent type and model from the ticket's `effort` and `model` — see **Model
   and effort** below. Launch independent subagents in a single message so they run concurrently. Every
   subagent starts in the **main checkout** — never inside another ticket's worktree.
   - `implement` → the subagent runs `kanban_worktree_start` (tell it to supply a short kebab-case `slug`), `cd`s into
     the reported worktree and stays there, works the spec, commits logical chunks, `kanban_note`s progress, runs
     the tests/build, and calls `kanban_worktree_finish` once everything is committed. It reports back: branch name,
     what landed, and whether verification passed. It does NOT move the card — closing out is yours.
   - `refine` → the subagent researches the codebase (no worktree, no commits, no board writes) and returns the
     fleshed-out spec text, a sharper title if it found one, and any splits. You call `kanban_refine` with what it
     returned — subagents never hold board-version state.
3. **Close out as results arrive** — re-read `kanban_board` for a fresh version, then: reported success →
   `kanban_move` to `review` (with `--push`, push the branch and open the PR first; the board lands review tickets in
   done itself once the merge reaches local main); reported failure or an unusable result → `kanban_note` what
   happened and `kanban_release` the ticket. If the subagent died leaving the worktree dirty, leave the worktree for
   the human — never `force_discard`.
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

## Model and effort

A ticket can name what its work is worth running at: `model` (an alias like `opus`, or a full id like
`claude-opus-4-8`) and `effort` (`low` / `medium` / `high` / `xhigh` / `max`). Both are optional and usually absent —
absent means "inherit", i.e. exactly today's behaviour.

You cannot change your own model or effort mid-session, so the only way to honour either is to dispatch the ticket to a
subagent. Read both fields off the ticket and pick:

| `effort` | `model` | Dispatch |
|----------|---------|----------|
| absent | absent | Work it yourself (sequential loop), or the default subagent (parallel loop). Nothing changes. |
| absent | set | Agent tool with `model: <the ticket's>`. |
| set | absent | Agent tool with `subagent_type: "kanban-effort-<level>"`. |
| set | set | Agent tool with `subagent_type: "kanban-effort-<level>"` **and** `model: <the ticket's>`. |

The `kanban-effort-*` agents ship with this plugin, one per level, each carrying its `effort:` in frontmatter — that
frontmatter is the only place effort can be set, since the Agent tool takes no effort parameter. They declare
`model: inherit` so your per-call `model` override wins; passing no `model` leaves the subagent on the session's.

Everything else about delegating is unchanged from the parallel loop: pass the ticket id, its full `body` as the spec,
and the action; the subagent runs `kanban_worktree_start`, works in its own worktree, commits, notes progress, verifies,
and calls `kanban_worktree_finish`; **you** close the card out.

**Never silently ignore either field.** If you dispatch a ticket at anything other than what it asked for — a level your
harness rejects, a model that isn't available, a fallback you chose — `kanban_note` what was requested versus what
actually ran, and say so in the end-of-loop summary. A dial that lies about being applied is worse than no dial.

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

## Rework (a review ticket got feedback)

A ticket in `review` is code-complete but unlanded — PR feedback or human review can send it back. Only do this when
the user asks for the rework (or the ticket's notes clearly request it):

1. `kanban_claim` the ticket — review tickets are claimable; the claim keeps the recorded branch.
2. `kanban_worktree_start` — it re-attaches to the existing `k-<n>/…` branch idempotently; your previous commits are
   all there.
3. Address the feedback, commit, and — if the ticket has an open PR — push the branch so the PR updates.
4. `kanban_worktree_finish`, then `kanban_move` back to `review`. The board takes it from there.

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
- A ticket's `model`/`effort` is the human's instruction, not a suggestion to weigh. Honour it or report that you
  couldn't — never substitute your own judgement about what a ticket deserves, and never set these fields on tickets
  you create unless the user asked for them.
- Every mutating kanban tool needs `expected_version` from your latest `kanban_board` read (or the `version`
  `kanban_next` returns — its landing sweep may have advanced the board). On a version conflict, re-read the board
  and retry the operation against the new state.
- Never move a ticket to `done` yourself, and never discard one — landing is the board's job (it needs proof the code
  reached local main) and discarding is the human's.
- If a ticket turns out to be much bigger than its spec, don't silently balloon: `kanban_note` the discovery, create
  follow-up tickets with `kanban_create_ticket` (they land in `review` for the human to vet), and finish the
  original at its honest scope.
- If you discover a real ordering constraint mid-flight — this ticket can't land before another, or a dependency it was
  given turns out not to hold — put it on the board with `kanban_update_ticket`, not just in a note: `depends_on` is what
  actually gates `kanban_next`, a note is prose nobody's scheduler reads. It replaces the whole list, so read the ticket
  first and send the set you want. Dangling ids and cycles are refused, and drafts are off-limits.
- If genuinely stuck, `kanban_note` why, `kanban_release` the ticket (it returns to the top of todo), clean up with
  `kanban_worktree_finish`, and move to the next ticket.
- At the end of the loop, summarise: tickets completed, branches created (and PRs, with `--push`), tickets released
  or split, and what the board looks like now.
