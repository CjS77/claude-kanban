#!/usr/bin/env bash
# Print one epic and all its tickets from the board as JSON, by number.
# Usage: kanban-epic.sh <epic>   (3, EP-3 and ep-3 all mean EP-3)
# Prints {epic, tickets}. Exit: 0 epic exists, 1 no such epic, 2 no board, 3 bad usage.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: kanban-epic.sh <epic>" >&2; exit 3; }

# The live board sits in the main checkout, even when we're in a worktree
# (each worktree carries its own tracked-but-stale copy).
board="$(dirname "$(git rev-parse --git-common-dir 2>/dev/null)")/.kanban/board.json"
[ -f "$board" ] || board=".kanban/board.json"
[ -f "$board" ] || { echo "no board at $board — run /kanban:init first" >&2; exit 2; }

num="${1#[Ee][Pp]-}"; id="EP-${num}"

jq --exit-status --arg id "$id" \
  '{epic: (.epics[] | select(.id == $id)), tickets: [.tickets[] | select(.epic == $id)]}' \
  "$board" || { echo "no epic $id on the board" >&2; exit 1; }
