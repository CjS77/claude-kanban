---
description: "Create this project's Kanban board and config, then open it in a browser. Safe to re-run: an existing board is opened, never overwritten."
argument-hint: ""
---

# /kanban:init — set this project up with a board

1. **Seed the store** (foreground):

   ```bash
   "${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp" init
   ```

   Use the launcher, not a bare `claude-kanban` — the binary isn't on `PATH` and may not exist on a fresh install.

2. **Read the result.** Success prints `Initialised an empty board at …`: it created `.kanban/board.json`,
   `.kanban/config.json`, a store-local `.gitignore`, and `.kanban/merge.sh` (manual land helper); config fully defined,
   `port` `null` so `serve` can hunt. **`already exists` on stderr is not a failure** — say so and carry on. Any other
   non-zero exit is a real failure: report stderr and stop.

3. **Open the board** — exactly what `/kanban:open` does (see `commands/open.md`); **run `serve` in the background**,
   since it blocks until stopped.

4. **Tell the user to commit** `.kanban/board.json`, `.kanban/config.json`, `.kanban/merge.sh`. `merge.sh` is a
   deprecated fallback for landing a branch by hand (`.kanban/merge.sh <branch>`) when no loop runs; normally they press
   **Accept** instead. The ⚙ gear edits config later; the rest of `.kanban/` is machine-local and gitignored.
