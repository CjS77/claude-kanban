---
description: "Create this project's Kanban board and config, then open it in a browser. Safe to re-run: an existing board is opened, never overwritten."
argument-hint: ""
---

# /kanban:init — set this project up with a board

Seed `.kanban/` in this project and put the board on screen. This is the first thing a user runs after installing the
plugin, so it ends with a working board every time — including when there already was one.

## Steps

1. **Seed the store.** Run, in the foreground (it's fast and terminates):

   ```bash
   "${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp" init
   ```

   Use the launcher, not a bare `claude-kanban`: the binary isn't on `PATH` and may not exist yet on a fresh install.
   The launcher materialises it and forwards the subcommand.

2. **Read the result.**
   - Success prints `Initialised an empty board at …` — it created `.kanban/board.json`, `.kanban/config.json`, and a
     store-local `.gitignore`.
   - **`already exists` on stderr is not a failure to report.** The project already has a board and `init` refused to
     clobber it — exactly right. Say the board already exists and carry on to step 3.
   - Any other non-zero exit is a real failure: report stderr and stop.

3. **Open the board.** Do exactly what `/kanban:open` does — see `commands/open.md` for the full behaviour; don't
   reimplement it from memory here. The one thing worth repeating because it bites hardest: **run `serve` in the
   background**, since it blocks until stopped and a foreground call wedges the session.

4. **Tell the user to commit** `.kanban/board.json` and `.kanban/config.json`. The rest of `.kanban/` (claims, locks,
   pid files) is machine-local and the seeded `.gitignore` already covers it.
