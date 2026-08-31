# Workflow

## The board

Four columns. A ticket's **column is its workflow state** and its **position in that column is its priority** — top
of the column is next up. Dragging a card to the top is how you tell Claude what to do next.

| Column   | Meaning                                                                                                                                                  |
|----------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| `todo`   | Ready to be worked. Claude takes the highest unblocked ticket.                                                                                           |
| `doing`  | Claimed and in progress.                                                                                                                                 |
| `review` | Code-complete but not landed: the branch or PR is waiting to reach the local main branch. **The worktree is kept**, so you can read the code on disk while you review it, and sending the ticket back for changes resumes instantly instead of rebuilding a checkout. The board removes the worktree when the ticket lands (or is discarded) — a worktree with uncommitted changes is kept, with a note on the card saying where it is. For an external ticket there was never a local worktree, so none of this applies. |
| `done`   | Landed in the **local** main branch — or explicitly discarded (`discarded: true`).                                                                       |

**Done means landed.** The board itself moves review tickets to done — automatically, and only on positive proof:
the branch tip (or the PR's merge commit) is an ancestor of local main, or a deleted branch's last-observed tip
proves patch-equivalent (`git cherry` — a rebase-then-fast-forward flow keeps patch-ids). Merged on `origin/main`
is *not* done; the merge has to arrive locally. No proof → the card stays in review, flagged for the human.
Discarding — closing work that will never land — is always an explicit human action (the Discard button), never
inferred.

## Statuses

Every ticket and epic also carries a **`status`** field saying how well-defined it is. This is orthogonal to the
column: the column is where the work sits in the workflow, `status` is whether the work is defined enough to do
at all.

| `status` | Meaning                                                                                                                                                                                                                                           |
|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `draft`  | Still being defined by the user. Ignored from a work point of view — Claude neither picks it up nor touches it.                                                                                                                                   |
| `stub`   | A rough outline the user wants fleshed out. Claude expands it into a detailed description (a planning-mode pass), splitting a ticket into subtasks — or an epic into sub-epics — if it turns out to be too much work for one unit.                |
| `review` | Fleshed out and awaiting the user's verdict. When Claude expands a stub, that ticket or epic — and everything newly created by the split — becomes `review`. The user either pushes it back to `stub` for another pass or promotes it to `ready`. |
| `ready`  | Fully specified and ready to be picked up by the LLM harness and implemented.                                                                                                                                                                     |

## The loop

1. **Write tickets** on the board — or drop one-line ideas as `stub`s for Claude to flesh out into specs.
2. **Prioritise by dragging.** Column is workflow state (`todo` / `doing` / `review` / `done`); position in the
   column is priority. A ticket's `status` says how well-defined it is: `draft` (yours, untouchable) → `stub`
   (flesh me out) → `review` (vet the spec) → `ready` (implementable). Promoting to `ready` is your call, made on
   the card. A card can also name the **model** and **effort** its work deserves; leave them blank and it inherits
   the board's `implement_model` / `refine_model` role default for that kind of work, or failing that
   whatever the worker session is running.
3. **Run `/kanban:work`** in Claude Code. Claude claims the top eligible ticket, works it in its own worktree on
   its own branch, notes progress on the card, and moves it to `review` — code-complete, waiting to land — then
   takes the next. When the board runs dry the loop doesn't exit: it sleeps and polls again, so you can keep
   dropping tickets while it runs — interrupt it to stop. Nothing moves your main branch unless you ask: integrating
   is your step — press **Accept** on the review card and the loop lands it for you, merge it by hand, or click
   **Create PR** on the detail pane to push the branch and open a GitHub PR via `gh`. The ticket's worktree stays on disk for as long as the card sits in
   `review`, so `merge.sh` (and the loop's auto-merge) removes it as their first step — and refuses if you have
   uncommitted work in there. The cost is disk: worktrees now live for the whole review period rather than seconds,
   under `/tmp/claude-kanban` by default.
4. **Done happens by itself.** Done means *landed in your local main branch*: the board watches review tickets
   and moves each to `done` the moment its branch — or its PR's merge commit, once you pull — reaches local main,
   with a note saying why. A PR merged only on GitHub shows "PR merged — pull main" until the merge arrives
   locally. Work that will never land is retired with the card's **Discard** button; a discarded ticket closes but
   keeps its dependents blocked.
5. **Or `/kanban:delegate`** a ticket to an external worker: it's mirrored to a GitHub issue and the board tracks
   it as worked elsewhere; once its PR opens, move the card to `review` with the PR's head branch and the board
   lands it like any other.

## Reviewing a ticket

A card in `review` carries a **Review** tab beside its details, with the branch, the worktree still on disk, the
progress log, a comment box, and three verdicts:

- **Accept** — clears the work to land. It doesn't close the card: it marks the ticket `accepted`, and a running
  `/kanban:work` loop then picks it up ahead of all other work, rebases its branch onto your main branch and
  fast-forwards main into it. The card moves to `done` the moment the board can prove the code arrived — the same
  proof every other landing needs. So Accept is your permission for main to move, and nothing unblocks until it
  actually has. The card shows **✓ accepted** while it waits, and **⟳ … is landing this** while a loop has it.

  If the rebase hits a conflict the agent can't resolve confidently, it aborts — main and the branch untouched — and
  hands the ticket back: the card stays in review, shaded red with a **⚠ landing blocked** badge, and the newest
  progress note says which files collided. Resolve it on the branch yourself and press **Accept again** (the button
  says so), or send it back with **Request changes**. Until you do one of those, no loop will retry it.

  Two things worth knowing. Accept does nothing on its own if no work loop is running — the ticket just waits, which
  is what `.kanban/merge.sh <branch>` is still there for. And the pane shows the lander's own verdict — *"branch
  k-7/foo exists but main does not contain its work yet"* — so you can see where the code actually is.
- **Request changes** — sends the card back to the top of `doing` with your comment as the spec for the next round.
  Its branch and worktree stay exactly as they were, and a running `/kanban:work` loop picks it up ahead of any new
  work. Feedback is required; the button won't send an empty round trip.
- **Discard** — retires work that will never land. The card closes but its dependents stay blocked, which is the
  point of the difference from Accept. Your comment is folded into the reason.

All three are yours alone: agents address feedback and land what you approve, but they never decide the work was good
enough. Delegated
(external) tickets have no Review tab — their verdict belongs on the issue the daemon is working from.

Dependencies (`depends_on`) block a ticket until they're all done — and since done means landed, a dependent's
fresh worktree is guaranteed to contain its predecessors' code. Epics group tickets, colour their cards, and move
themselves — their column is derived from their tickets; deleting an epic deletes its tickets.
