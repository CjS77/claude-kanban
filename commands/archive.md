---
description: "Move long-since-completed tickets out of board.json into an append-only .kanban/archive.json, to keep the live board small."
argument-hint: "[<age>]  e.g. 30d, 6m, 1y (default 30d)"
---

# /kanban:archive — retire old done tickets off the live board

`$ARGUMENTS` is how old "old" is: `{n}d`, `{n}m`, `{n}y`; **defaults to `30d`**.

```bash
"${CLAUDE_PLUGIN_ROOT}/commands/scripts/kanban-archive.sh" $ARGUMENTS
```

Guarantees: **only landed work moves** (only `done` tickets have an age); **done epics move with their tickets** while
no remaining ticket names them; **dependencies stay intact** (held back while a remaining ticket depends on them, so a
run leaving a few old tickets behind is normal); **append-only**; and the **version is bumped** with atomic writes, so
the live board reloads smaller. The 🗄️ toggle shows the archive as a read-only fifth column.

It prints how many tickets moved (ids/titles), or `nothing … to archive`. Exit codes: **2** no `.kanban/` (run
`/kanban:init`), **3** bad age, **4** `date` couldn't compute the cutoff.
