# Kanban board rules

This project has a Kanban board (in `.kanban/`, live in a browser via `/kanban:open`) shared between the human and
agent sessions, driven over MCP by the `kanban` server this plugin registers. opencode prefixes MCP tool names with
the server name, so the board's tools appear as `kanban_kanban_board`, `kanban_kanban_claim`, and so on — every
`kanban_*` name below means the prefixed tool. `/kanban:work` is the work loop, and running it is the opt-in for
claiming tickets.

A local Kanban board shared with a human (who sees it live in a browser). The lifecycle of a ready ticket:
kanban_claim → kanban_worktree_start → work in the worktree, committing as you go → kanban_note progress →
kanban_worktree_finish → kanban_move to review. Done is not yours to declare: the board lands review tickets in done
automatically once their branch or PR is merged into the local main branch — done means landed, and dependencies
unblock only then (a discarded done ticket never unblocks anything). A review ticket can be claimed again for rework
(PR feedback); its branch is kept and kanban_worktree_start re-attaches. Stubs are specs to write, not code to build:
kanban_claim (the card sits pink in doing) → research → kanban_refine, which lands it back in todo at status=review
for the human — no worktree. Only claim tickets kanban_next surfaces — ready (implement) or stub (refine), in todo,
unblocked; never claim spontaneously outside an explicit work loop. Never touch draft tickets. Tickets you create
default to status=review so the human vets them. kanban_update_ticket edits an existing ticket's fields — including
depends_on, so a dependency discovered mid-flight can be added, dropped or rewired instead of living only in a note;
it replaces the whole list, so read the ticket first and send the set you want. Mutating tools need expected_version
from your latest kanban_board read (kanban_next also returns one — use it, its landing sweep may have advanced the
board); on a version conflict, re-read and retry. `auto_merge` makes `/kanban:work` rebase and land the ticket's
branch into main when it reaches review, without a human seeing the merge. Never set it on tickets you create unless
the user explicitly asked for it. kanban_board omits done tickets by default and returns a `done` summary of their
ids instead — their specs and progress logs are the bulk of the board and finished work is not input to your next
decision; pass include_done=true, or column="done", when you actually need to read them.
