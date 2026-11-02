---
description: "Work the board: claim the next eligible ticket — implement it (ready) or flesh out its spec (stub) — move it across, repeat; when the board runs dry, idle and re-poll. Running this is the opt-in — never claim tickets outside a loop the user started."
argument-hint: "[ticket-id] [--one] [--push]"
---

# /kanban:work — the policy loop

Starting this command is your authorisation to claim tickets one after the next; outside a running loop you never claim
spontaneously.

Arguments: `$ARGUMENTS` — a ticket id works exactly that ticket (must be ready/stub, in todo, unblocked); `--one` stops
after one; `--push` pushes each finished branch and opens a PR with `gh` — without it nothing leaves the machine.

## Picking the mode

Your first `kanban_board` read carries the effective `max_workers` and `idle_time` — take both from the read, never
assume them.
- `max_workers` = 1 → **The loop** below, one ticket at a time, worked by you.
- `max_workers` = N > 1 → **The parallel loop** below, up to N tickets in flight, each worked by a subagent.
- A ticket-id argument or `--one` caps parallelism at 1: use the sequential loop regardless.

A ticket's `model`/`effort` decides *how* a ticket is dispatched, not which loop — see **Model and effort**.

## Minesweeper mode

When the read's `minesweeper` is `true`, this board delegates implementation to an external daemon and **you write no
code for ready tickets**:
- **Stubs are unchanged** — `refine` works exactly as **Refining a stub** below.
- **`implement` becomes a handoff.** The claim itself delegates: the binary mirrors the ticket to a labelled GitHub
  issue and re-owns the card to `minesweeper`. After `kanban_claim`, re-read the ticket, confirm the binding (owner now
  `minesweeper`), report the issue URL from the latest note, and continue. **No worktree, no implementation, no
  close-out** — `--push`/auto-merge never run for a delegated ticket, and it occupies no worker slot.
- **No binding appeared?** The card's note says why delegation failed. Report it and move on — it stays in `doing` for
  the user.
- **Hands off after the handoff.** The serve poller tracks the issue from here (PR → review, merge → done). Delegated
  review tickets are never rework candidates — `RequestChanges` refuses an external ticket outright. When idling,
  mention delegated tickets stuck in `doing` across polls with no PR — only the user can look.

## The loop

Repeat until the user stops you (a ticket id or `--one` ends it):

1. **Pick** — `kanban_board` (note the `version`), then `kanban_next`. Its `action` names what the ticket needs: `land`
   (accepted review ticket — **Landing a branch**, and don't claim it), `implement` (steps 2–8), `refine` (**Refining a
   stub**), `rework` (**Rework**). Nothing eligible → idle (**Idling**). `kanban_next`'s sweep may have landed tickets, so
   **use the `version` it returns** for the claim.
2. **Claim** — `kanban_claim`; a pure board mutation. **Then resolve model and effort** — the ticket's own `model`, else
   the board's `implement_model` (for `implement`/`rework`), else nothing. If one resolves, hand the ticket to a subagent
   (see **Model and effort**) and close out at step 8; only when neither resolves do you carry on and work it yourself.
