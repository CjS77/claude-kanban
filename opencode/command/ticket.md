---
description: "Fetch one ticket from the board as JSON, by number — so you read a single card instead of the whole board.json."
---

# /kanban:ticket — read one card, not the whole board

The board is a single, ever-growing `.kanban/board.json`. Loading it to find one card burns tokens on every unrelated
ticket. This pulls out just the one you asked for.

Run the helper, passing the number the user gave (`48`, `K-48` and `k-48` all mean ticket **K-48**):

```bash
"{{KANBAN_ROOT}}/commands/scripts/kanban-ticket.sh" $ARGUMENTS
```

It prints the whole ticket object — id, title, status, body, labels, effort, notes, column — and finds the live board
in the main checkout via git, so it reads the right copy even when this session sits in a worktree. Relay what the user
needs from it.

Non-zero exit codes: **1** the id isn't on the board (say so; suggest a near miss rather than guessing its contents),
**2** this project has no `.kanban/` yet (tell the user to run `/kanban:init` — don't create one yourself), **3** no
ticket number was given.
