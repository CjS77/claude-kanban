#!/usr/bin/env bash
# Move tickets completed longer ago than <age> out of the board and into .kanban/archive.json.
# Usage: kanban-archive.sh [<age>]   age is {n}d | {n}m | {n}y (days/months/years), default 30d.
#
# Only tickets that have landed in done (they carry column.completed_at) have an age; everything
# still in flight is left alone. The archive is append-only: tickets already in it are never
# removed or overwritten, and a ticket is not re-added if its id is already there. A candidate is
# held back while any ticket that REMAINS on the board still depends on it, so the board never ends
# up with a dangling depends_on (a later run archives it once its dependents are gone too).
#
# Exit: 0 done (0 or more moved), 2 no board, 3 bad usage, 4 could not compute the cutoff date.
set -euo pipefail

age="${1:-30d}"
[[ "$age" =~ ^([0-9]+)([dmy])$ ]] || { echo "usage: kanban-archive.sh [<n>d|<n>m|<n>y]  (default 30d)" >&2; exit 3; }
n="${BASH_REMATCH[1]}"; unit="${BASH_REMATCH[2]}"
case "$unit" in d) span="$n days";; m) span="$n months";; y) span="$n years";; esac

# GNU date first (Linux), BSD date second (macOS). Both yield a UTC RFC3339 string that compares
# lexicographically against the board's completed_at timestamps.
cutoff="$(date -u -d "$span ago" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
  || date -u -v"-${n}${unit}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)" \
  || { echo "could not compute a cutoff date for '$age' (need GNU or BSD date)" >&2; exit 4; }

# The live board sits in the main checkout, even when we're in a worktree.
board="$(dirname "$(git rev-parse --git-common-dir 2>/dev/null)")/.kanban/board.json"
[ -f "$board" ] || board=".kanban/board.json"
[ -f "$board" ] || { echo "no board at $board — run /kanban:init first" >&2; exit 2; }
dir="$(dirname "$board")"
archive="$dir/archive.json"
[ -f "$archive" ] || echo '{"tickets":[]}' > "$archive"

# Which done tickets are old enough to move, held back if a surviving ticket still depends on them.
plan_prog='
  [ .tickets[] | select((.column.completed_at // "") != "" and .column.completed_at < $c) | .id ] as $cand
  | ([ .tickets[].id ] - $cand) as $keep0
  | .tickets as $ts
  | ( { keep: $keep0, changed: true }
      | until(.changed == false;
          .keep as $k
          | [ $ts[] | select(.id | IN($k[])) | .depends_on[]? ] as $needed
          | ( ($k + [ $cand[] | select(IN($needed[])) ]) | unique ) as $k2
          | { keep: $k2, changed: (($k2 | length) != ($k | length)) } )
      | .keep ) as $keep
  | ($cand - $keep)'

ids="$(jq -c --arg c "$cutoff" "$plan_prog" "$board")"
count="$(jq 'length' <<<"$ids")"

if [ "$count" -eq 0 ]; then
  echo "nothing completed before $cutoff to archive (age $age)"
  exit 0
fi

# Atomic rewrites: temp files in the same dir, then rename. Build the archive from the ORIGINAL
# board (it still holds the tickets) before overwriting the board.
tmp_arc="$(mktemp "$dir/.archive.XXXXXX")"
tmp_brd="$(mktemp "$dir/.board.XXXXXX")"
trap 'rm -f "$tmp_arc" "$tmp_brd"' EXIT

jq --argjson ids "$ids" --slurpfile board "$board" '
  (.tickets // []) as $existing
  | ($existing | map(.id)) as $have
  | [ $board[0].tickets[] | select(.id | IN($ids[])) | select(.id | IN($have[]) | not) ] as $new
  | { tickets: ($existing + $new) }
' "$archive" > "$tmp_arc"

jq --argjson ids "$ids" '.version += 1 | .tickets |= map(select(.id | IN($ids[]) | not))' "$board" > "$tmp_brd"

mv "$tmp_arc" "$archive"
mv "$tmp_brd" "$board"
trap - EXIT

echo "archived $count ticket(s) completed before $cutoff (age $age) to $archive"
jq -r --argjson ids "$ids" '.tickets[] | select(.id | IN($ids[])) | "  \(.id)  \(.title)"' "$archive"
