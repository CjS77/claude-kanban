//! A thin runner around the `git` binary (and, for the PR button, `gh`), plus the two queries the store and worktree
//! code build on.
//!
//! Everything shells out via [`std::process::Command`] — argument vectors, never a shell string, so paths and branch
//! names need no quoting. This module is deliberately dumb: it knows how to *run* a program, while `worktree.rs` and
//! `pr.rs` know *which* plumbing to run.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
};

/// A failed invocation. `args` carries the whole command line, program included, so the message names what actually ran.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("`{args}` failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("could not run: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Run `<program> <args>` in `dir` with `envs` set, returning trimmed stdout. Non-zero exit becomes [`GitError::Failed`]
/// carrying stderr.
pub fn run(program: &str, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, GitError> {
    tracing::trace!(dir = %dir.display(), args = args.join(" "), "{program}");
    let output = Command::new(program).current_dir(dir).args(args).envs(envs.iter().copied()).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_owned())
    } else {
        Err(GitError::Failed {
            args: format!("{program} {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim_end().to_owned(),
        })
    }
}

/// Run `git <args>` in `dir`, returning trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    run("git", dir, args, &[])
}

/// The installed git's `(major, minor)`, or `None` when git is missing or its version string is unparseable.
#[must_use] 
pub fn version() -> Option<(u32, u32)> {
    let out = git(Path::new("."), &["version"]).ok()?;
    // "git version 2.43.0" — take the third word, then its first two numeric components.
    let mut parts = out.split_whitespace().nth(2)?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The main working tree's root, asked of git from `cwd`: the first `worktree <path>` stanza of
/// `git worktree list --porcelain`. This is what anchors the store — a process run deep inside a ticket worktree still
/// finds the one true `.kanban/`. `None` outside a git repository.
pub fn main_worktree(cwd: &Path) -> Option<PathBuf> {
    git(cwd, &["worktree", "list", "--porcelain"])
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
}

/// Local branches NOT merged into the main checkout's current HEAD. `None` when it can't be
/// determined (not a git repo, unborn HEAD) — callers must then flag nothing as merged.
///
/// A done ticket's branch counts merged iff it is *absent* from this set, which covers two cases at once: the branch tip
/// is an ancestor of `HEAD` (a real merge), or the branch no longer exists locally (merged-and-deleted — the main case
/// here: `merge.sh` rebases, fast-forwards `main`, then deletes the branch, and GitHub's squash-and-delete lands the same
/// way). Caveat: a branch squash-merged upstream but kept alive locally reads as not merged (its tip is no ancestor);
/// conversely a branch deleted because the work was *abandoned* reads as merged — accepted, since deleting the branch is
/// how you retire it either way.
#[must_use]
pub fn unmerged_branches(repo: &Path) -> Option<HashSet<String>> {
    git(repo, &["branch", "--no-merged", "HEAD", "--format=%(refname:short)"])
        .ok()
        .map(|out| out.lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]).unwrap();
        dir
    }

    #[test]
    fn version_parses() {
        let (major, minor) = version().expect("git is installed in CI and dev environments");
        assert!(major >= 2, "unexpectedly ancient git {major}.{minor}");
    }

    #[test]
    fn main_worktree_finds_the_repo_root_from_a_subdirectory() {
        let repo = scratch_repo();
        let sub = repo.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let found = main_worktree(&sub).expect("inside a repo");
        assert_eq!(found.canonicalize().unwrap(), repo.path().canonicalize().unwrap());
    }

    #[test]
    fn main_worktree_is_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // A tempdir under /tmp can still sit inside *some* repo if the suite itself runs in one; guard by checking the
        // answer is not the tempdir rather than assuming None — but on a clean /tmp this is simply None.
        let found = main_worktree(dir.path());
        assert!(found.is_none() || found.unwrap().canonicalize().unwrap() != dir.path().canonicalize().unwrap());
    }

    #[test]
    fn unmerged_branches_tracks_merge_and_deletion() {
        let repo = scratch_repo();
        let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
        let commit = |msg: &str| {
            let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
            git(repo.path(), &args).unwrap();
        };
        commit("seed");

        // A branch with a commit HEAD lacks is unmerged.
        git(repo.path(), &["checkout", "-q", "-b", "k-9/feature"]).unwrap();
        commit("work");
        git(repo.path(), &["checkout", "-q", "main"]).unwrap();
        assert!(unmerged_branches(repo.path()).unwrap().contains("k-9/feature"));

        // Fast-forwarding main onto it makes the tip an ancestor: merged.
        git(repo.path(), &["merge", "-q", "--ff-only", "k-9/feature"]).unwrap();
        assert!(!unmerged_branches(repo.path()).unwrap().contains("k-9/feature"));

        // A deleted branch is simply absent — the merged-and-deleted arm.
        git(repo.path(), &["checkout", "-q", "-b", "k-10/gone"]).unwrap();
        commit("orphaned work");
        git(repo.path(), &["checkout", "-q", "main"]).unwrap();
        assert!(unmerged_branches(repo.path()).unwrap().contains("k-10/gone"));
        git(repo.path(), &["branch", "-q", "-D", "k-10/gone"]).unwrap();
        assert!(!unmerged_branches(repo.path()).unwrap().contains("k-10/gone"));
    }

    #[test]
    fn unmerged_branches_is_none_where_it_cannot_answer() {
        // An unborn HEAD (fresh init, no commit) can't anchor --no-merged.
        let repo = scratch_repo();
        assert!(unmerged_branches(repo.path()).is_none());
        // Neither can a directory that is no repository at all (a clean /tmp is not inside one).
        let plain = tempfile::tempdir().unwrap();
        assert!(unmerged_branches(plain.path()).is_none());
    }

    #[test]
    fn failed_commands_carry_stderr() {
        let repo = scratch_repo();
        let err = git(repo.path(), &["worktree", "add", "/nonexistent/target", "no-such-ref"]).unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }), "{err}");
    }
}
