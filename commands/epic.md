---
description: "Fetch one epic and all its tickets from the board as JSON, by number — so you read a slice instead of the whole board.json."
argument-hint: "<epic>"
---

# /kanban:epic — read one epic and its tickets, not the whole board

Sibling to `/kanban:ticket`. The board is a single, ever-growing `.kanban/board.json`; loading it to see what's under
one epic burns tokens on every unrelated card. This pulls out the epic and the tickets that belong to it.

If the `kanban_dev` MCP server is connected, call its `kanban_epic` tool directly instead of running the helper below —
same data, no shell-out. This command remains the way in for a harness (or a session) without that MCP connection.

Run the helper, passing the number the user gave (`3`, `EP-3` and `ep-3` all mean epic **EP-3**):

```bash
"${CLAUDE_PLUGIN_ROOT}/commands/scripts/kanban-epic.sh" $ARGUMENTS
```

It prints an object with the `epic` (id, title, body, status, colour) and its `tickets` array — each a full ticket
object — and finds the live board in the main checkout even when you're standing in a worktree. An epic that owns no
tickets prints with `tickets: []`; that's a real, empty epic. Relay what the user needs.

Non-zero exit codes: **1** the id isn't on the board (say so rather than guessing), **2** this project has no
`.kanban/` yet (tell the user to run `/kanban:init` — don't create one yourself), **3** no epic number was given.
