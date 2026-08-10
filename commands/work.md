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

Your first `kanban_board` read carries the effective `max_workers` and `idle_time`, already resolved from
`.kanban/config.json` and its defaults — take both values from the read; never assume them.

- `max_workers` = 1 → **The loop** below: one ticket at a time, worked by you.
- `max_workers` = N > 1 → **The parallel loop** below: up to N tickets in flight at once, each worked by a subagent.
- A ticket-id argument or `--one` caps useful parallelism at 1: use the sequential loop regardless of config.

Orthogonally, a ticket may name the `model` and `effort` its work deserves — see **Model and effort** below. That
decides *how* a ticket is dispatched, not which loop you are in.

## Minesweeper mode

The same `kanban_board` read carries `minesweeper`. When it is `true`, this board delegates implementation to an
external daemon and **you write no code for ready tickets**:

- **Stubs are unchanged.** `action: refine` works exactly as **Refining a stub** below — specs are still yours to
  write, and refining is how a stub becomes delegable in the first place.
- **`action: implement` becomes a handoff.** The claim itself delegates: the binary mirrors the ticket to a labelled
  GitHub issue and re-owns the card to `minesweeper`. After `kanban_claim`, re-read the ticket, confirm the external
  binding appeared (the owner is now `minesweeper`), report the issue URL from the card's latest note, and continue
  the loop. **No worktree, no implementation, no close-out** — steps 3–8, model/effort dispatch, `--push` and
  auto-merge never run for a delegated ticket, and it occupies no worker slot in the parallel loop.
- **If no binding appeared**, the card carries a `kanban` note saying why delegation failed. Report it and move on —
  the ticket stays in doing for the human to sort out, like every other minesweeper failure.
- **Hands off after the handoff.** The serve poller tracks the issue from here: a PR moves the card to review, the
  merge reaching local main lands it. Delegated review tickets are not rework candidates for you — PR feedback is the
  daemon's job, and the board enforces it: `Op::RequestChanges` refuses an external ticket outright, so a delegated
  card can never carry `changes_requested` and `action: "rework"` can never name one. When idling, do mention delegated
  tickets that have sat in `doing` across several polls with no PR and no flag: a daemon paused on API limits is
  invisible to the board, and only the human can go look.

## The loop

Repeat until the user stops you (or you've done the one requested ticket — a ticket-id argument or `--one` ends the
loop after it):

1. **Pick** — call `kanban_board` (remember the `version`), then `kanban_next`. Its `action` field says what the ticket
   needs: `land` (a review ticket a human accepted — see **Landing a branch** below, and do **not** claim it), `implement`
   (a ready ticket — steps 2–8), `refine` (a stub — see **Refining a stub** below), or `rework` (a ticket a human sent
   back with feedback — see **Rework** below). If nothing is
   eligible, go idle instead of ending the loop — see **Idling** below. `kanban_next` first auto-lands any review
   tickets whose branches have reached local main, so **use the `version` it returns** for the claim — the sweep may
   have advanced the board.
2. **Claim** — `kanban_claim` the ticket. A pure board mutation; git is untouched.
   **Then check `model` and `effort` on the ticket.** If either is set, hand the ticket to a subagent instead of working
   steps 3–7 — see **Model and effort** below — then close it out at step 8 as usual. If both are absent (the common
   case), carry straight on.
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
7. **Commit everything** — the worktree is **kept** through review: the human reads the code there, and a rework round
   re-attaches to it instantly. So there is no `kanban_worktree_finish` at close-out — the board removes the worktree
   itself once the ticket lands. What you must do is leave nothing uncommitted: step 8's move refuses while the
   worktree is dirty, and that worktree may sit on volatile /tmp where only commits survive.
8. **Close out** — `kanban_move` the ticket to `review`. Done is not yours to declare: the board lands review tickets
   in `done` automatically once their branch (or PR) is merged into the **local** main branch, and dependencies
   unblock only then. Report the branch name prominently: integrating it is the user's explicit next step. With
   `--push`: `git push -u origin <branch>` and `gh pr create` (title from the ticket, body summarising the work and
   linking the ticket id), then include the PR URL in the report — you don't record the PR on the board, the server's
   poller discovers it by branch. **Then, if `kanban_next` reported `auto_merge: true` for this ticket, land the branch
   yourself — see **Landing a branch** below.** Without that flag the branch waits in review for the user to accept it,
   which is what puts it back in front of this loop as `action: "land"`.

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
     the tests/build, and commits everything. It must NOT call `kanban_worktree_finish` — the worktree is kept through
     review and the board retires it on landing. It reports back: branch name, what landed, and whether verification
     passed. It does NOT move the card — closing out is yours.
   - `refine` → the subagent researches the codebase (no worktree, no commits, no board writes) and returns the
     fleshed-out spec text, a sharper title if it found one, and any splits. You call `kanban_refine` with what it
     returned — a refine subagent makes no board writes.
