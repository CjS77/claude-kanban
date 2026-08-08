---
description: "Serve this project's Kanban board and open it in a browser. Reuses a server that's already running instead of starting a second."
---

# /kanban:open — put the board on screen

Start this project's board UI and open it in the user's browser. If the board is already being served, reuse that
server rather than starting a duplicate.

Arguments given: `$ARGUMENTS`
- `--port <n>` pins the port. Without it, `serve` tries 4747 and picks a free port if another project holds it —
  which is what you want; don't invent a port.

## Steps

1. **Run the launcher, never the bare binary** — `claude-kanban` is not on `PATH` and may not exist yet on a fresh
   install: the launcher materialises the binary (download or build) and then `exec`s it, forwarding whatever
   subcommand you pass — despite the name it is not MCP-only. A bare `claude-kanban serve` is the failure to avoid.

2. **Detach it from your shell.** `serve` blocks until the user stops it, and this harness's shell tool has no
   background mode — a foreground call sits until the tool's timeout kills it, taking the server down with it. Start
   it detached and read back what it printed:

   ```bash
   log="$(mktemp)" && nohup "{{KANBAN_ROOT}}/bin/kanban-mcp" serve >"$log" 2>&1 &
   for _ in $(seq 1 30); do grep -q "http://127.0.0.1" "$log" && break; sleep 1; done; cat "$log"
   ```

   plus `--port <n>` after `serve` when the user gave one. The 30-second wait covers a first run, where the launcher
   downloads (or builds) the binary before serving. Expect one of two lines:

   - `Serving the board on http://127.0.0.1:<port>/  (ctrl-c to stop)` — you started it.
   - `This board is already being served on http://127.0.0.1:<port>/ (pid N) — not starting a duplicate.` — one was
     already up; that process keeps serving and this one exits. Nothing is wrong, and the browser still opens.

   If neither appeared, the log's tail says why — most likely a first-run `cargo build` still compiling (tell the
   user it's building and to re-run `/kanban:open` in a minute or two; the build carries on) or the no-board error
   in step 4. `.kanban/serve.pid` records the live `pid` and `port` once a server is up.

3. **Report the URL** either way, and mention the server keeps running until the user stops it (`nohup` detached it
   from this session, so it outlives opencode).

4. **No board yet?** stderr saying `no board at … — run 'claude-kanban init' first` means this project has no
   `.kanban/` at all. Tell the user to run `/kanban:init` — don't guess at a fix, and don't run `init` yourself
   unless they ask.
