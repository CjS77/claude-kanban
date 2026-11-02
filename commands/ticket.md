---
description: "Fetch one ticket from the board as JSON, by number — so you read a single card instead of the whole board.json."
argument-hint: "<ticket-num>"
---

# /kanban:ticket — read one card, not the whole board

If the `kanban_dev` MCP server is connected, call `kanban_ticket` directly instead of the helper below — same data, no
shell-out.

```bash
"${CLAUDE_PLUGIN_ROOT}/commands/scripts/kanban-ticket.sh" $ARGUMENTS
```

It prints the whole ticket object — id, title, status, body, labels, effort, notes, column — even from a worktree.

Exit codes: **1** id not on the board (say so; suggest a near miss), **2** no `.kanban/` (run `/kanban:init`), **3** no
ticket number given.
