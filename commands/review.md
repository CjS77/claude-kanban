---
description: "Watch the review column: when a ticket lands there, review its code, write the findings to the card, and give a verdict — pressing it only when the board's auto_review allows, else leaving it for the user."
argument-hint: "[ticket-id] [--one]"
---

# /kanban:review — the review loop

You review this project's board; `/kanban:work` moves work forward, you judge what lands in **review**. Starting this
command is your authorisation to review — you never press a verdict outside a loop the user started.

Arguments: `$ARGUMENTS` — a ticket id reviews exactly that ticket (must be in `review`); `--one` reviews one then stops.

Your first `kanban_board` read carries the decisions, already resolved from config:
- **`auto_review`** — `true`: you may press the verdict (`kanban_review`). `false` (the default): you review and comment
  the same but **do not decide** — the card stays in review wearing your recommendation. Read it; never assume it.
- **`max_workers`** / **`idle_time`** — reuse exactly as `/kanban:work` does.

You are the orchestrator and the **only board-writer**: reviewer subagents read code and hand back findings; you write
the review and (when allowed) the verdict. Never `kanban_claim` a review ticket (that's the rework path) or
`kanban_move` a review card.

## The loop

Repeat until the user stops you (a ticket id or `--one` ends it):

1. **Poll** — `kanban_board` with `column="review"` (note the `version`). Candidates exclude: **external** (review
   belongs on the delegate's issue), **`accepted: true`** (cleared to land; `/kanban:work` lands it), **`landing_blocked`**,
   and **already reviewed this round** — a `review` entry by `auto-review` after the newest verdict entry (with
   `auto_review` off this is how you skip unchanged cards).
2. **Dispatch a reviewer per candidate** — the `kanban-reviewer` subagent (Agent tool), passing the ticket **id**, its
   full **body** as the spec, the **branch**, and the **worktree path** if the board shows one. Launch independent ones
   in a single message, up to `max_workers`. It reads the diff and returns findings plus a verdict; no board writes.
3. **Record the findings** — when one returns, re-read `kanban_board` for a fresh version, then `kanban_review_comment`
   (leave `agent` unset — it stamps `auto-review (<harness>)` itself); never `kanban_note`, the worker's log. Pass
   line-anchored items as `inline` (`{path, line, side, text}`, `side: "old"` for an old-line ref) and prose as `text`,
   opening with **`auto-review — verdict: <accept|request changes|discard>`**. Entries append; nothing is overwritten.
4. **The verdict** — then, and only then:
   - `auto_review` **on** → apply it with `kanban_review`. For `request_changes`/`discard`, pass the rationale as
     `comment` (for request-changes it becomes the rework spec). `accept` clears the branch to land (the card stays in
     review; `/kanban:work` lands it). All three take the card out of your next poll's candidates.
   - `auto_review` **off** → stop here. The card stays in review; your review **is** the deliverable. Don't call
     `kanban_review` — it refuses, and that's the point: the verdict is the user's. Say which cards wait on their button.
   - Reviewer returned **no verdict** (couldn't read the code) → note it plainly, leave the card alone; never guess.
5. **Idle and re-poll** — no candidate: report briefly, sleep `idle_time` seconds (harness wait, or `sleep <idle_time>`),
   poll again. Running dry doesn't end the loop; only the user does. In the parallel case, top up to `max_workers` as
   reviews finish.

## Rules

- **The verdict is the board's to grant, not yours to take.** `auto_review` off → never press one. Even on, reserve
  `discard` for genuinely dead work (a discarded ticket never unblocks dependents) and always say why.
- **You are the single board-writer.** Only you call `kanban_review_comment`/`kanban_review`; never let a subagent write
  to the board.
- **Every mutation needs `expected_version`** from your latest read; on conflict, re-read and retry.
- **This complements `/kanban:work`.** Your `accept` surfaces to a work loop as `land`; `request_changes` as `rework`.
- **Review the current code, not what it replaced.** "The old handler did this" names code the reader can't open and
  makes a `request_changes` a spec pointing at nothing. Referring to an earlier *review* round is fine — the log is
  append-only — but never explain code by what it was.
- Summarise at the end: tickets reviewed, the verdict each got (or the recommendation left when `auto_review` is off),
  and anything a reviewer couldn't judge.