3. **Close out as results arrive** — re-read `kanban_board` for a fresh version, then: reported success →
   `kanban_move` to `review` (with `--push`, push the branch and open the PR first; the board lands review tickets in
   done itself once the merge reaches local main); reported failure or an unusable result → `kanban_note` what
   happened and `kanban_release` the ticket. If the subagent died leaving the worktree dirty, leave the worktree for
   the human — never `force_discard`. A ticket whose `kanban_next` payload said `auto_merge: true` gets landed here,
   by you, once the move to `review` succeeds — see **Landing a branch** below. Subagents never merge, and neither does
   an `action: "land"` ticket get delegated: landings run in the main checkout, one at a time, in this session.
4. **Top up** — after each close-out, pick and claim the next eligible ticket while others are still running.
   Between close-outs, while tickets are in flight and fewer than `max_workers` are running, don't only wait for a
   completion: re-poll the board on a fixed 60-second cadence. Workers are active, so the human is likely at the
   board creating tickets — new work should start promptly, and a re-poll costs one cheap `kanban_board` +
   `kanban_next` read. Wait out each interval the same way **Idling** does (your harness's wait or scheduling
   mechanism; plain Bash `sleep 60` as the fallback). `idle_time` stays the empty-board cadence: waiting with workers
   in flight is a different situation from a dry board.
   - Each re-poll is the normal pick step: fresh `kanban_board` (new version), `kanban_next`, claim and delegate
     up to the cap, exactly as steps 1–2. Nothing eligible → keep waiting for completions on the same cadence.
   - At capacity (in-flight = `max_workers`), don't re-poll — nothing could be claimed anyway. The next close-out
     frees a slot and resumes the cadence.
   The loop-end condition is unchanged: when `kanban_next` reports nothing eligible AND every in-flight ticket is
   closed out, go idle — see **Idling** below.

The store is safe under concurrency (advisory lock, version CAS, one worktree per ticket, per-ticket branches) —
what needs discipline is the policy above: one claimer, one board-writer, subagents in their own worktrees.

## Model and effort

A ticket can name what its work is worth running at: `model` (one of the board's models — `kanban_board` returns the
effective list as `models` — or any full id) and `effort` (`low` / `medium` / `high` / `xhigh` / `max`). Both are
optional and usually absent — absent means "inherit", i.e. exactly today's behaviour.

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
and leaves everything committed — no `kanban_worktree_finish`, the worktree is kept through review; **you** close the
card out.

**Never silently ignore either field.** If you dispatch a ticket at anything other than what it asked for — a level your
harness rejects, a model that isn't available, a fallback you chose — `kanban_note` what was requested versus what
actually ran, and say so in the end-of-loop summary. A dial that lies about being applied is worse than no dial.

## Landing a branch

Landing is the rebase-and-fast-forward that puts a reviewed branch into the main branch. **One procedure, two ways in:**

- **`action: "land"`** — a human pressed **Accept** on the review card, which does not close it: accepting is
  *permission to land*, and `kanban_next` then hands you the ticket ahead of all other work, with its `branch` beside
  the action. This is the common case.
- **`auto_merge: true`** — standing permission, granted in advance on the ticket or its epic, to land the branch you
  have just finished without a human looking at it. `kanban_next` returns the **effective** answer beside `action` —
  the ticket's own flag OR its epic's. Read it there, not off the ticket: an epic-level grant is invisible on the
  card. Run it **after `kanban_move to=review` succeeds**, and only when the ticket is not `external` and has a
  recorded branch.

Either way the merge lives here rather than in the binary, on purpose. `src/land.rs` only ever *proves* that code
landed, it never causes it; the binary's one path that writes to main (`kanban_worktree_finish merge=true`) is
explicitly human-approved; and resolving a rebase conflict needs judgement that has to sit with an agent reading the
code, not with a store operation or a browser click. That is exactly why **Accept** hands the work to you instead of
doing it itself.

