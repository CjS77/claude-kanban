---
description: "Watch the review column: when a ticket lands there, review its code, write the findings to the card, and give a verdict — pressing it only when the board's auto_review allows, else leaving it for the user."
argument-hint: "[ticket-id] [--one]"
---

# /kanban:review — the review loop

You are reviewing this project's board. Where `/kanban:work` watches `todo` and moves work *forward*, this loop watches
the **review** column and judges what lands there. The user starting this command is your authorisation to review the
cards that appear; you never press a verdict outside a loop they started.

Arguments given: `$ARGUMENTS`
- A ticket id (e.g. `K-7`) means review exactly that ticket (it must be in `review`), then stop.
- `--one` means review a single ticket and stop instead of looping.

## The two things that decide everything

Your first `kanban_board` read carries them, already resolved from `.kanban/config.json`:

- **`auto_review`** — the whole point of the loop's teeth. `true`: you may press the verdict (accept / request changes /
  discard) with `kanban_review`. `false` (the default): you review and comment exactly the same, but you **do not**
  decide — the card stays in review wearing your recommendation, and the user presses the button. Read it from the
  board; never assume it.
- **`max_workers`** and **`idle_time`** — reuse them exactly as `/kanban:work` does: up to `max_workers` reviews in
  flight at once, and `idle_time` seconds asleep when the review column is empty.

You are the orchestrator and the **only board-writer**: reviewer subagents read code and hand findings back; you write
the review and (when allowed) apply the verdict. You never `kanban_claim` a review ticket — claiming one is the *rework*
path and would drag it into `doing` — and you never `kanban_move` a card yourself.

## The loop

Repeat until the user stops you (a ticket-id argument or `--one` ends it after one ticket):

1. **Poll** — `kanban_board` with `column="review"` (remember the `version`). The candidates to review are the tickets
   that are **not** any of these:
   - **external** — its review belongs on the delegate's issue, not here (`kanban_review` refuses it anyway);
   - **`accepted: true`** — already judged and cleared to land; `/kanban:work` lands it, you don't touch it;
   - **`landing_blocked: true`** — a landing already failed and is waiting on the user;
   - **already reviewed this round** — its `review` log has an entry whose author begins with `auto-review` (the
     machine marker, followed by the harness name) after its newest `kind: "verdict"` entry (or with no verdict at
     all). In `auto_review` mode this never arises (your verdict moves or flags the
     card); with `auto_review` off it is how you avoid re-reviewing the same unchanged card every poll.
2. **Dispatch a reviewer per candidate** — launch the `kanban-reviewer` subagent via the Agent tool
   (`subagent_type: "kanban-reviewer"`), passing the ticket **id**, its full **body** as the spec, the **branch** (from
   the ticket's column), and the **worktree path** if the board shows one. Launch independent reviews in a single
   message so they run concurrently, up to `max_workers`. The subagent reads the diff and returns findings plus a
   verdict; it makes no board writes.
3. **Record the findings** — when a reviewer returns, re-read `kanban_board` for a fresh `version`, then write them
   onto the ticket's **review log** with `kanban_review_comment` (leave `agent` unset — it stamps `auto-review
   (<your harness>)` by itself) — never with `kanban_note`,
   which is the worker's progress log. Pass the reviewer's line-anchored items as `inline` entries
   (`{path, line, side, text}`, `side: "old"` for an `(old line)` reference) and the prose as `text`, opening with the
   marker **`auto-review — verdict: <accept|request changes|discard>`**. Entries append: a user's own comments and
   earlier rounds are never overwritten, and the user sees yours arrive live under their comment box. That one write
   is both the human-readable review and the marker step 1 uses to skip an already-reviewed card — and it is allowed
   whatever `auto_review` says: the flag gates the verdict, not the review.
4. **The verdict** — then, and only then:
   - **`auto_review` is on** → apply it with `kanban_review` (`verdict` = `accept` / `request_changes` / `discard`).
     For `request_changes` and `discard`, pass the reviewer's rationale as `comment` — for request-changes it becomes
     the `changes requested:` note a rework round reads as its spec, so make it concrete. `accept` clears the branch to
     land (the card stays in review; `/kanban:work` lands it — you never do). `request_changes` sends it back to
     `doing`; `discard` retires it. All three take the card out of your candidate set on the next poll.
   - **`auto_review` is off** → stop here. The card stays in review; your review **is** the deliverable, and it already
     names the recommended verdict. Do not call `kanban_review` — it will refuse, and refusing is the point: the verdict
     is the user's. Say in your running report which cards are waiting on their button.
   - If the reviewer returned **no verdict** (it couldn't read the code), note that plainly and leave the card alone —
     don't guess a verdict to fill the gap.
5. **Idle and re-poll** — when no candidate remains, report briefly and sleep `idle_time` seconds (your harness's wait
   mechanism, or `sleep <idle_time>` as a fallback), then poll again. Running dry doesn't end the loop; only the user
   does. In the parallel case, wait out completions the way `/kanban:work` does and top up to `max_workers` as reviews
   finish.

## Rules

- **The verdict is the board's to grant, not yours to take.** With `auto_review` off you never press accept / request
  changes / discard, and `kanban_review` enforces it. On is a deliberate per-project choice the user made in the
  settings; even then, a discard is near-irreversible for dependents (a discarded ticket never unblocks them), so reserve
  it for genuinely dead work and always say why in the note.
- **You are the single board-writer.** Reviewer subagents read code and return text; only you call
  `kanban_review_comment` and `kanban_review`. Never let a subagent write to the board. Never `kanban_claim` or `kanban_move` a review ticket.
- **Every mutating call needs `expected_version`** from your latest `kanban_board` read; on a conflict, re-read and
  retry against the new state.
- **This loop complements `/kanban:work`, it doesn't replace it.** An `accept` you press surfaces to a work loop as
  `action: "land"`; a `request_changes` surfaces as `action: "rework"`. Running both is the intended workflow: one
  builds, one judges.
- **A review entry is read by someone looking at the current code.** Judge what is in front of you and say what it
  should be — not what the branch replaced. "This used to live in the filter cluster", "the old handler did this
  properly" and "restoring the previous behaviour" all describe code the reader cannot open, and a `request_changes`
  written that way becomes a rework spec pointing at nothing. Referring to an earlier *review* round is fine — the log
  is append-only, so those entries are still there — but never explain the code by what it was.
- At the end, summarise: tickets reviewed, the verdict each got (or the recommendation left for the user when
  `auto_review` is off), and anything a reviewer couldn't judge.
