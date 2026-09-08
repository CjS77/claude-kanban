#!/usr/bin/env bash
# Print one ticket from the board as JSON, by number.
# Usage: kanban-ticket.sh <ticket-num>   (48, K-48 and k-48 all mean K-48)
# Exit: 0 found, 1 no such ticket, 2 no board, 3 bad usage.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: kanban-ticket.sh <ticket-num>" >&2; exit 3; }

# The live board sits in the main checkout, even when we're in a worktree
# (each worktree carries its own tracked-but-stale copy).
board="$(dirname "$(git rev-parse --git-common-dir 2>/dev/null)")/.kanban/board.json"
[ -f "$board" ] || board=".kanban/board.json"
[ -f "$board" ] || { echo "no board at $board — run /kanban:init first" >&2; exit 2; }

num="${1#[Kk]-}"; id="K-${num}"

out="$(jq --arg id "$id" '.tickets[] | select(.id == $id)' "$board")"
[ -n "$out" ] || { echo "no ticket $id on the board" >&2; exit 1; }
printf '%s\n' "$out"
