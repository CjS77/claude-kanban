---
description: "Fetch one epic and all its tickets from the board as JSON, by number — so you read a slice instead of the whole board.json."
argument-hint: "<epic>"
---

# /kanban:epic — read one epic and its tickets, not the whole board

Sibling to `/kanban:ticket`. If the `kanban_dev` MCP server is connected, call `kanban_epic` directly instead of the
helper below — same data, no shell-out.

```bash
"${CLAUDE_PLUGIN_ROOT}/commands/scripts/kanban-epic.sh" $ARGUMENTS
```

It prints the `epic` (id, title, body, status, colour) and its `tickets` array — each a full ticket — even from a
worktree. An epic with no tickets prints `tickets: []`; that's a real, empty epic.

Exit codes: **1** id not on the board (say so), **2** no `.kanban/` (run `/kanban:init`), **3** no epic number given.
