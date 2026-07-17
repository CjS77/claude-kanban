---
description: "Serve this project's Kanban board and open it in a browser. Reuses a server that's already running instead of starting a second."
argument-hint: "[--port <n>]"
---

# /kanban:open — put the board on screen

Start this project's board UI and open it in the user's browser. If the board is already being served, reuse that
server rather than starting a duplicate.

Arguments given: `$ARGUMENTS`
- `--port <n>` pins the port. Without it, `serve` tries 4747 and picks a free port if another project holds it —
  which is what you want; don't invent a port.

## Steps

1. **Run the launcher, never the bare binary.** The command is:

   ```bash
   "${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp" serve
   ```

   plus `--port <n>` when the user gave one. `claude-kanban` is not on `PATH` and may not exist yet on a fresh
   install: the launcher materialises the binary (download or build) and then `exec`s it, forwarding whatever
   subcommand you pass — despite the name it is not MCP-only. A bare `claude-kanban serve` is the failure to avoid.

2. **Run it in the background.** `serve` blocks until the user stops it, so a foreground call wedges the session
   forever. Use your harness's background-run mechanism, then read the line it prints on stdout — one of:

   - `Serving the board on http://127.0.0.1:<port>/  (ctrl-c to stop)` — you started it.
   - `This board is already being served on http://127.0.0.1:<port>/ (pid N) — not starting a duplicate.` — one was
     already up; that process keeps serving and this one exits. Nothing is wrong, and the browser still opens.

3. **Report the URL** either way, and mention the server keeps running until the user stops it.

4. **No board yet?** stderr saying `no board at … — run 'claude-kanban init' first` means this project has no
   `.kanban/` at all. Tell the user to run `/kanban:init` — don't guess at a fix, and don't run `init` yourself
   unless they ask.
