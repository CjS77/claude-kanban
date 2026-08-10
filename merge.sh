#!/usr/bin/env bash
# merge.sh <branch> — rebase <branch> onto main, fast-forward main, delete the branch.
#
# DEPRECATED. The normal way to land a branch is now the board: press Accept on the
# review card, which clears the work to land, and a running `/kanban:work` loop does
# exactly what this script does — with an agent that can actually resolve a rebase
# conflict, and with the card flagged for you when it cannot.
#
# It is kept, and still shipped into every project, for the case that flow does not
# cover: landing a branch with no work loop running, or from a session that only
# speaks MCP. Accept alone never moves main.
#
# Removes the branch's ticket worktree first when it is clean, and refuses only if
# it has uncommitted changes. On rebase conflicts it stops mid-rebase so you can
# resolve them by hand.
set -euo pipefail

branch="${1:?usage: merge.sh <branch>}"

if ! git show-ref --verify --quiet "refs/heads/${branch}"; then
    echo "error: branch '${branch}' does not exist" >&2
    exit 1
fi

# Safeguard 1: the branch is normally still checked out in its ticket worktree — worktrees are kept through review now,
# so refusing here would refuse every review ticket. Remove it when it is clean; only uncommitted work stops the merge.
this_wt=$(git rev-parse --show-toplevel)
wt_path=$(git worktree list --porcelain | awk -v b="branch refs/heads/${branch}" '/^worktree /{w=substr($0,10)} $0==b{print w}')
if [[ -n "${wt_path}" && "${wt_path}" != "${this_wt}" ]]; then
    if [[ -n "$(git -C "${wt_path}" status --porcelain)" ]]; then
        echo "error: the worktree at ${wt_path} has uncommitted changes" >&2
        echo "commit them there (or discard them) before merging '${branch}'" >&2
        exit 1
    fi
    echo "removing the ticket worktree at ${wt_path}"
    git worktree remove "${wt_path}"
    git worktree prune
fi

git checkout "${branch}"

# Safeguard 2: on conflict, stop mid-rebase and leave it for the user to resolve.
if ! git rebase main; then
    echo "error: rebasing '${branch}' onto main hit conflicts" >&2
    echo "fix the conflicts, then: git rebase --continue && git checkout main && git merge --ff-only ${branch} && git branch -d ${branch}" >&2
    echo "or to give up: git rebase --abort" >&2
    exit 1
fi

git checkout main
git merge --ff-only "${branch}"

# Safeguard 3: set -e means we only reach this line if everything above succeeded.
git branch -d "${branch}"
echo "merged and deleted '${branch}'"
