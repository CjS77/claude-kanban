---
description: "Serve this project's Kanban board and open it in a browser. Reuses a server that's already running instead of starting a second."
argument-hint: "[--port <n>]"
---

# /kanban:open — put the board on screen

Run the launcher, never the bare binary (`claude-kanban` isn't on `PATH`); add `--port <n>` when the user gave one —
without it `serve` tries 4747 and picks a free port. Run it **in the background** — `serve` blocks until stopped, so a
foreground call wedges the session:

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp" serve
```

Then read the line it prints on stdout:
- `Serving the board on http://127.0.0.1:<port>/  (ctrl-c to stop)` — you started it.
- `This board is already being served on http://127.0.0.1:<port>/ (pid N) — not starting a duplicate.` — one was already
  up; nothing is wrong, the browser still opens.

Report the URL either way (the server keeps running until the user stops it). If stderr says `no board at … — run
'claude-kanban init' first`, tell the user to run `/kanban:init` — don't run it yourself unless asked.