Everything below happens in the **main checkout** — never inside a worktree, and never in parallel with another
landing.

0. **Take the landing** — `kanban_start_landing`. Not `kanban_claim`: claiming a review ticket is the *rework* path and
   would drag the card into `doing`. This marks the card "landing…" for the human without giving it an owner, and it is
   the interlock in the parallel loop — if it refuses because another worker holds it, that ticket is not yours, so
   move on to the next one.
1. **Remove the worktree** — `git worktree list --porcelain`. The worktree is kept through review, so the branch is
   normally still checked out and this step normally runs: `kanban_worktree_finish` (never `force_discard`); if it
   refuses because the tree is dirty, stop. Git will not let you check out a branch that is live in a worktree, so
   skipping this fails the rebase two steps later.
2. **Confirm the main checkout is on main and clean outside the board** — `git branch --show-current` names the
   configured main branch, and `git status --porcelain -- . ':(exclude).kanban'` comes back empty. That exclusion is
   required, not cosmetic: `.kanban/board.json` is tracked and the board is written constantly, so an unqualified
   `git status` is dirty essentially every time you reach this step.
3. **Rebase** — `git checkout <branch>`, then `git rebase --autostash <main>`. Resolve conflicts **only** where the
   intent is unambiguous. Anything you would have to guess at is a failure, not a judgement call.
   `--autostash` is not optional here, and it is the other half of step 2's exclusion: `git checkout` tolerates a dirty
   `board.json` but `git rebase` flatly refuses to start with *any* unstaged change, so without it the rebase dies on
   the board write you just made. The stash pops cleanly because no ticket branch commits anything under `.kanban/`,
   and `git rebase --abort` restores it too — so the failure path leaves the board file exactly as it found it.
4. **Fast-forward** — `git checkout <main>`, then `git merge --ff-only <branch>`. Never `--no-ff`, never `--force`.
5. **Let the board land it, and only then delete the branch** — call `kanban_next` (its landing sweep runs first),
   confirm the ticket reached `done`, and *after* that `git branch -d <branch>`. This ordering is load-bearing; the
   next paragraph says why. The landing marker retires with the card, so a success needs no cleanup call.
6. **Note what happened** — `kanban_note` on the ticket: what merged into main, and, if step 3 resolved conflicts,
   exactly which files conflicted and how you resolved each one. A silently resolved rebase conflict is the worst
   possible outcome of this feature, and the note is the only thing that makes it reviewable afterwards.

**Why step 5 deletes the branch last.** While the branch still exists, `land::sweep` proves the landing by its
strongest rule: the branch tip is an ancestor of main (`git merge-base --is-ancestor`), which needs nothing but the
repo in front of it. Delete the branch first and that rule is simply unavailable — the sweep falls back to the tip
recorded in `.kanban/land-state.json`, which the move into `review` takes for you, so the fallback is armed rather
than hypothetical. It is still the weaker proof: machine-local, by patch-id, and losable to a gc. A landing should
never have to depend on that sidecar file when keeping the branch a few seconds longer makes rule 1 answer.

**When it doesn't work.** Every failure ends the same way: **the ticket stays in `review`, `kanban_block_landing`
names the failure on the card, and the loop moves on to the next ticket.** That tool flags the card red for the human
and takes it out of the landing queue until they accept it again — so it is the end of your involvement with that
ticket, not something to retry. Never discard it, never drag it to `done`, never reach for `--force` or `--no-ff` to
make a merge go through.

| Situation | What to do |
|---|---|
| Worktree dirty, so `kanban_worktree_finish` refuses | Stop before touching git. `kanban_block_landing` with the worktree path so the human can finish it. (A worktree merely being *present* is normal — step 1 removes it.) |
| Rebase conflict you cannot resolve confidently | `git rebase --abort` **first** — never leave a half-rebase behind — then `kanban_block_landing` naming which paths conflicted. |
| `git checkout <branch>` refuses over `.kanban/board.json` | The project commits its board and main has moved it since the branch point. Don't force it: `kanban_block_landing` saying so. Committing the board on main first is the human's call. |
| `git merge --ff-only` refuses (main moved under you) | Retry steps 3–4 exactly once. Still refusing means main moved twice during one landing: `kanban_block_landing`. |
| Branch is already an ancestor of main | Benign — it was merged already. Skip to step 5 and let the sweep land it by ancestry. |
| Branch no longer exists | Leave it alone: the sweep's observed-tip path may still land it, and otherwise the existing "branch gone" flag is the right outcome. |
| No branch recorded on the ticket | Nothing to merge. For a companion subtask this means its close-out omitted `kanban_move branch=…`; its parent's branch may well land it anyway. |
| The ticket is `external` | Never landed from here, whatever the flags say. Its branch was never a local ref — the same principle that stops the sweep landing external tickets from local branch state. |

