#!/usr/bin/env bash
# merge.sh <branch> — rebase <branch> onto main, fast-forward main, delete the branch.
# Refuses to run while the branch is still checked out in a linked worktree. On
# rebase conflicts it stops mid-rebase so you can resolve them by hand.
set -euo pipefail

branch="${1:?usage: merge.sh <branch>}"

if ! git show-ref --verify --quiet "refs/heads/${branch}"; then
    echo "error: branch '${branch}' does not exist" >&2
    exit 1
fi

# Safeguard 1: bail if the branch is checked out in a worktree other than this one.
this_wt=$(git rev-parse --show-toplevel)
wt_path=$(git worktree list --porcelain | awk -v b="branch refs/heads/${branch}" '/^worktree /{w=substr($0,10)} $0==b{print w}')
if [[ -n "${wt_path}" && "${wt_path}" != "${this_wt}" ]]; then
    echo "error: branch '${branch}' is still checked out in worktree ${wt_path}" >&2
    echo "remove it first: git worktree remove ${wt_path}" >&2
    exit 1
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