3. **Start** — `kanban_worktree_start` with your own `slug`: a short kebab-case digest of the title (e.g. "Add
   authorization based on OAuth from Google" → `google-oauth`).
4. **Work** — `cd` into the reported worktree path and stay there for the ticket's lifetime. The ticket's `body` is the
   spec. Commit after each logical chunk — the worktree may be on volatile /tmp, and commits are what survive. Subtasks
   that emerge, two kinds:
   - **Companion** (extra work now, on this ticket's session): create WITHOUT `depends_on` this one, claim it, work it
     **in this same worktree on this same branch** (never a worktree from inside a worktree), close out with
     `kanban_move to=review branch=<this branch>` — that `branch` is what lets the board land it.
   - **Deferred follow-up** (real future work): create WITH `depends_on` this ticket, leave it in todo — it stays
     blocked until this code lands in main. Don't work it now.
5. **Note** — `kanban_note` progress at meaningful moments; the user watches them appear live.
6. **Verify** — run the project's tests/build before calling anything done. A failing ticket is not done: note the
   failure, then fix it or release it with a note.
7. **Commit everything** — the worktree is **kept** through review (the user reads the code there, rework re-attaches),
   so there is **no `kanban_worktree_finish` at close-out**. But step 8 refuses a dirty worktree, and the tree may sit on
   volatile /tmp, so leave nothing uncommitted.
8. **Close out** — `kanban_move` to `review`. Done is not yours to declare: the board lands it once the branch/PR merges
   into **local** main, and dependencies unblock only then. Report the branch name prominently. With `--push`: `git push
   -u origin <branch>` and `gh pr create` (title from the ticket, body summarising and linking the id); don't record the
   PR — the poller discovers it by branch. Then, if `kanban_next` said `auto_merge: true`, land the branch yourself
   (**Landing a branch**); otherwise it waits in review for the user.

## The parallel loop (max_workers > 1)

You become the orchestrator: you own every board mutation, subagents do the work. A refinement counts as one worker, an
implementation as one.

1. **Pick and claim yourself** — `kanban_board`, `kanban_next`, `kanban_claim`, as above. Never let subagents race
   `kanban_next`: claim first, then delegate (claims are CAS-guarded; a race costs only a re-read and retry).
2. **Delegate** — one subagent per claimed ticket (Agent tool), passing the ticket id, its full `body` as spec, and the
   action; pick the subagent type from `effort` and the model from the ticket's own `model` else the role default
   (**Model and effort**). Launch independent ones in a single message. Every subagent starts in the **main checkout**.
   - `implement` → subagent runs `kanban_worktree_start` (short kebab-case `slug`), works in that worktree, commits
     chunks, notes progress, verifies, commits everything. No `kanban_worktree_finish`. Reports branch, what landed,
     whether verification passed; does **not** move the card.
   - `refine` → subagent researches (no worktree, no board writes) and returns the spec text, a sharper title, and any
     splits. You call `kanban_refine`.
3. **Close out as results arrive** — re-read `kanban_board` for a fresh version: success → `kanban_move` to `review`
   (with `--push`, push + PR first); failure or unusable result → `kanban_note` and `kanban_release`. A dead subagent
   leaving the worktree dirty → leave it for the user, never `force_discard`. An `auto_merge` ticket is landed by you
   after the move to `review`. Landings run in the main checkout, one at a time, in this session; subagents never merge,
   and `action: "land"` is never delegated.
4. **Top up** — after each close-out, claim the next eligible ticket while others run. Between close-outs with in-flight
   workers, re-poll on a fixed 60-second cadence (`kanban_board` + `kanban_next`, claim and delegate up to the cap). At
   capacity, don't re-poll. Loop-end: nothing eligible AND nothing in flight → idle.

The store is safe under concurrency (lock, CAS, per-ticket worktrees); the discipline is policy: one claimer, one
board-writer, subagents in their own worktrees.

## Model and effort

A ticket may name `model` (a board model or any full id) and `effort` (`low`/`medium`/`high`/`xhigh`/`max`); absent
means inherit. The read also carries `implement_model` and `refine_model` — role defaults for tickets that name nothing,
split by action (`implement`/`rework` vs `refine`); either may be `null`. **A ticket's own `model` always wins over the
role default**; `effort` only ever comes off the card.

Effective model: **its own `model`, else the role default for its action, else inherit.** Landing is never affected.

You can't change your own model mid-session, so the only way to honour either is to dispatch to a subagent. Resolve the
model first, then read `effort`:

| `effort` | effective `model` | Dispatch |
|----------|-------------------|----------|
| absent | none | Work it yourself (sequential) or the default subagent (parallel). |
| absent | resolved | Agent tool with `model: <the effective model>`. |
| set | none | Agent tool with `subagent_type: "kanban-effort-<level>"`. |
| set | resolved | Both. |

Consequence: with an `implement_model` set, even a ticket naming nothing is delegated — the sequential loop loses
in-session visibility, which is exactly the trade a per-ticket `model` already makes. The `kanban-effort-*` agents ship
with this plugin (one per level, `effort` in frontmatter, `model: inherit` so your `model` override wins).

Delegation is otherwise as in the parallel loop: subagent works its own worktree, commits, notes, verifies, leaves
everything committed, no `kanban_worktree_finish`; **you** close the card out.

**Never silently ignore either field.** If you dispatch at anything other than what was asked (a rejected level, an
unavailable model, a fallback), `kanban_note` requested vs actual and say so in the summary — including role defaults
the board couldn't get.

## Landing a branch

The rebase-and-fast-forward that puts a reviewed branch into main. **One procedure, two ways in:**
- **`action: "land"`** — the user pressed **Accept** (permission to land, not a close); `kanban_next` hands it to you
  ahead of all other work, with its `branch`.
- **`auto_merge: true`** — standing permission on the ticket or its epic, read from `kanban_next` (the **effective**
  answer — the ticket's flag OR its epic's), run **after `kanban_move to=review` succeeds**, only when the ticket is not
  `external` and has a branch.

The merge lives here, not in the binary: `src/land.rs` only ever *proves* code landed, and resolving a rebase conflict
needs an agent reading the code. Everything below runs in the **main checkout** — never in a worktree, never parallel
to another landing.

0. **Take the landing** — `kanban_start_landing`, not `kanban_claim` (that's the rework path). Refused because another
   worker holds it → that ticket isn't yours; move on.
1. **Remove the worktree** — `git worktree list --porcelain`, then `kanban_worktree_finish` (never `force_discard`); if
   it refuses (dirty), stop. Git won't check out a branch that's live in a worktree.
2. **Confirm the main checkout** — `git branch --show-current` is the configured main, and `git status --porcelain -- .
   ':(exclude).kanban'` is empty. The exclusion is required: `board.json` is tracked and written constantly.
3. **Rebase** — `git checkout <branch>`; `git rebase --autostash <main>`. `--autostash` is not optional — the rebase
   flatly refuses any unstaged change. Resolve conflicts only where intent is unambiguous; anything you'd guess at is a
   failure.
4. **Fast-forward** — `git checkout <main>`; `git merge --ff-only <branch>`. Never `--no-ff`, never `--force`.
5. **Let the board land it, then delete the branch** — call `kanban_next` (its landing sweep runs first), confirm the
   ticket reached `done`, *then* `git branch -d <branch>`. This ordering is load-bearing: while the branch exists the
   sweep proves the landing by ancestry; deleted, it falls back to a weaker machine-local proof.
6. **Note** — `kanban_note`: what merged into main, and, if step 3 resolved conflicts, exactly which files and how each
   was resolved. A silently-resolved conflict is the worst possible outcome of this feature.

**When it doesn't work** — every failure ends the same way: the ticket stays in `review`, `kanban_block_landing` names
the failure, and the loop moves on. Never discard it, never drag it to `done`, never `--force`/`--no-ff` to force a
merge.
- Worktree dirty (so `kanban_worktree_finish` refuses) → stop before touching git; `kanban_block_landing` with the path.
- Rebase conflict you can't resolve → `git rebase --abort` **first**, then `kanban_block_landing` naming the paths.
- `checkout` refuses over `board.json` → main moved; `kanban_block_landing` saying so; committing board on main is the
  user's call.
- `--ff-only` refuses (main moved under you) → retry steps 3–4 once; still refusing → `kanban_block_landing`.
- Branch already an ancestor of main → benign; skip to step 5.
- Branch gone → leave it; the observed-tip path may still land it.
- No branch recorded → nothing to merge (a companion's close-out omitted `branch=…`); its parent's branch may land it.
- External → never landed from here.

A ticket with an open PR: the local merge lands the card but leaves the PR open — say so in the report.

## Idling

Running dry doesn't end the loop; the user keeps feeding the board, so wait and look again.

1. **Report briefly** — `kanban_next`'s `waiting` says why each todo ticket doesn't qualify and why each review ticket
   couldn't be proven landed. Report it, don't invent it. Name tickets whose branch is gone or PR closed unmerged — they
   never land on their own and keep dependents blocked.
2. **Wait `idle_time`** seconds — the value from your latest read; harness wait, or `sleep <idle_time>`.
3. **Re-poll** — fresh `kanban_board` (picks up config changes) then `kanban_next`; work what became eligible, else idle
   again.

Only the user ends an idling loop (a ticket id or `--one` never idles — finish, report, end).

## Rework (`action: "rework"`)

A user pressed **Request changes**: the card sits at the top of `doing`, unclaimed, branch and worktree intact.
`kanban_next` hands it to you **ahead of any todo work** — the dispatch is the ask.

1. `kanban_claim` — re-owns the card to you; this is the one `doing` state any agent may claim. Resolve model as in step
   2: the ticket's own `model`, else `implement_model`.
2. `kanban_worktree_start` — re-attaches the kept `k-<n>/…` branch and worktree; your previous commits are all there.
3. **The newest `changes requested:` note is this round's spec.** Address exactly it, commit; with an open PR, push so it
   updates. The review log holds the same feedback structured — read it when a bullet needs context.
4. `kanban_move` back to `review` (clears the flag). Worktree stays, as at any close-out.

The same path serves the older case — a review ticket the user asks you to rework with no flag: claim, steps 2–4.

## Refining a stub

A stub is a spec to write, not code to build. On `action: "refine"`:

1. `kanban_claim` — the card sits pink in `doing` while you write.
2. **No worktree.** Stay in the main checkout; touch nothing.
3. Research until you can write a precise, implementable spec (what, where, how to verify). **Check the effective model
   first** — the stub's own `model`, else `refine_model`. If one resolves, dispatch research to a subagent exactly as
   **Model and effort** describes (it returns the spec text, a sharper title, any splits; no board writes, no worktree);
   you make the `kanban_refine` call.
4. `kanban_refine` with the fleshed-out `body` (and a sharper `title` if found); a stub that's several units of work
   passes `split_tickets`/`split_epics` — atomic. The body **replaces** the stub's text, so write a spec that stands
   alone; don't reference wording this call deletes.
5. Continue the loop. Don't implement what you just specced — the user vets `review` tickets and promotes to `ready`.

## Rules

- Only `ready` (implement) or `stub` (refine), unblocked, unclaimed, non-external. Never touch `draft` tickets.
- `model`/`effort`/`implement_model`/`refine_model` are the user's instruction — honour them or report you couldn't;
  never substitute your own judgement, and never set these fields on tickets you create unless asked.
- `auto_merge` and **Accept** are the user's permission to move their integration branch — no undo once main has moved.
  Never set `auto_merge` unless explicitly asked; never land a branch carrying neither (report and leave in review).
- Never press a review verdict for the user — Accept, Request changes, Discard are theirs, in the browser. Your part
  begins after acceptance, at `action: "land"`.
- Every mutating kanban tool needs `expected_version` from your latest read (or `kanban_next`'s); on conflict, re-read
  and retry.
- Never move a ticket to `done` yourself, never discard one.
- A ticket much bigger than its spec: `kanban_note` the discovery, create follow-ups (they land in `review`), finish the
  original at honest scope.
- A real ordering constraint discovered mid-flight → `kanban_update_ticket` `depends_on` — that's what actually gates
  `kanban_next`; a note is prose nobody's scheduler reads. It replaces the whole list, so read first. Dangling ids/cycles
  refused; drafts off-limits.
- Genuinely stuck: `kanban_note` why, `kanban_release` (returns to top of todo), clean up with `kanban_worktree_finish`,
  move on. A ticket released mid-rework keeps its `changes_requested` flag on purpose.
- **Write for a reader who never saw the previous version.** Never explain code by what it replaced ("moved out of the
  filter cluster") — it stops existing the moment the change lands, leaving a reference nobody can resolve. Say what it
  does and why; git history records what it was. The exception is a still-live constraint (migration path, compat shim,
  format still in the wild).
- At the end, summarise: tickets completed, branches (and PRs with `--push`), released or split, and the board's state.