If the ticket has an open PR, the local merge still lands the card (by ancestry) and leaves the PR open on GitHub —
nothing here closes it. Say so in the end-of-loop report so the user knows a PR is now stale.

## Idling

Running dry doesn't end the loop: the human keeps feeding the board, so wait and look again. When nothing is
eligible (and, in the parallel loop, nothing is in flight):

1. **Report, briefly** — `kanban_next`'s answer carries `waiting` when it has no ticket for you: `waiting.todo` says
   why each todo ticket doesn't qualify (draft/review status, blocked by which tickets, claimed, external) and
   `waiting.review` says why each review ticket couldn't be proven landed. **Report what it says, don't invent it** —
   and say you're idling for `idle_time` seconds. A `not_shown` count means the list was capped; say so too.
   `waiting.review` is the one that needs the human's eye. A ticket whose branch is gone with nothing observed, or
   whose PR was closed unmerged, will never land on its own — its dependents stay blocked until somebody deletes the
   branch, drags the card to done, or discards it. An idling loop that stays quiet about that is the failure this
   field exists to prevent, so name those tickets and what they're waiting for rather than reporting "nothing ready".
2. **Wait `idle_time` seconds** — the value from your latest `kanban_board` read. Use whatever wait or scheduling
   mechanism your harness provides for sitting out a delay; a plain Bash `sleep <idle_time>` is the fallback when
   nothing better exists.
3. **Re-poll** — a fresh `kanban_board` (which also picks up any config change), then `kanban_next`. Work whatever
   became eligible, or idle again.

Only the user ends an idling loop — by interrupting or saying stop. The exceptions never reach idling at all: a
ticket-id argument or `--one` means one ticket, so finish it, report, and end.

## Rework (`action: "rework"`)

A ticket in `review` is code-complete but unlanded, and the human reviewing it can press **Request changes** instead of
accepting. That sends the card back to the top of `doing`, unclaimed, with its branch and worktree untouched — and
`kanban_next` then hands it to you as `action: "rework"`, **ahead of any todo work**. You don't wait to be asked: the
dispatch is the ask. Somebody is blocked on near-finished code, and its worktree already exists.

1. `kanban_claim` the ticket. The card is owned by the reviewer but worked by nobody, so the claim simply re-owns it to
   you — this is the one `doing` state any agent may claim.
2. `kanban_worktree_start` — it re-attaches to the existing `k-<n>/…` branch and the worktree that was kept through
   review; your previous commits are all there.
3. **The newest `changes requested:` note is the spec for this round.** Read it, address exactly it, commit — and if the
   ticket has an open PR, push the branch so the PR updates.
4. `kanban_move` back to `review`, which clears the flag. Leave the worktree in place, exactly as at any other
   close-out. The board takes it from there.

The same path still serves the older case — a review ticket the *user* asks you to rework, with no flag set. Claim it
and follow steps 2-4 identically.

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
- `auto_merge` and **Accept** are both the human's permission to move their integration branch, and there is no undo
  once main has moved. Never set `auto_merge` on tickets you create unless the user explicitly asked for it, and never
  land a branch that carries neither — an unaccepted, unflagged branch is reported and left in review for them.
- Never press a review verdict for the human. Accept, Request changes and Discard are theirs, in the browser; your part
  begins after they have accepted, when `kanban_next` hands you the landing.
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
  `kanban_worktree_finish`, and move to the next ticket. A ticket released mid-rework keeps its `changes_requested`
  flag on purpose — the feedback did not stop existing because you handed the card back.
- **Never press a human's verdict for them.** Accept, Request changes and Discard are the reviewer's, made in the
  browser. You address feedback; you never decide that it was addressed well enough.
- At the end of the loop, summarise: tickets completed, branches created (and PRs, with `--push`), tickets released
  or split, and what the board looks like now.
