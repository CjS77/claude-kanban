---
description: "Move long-since-completed tickets out of board.json into an append-only .kanban/archive.json, to keep the live board small."
---

# /kanban:archive — retire old done tickets off the live board

The board is one ever-growing `.kanban/board.json`: every landed ticket stays in it forever, so reads get more
expensive the longer the project runs. This moves tickets that finished long ago into `.kanban/archive.json`, shrinking
the board without losing the history.

Age asked for: `$ARGUMENTS` — how old is "old". `{n}d`, `{n}m`, `{n}y` (days, months, years); **defaults to `30d`**.

Run the helper, passing whatever age the user gave (or nothing, for the 30-day default):

```bash
"{{KANBAN_ROOT}}/commands/scripts/kanban-archive.sh" $ARGUMENTS
```

What it does, and the guarantees you can rely on when you report back:

- **Only landed work moves.** A ticket has an age only once it's in `done` (it carries `completed_at`); anything still
  in flight has no age and is never touched.
- **Dependencies stay intact.** An old ticket is held back while any ticket that *remains* on the board still depends on
  it, so the board never ends up with a dangling `depends_on`. A later run archives it once its dependents are gone —
  so it's normal for a run to leave a few old tickets behind.
- **The archive is append-only.** Tickets already in `archive.json` are never removed, overwritten, or duplicated.
- **The board version is bumped** and the files are written atomically, so a board being served live in the browser
  reloads to the smaller board on its own.

It finds the live board in the main checkout via git, so it works even when this session sits in a worktree. It prints
how many tickets it moved (and their ids/titles), or `nothing … to archive` when none are old enough. Relay that. Exit
codes: **2** no `.kanban/` yet (tell the user to run `/kanban:init`), **3** the age wasn't `{n}d`/`{n}m`/`{n}y`, **4**
the system `date` couldn't compute the cutoff.
