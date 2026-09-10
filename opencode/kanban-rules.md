# Kanban board rules

This project has a Kanban board (in `.kanban/`, live in a browser via `/kanban:open`) shared between the user and agent
sessions, driven over MCP by the `kanban` server this plugin registers. opencode prefixes MCP tool names with the
server name, so the board's tools appear as `kanban_kanban_board`, `kanban_kanban_claim`, and so on — every `kanban_*`
name below means the prefixed tool. `/kanban:work` is the work loop, and running it is the opt-in for claiming tickets.
This plugin launches the board's MCP server as `opencode`, so your claims, landings and notes are already stamped with
that name — you never pass `agent` for your own actions.

A local Kanban board shared with a user who watches it live in a browser. Read the whole board with kanban_board, a
single card with kanban_ticket, or one epic with kanban_epic — prefer the slice reads. kanban_board omits done tickets
and returns a `done` summary of their ids instead; pass include_done=true, or column="done", only when you need
finished specs and logs. Every mutating tool needs expected_version from your latest read; kanban_board and kanban_next
both return one, and kanban_next's landing sweep may have advanced the board, so use the freshest. On a version
conflict, re-read and retry.

Work only what kanban_next surfaces, and only inside an explicit work loop — never claim spontaneously, never touch
draft tickets. It ranks and names the action: "land" (an accepted ticket, top priority), "rework" (sent back with
feedback), "implement" (a ready ticket), "refine" (a stub). Eligibility is always unblocked, unclaimed, non-external.

Ready ticket: kanban_claim → kanban_worktree_start → work in that worktree, committing as you go → kanban_note
progress → kanban_move to review. That move is the close-out: it keeps the branch, drops your claim, and refuses while
the worktree has uncommitted changes, so commit first. Do NOT kanban_worktree_finish at close-out — the worktree is
kept through review so the user can read the code and rework re-attaches instantly, and the board removes it once the
ticket lands. Drop a claim you cannot finish with kanban_release.

Stub: kanban_claim (the card sits pink in doing) → research → kanban_refine, which replaces the spec (splitting off new
tickets or epics when needed) and lands it back in todo at status=review for the user. No worktree.

Done is not yours to declare: the board lands a review ticket in done automatically once its branch or PR merges into
the local main branch — done means landed, and dependencies unblock only then (a discarded done ticket unblocks
nothing).

Rework: a review ticket the user sends back returns to doing unclaimed, ahead of any todo work. Claim it,
kanban_worktree_start re-attaches the kept branch and worktree, and the newest "changes requested:" note is that
round's spec — address it in place and kanban_move to review again.

Reviewing is not judging. A /kanban:review loop writes its findings with kanban_review_comment onto the ticket's review
log — always allowed, whatever auto_review says; never as kanban_note progress, and never replacing what an earlier
reviewer wrote. The verdicts — accept, request changes, discard — are the user's, made in the browser. Press one with
kanban_review only inside a review loop the board's auto_review permits; otherwise your kanban_review_comment is the
deliverable and the card stays in review for the user.

Landing: accepting does not close a ticket — it clears the branch to land, and kanban_next surfaces it as "land" ahead
of everything. Take it with kanban_start_landing, never kanban_claim (claiming a review ticket is the rework path).
Rebase the branch onto main and fast-forward main into it, then call kanban_next and let its sweep move the card.
Resolve a rebase conflict only where the intent is unambiguous; anything you would have to guess at goes back to the
user with kanban_block_landing, after git rebase --abort.

kanban_create_ticket and kanban_create_epic default to status=review so the user vets them. kanban_update_ticket edits
an existing ticket's fields, including depends_on — so a dependency discovered mid-flight is added, dropped or rewired
instead of living only in a note; it replaces the whole list, so read the ticket first and send the set you want.
`auto_merge` makes `/kanban:work` rebase and land the branch into main at review without a user seeing the merge —
never set it on tickets you create unless the user explicitly asked.

kanban_claim and kanban_start_landing stamp the card's owner, and kanban_note its author, with your own agent name by
default — leave `agent` unset for your own actions and pass it only to act as someone else, the way /kanban:delegate
names the external worker it hands a ticket to.
