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

/// Whether `rev` is an ancestor of `of` (`git merge-base --is-ancestor`). `None` when git cannot answer — no repo,
/// unknown rev — as distinct from a definite no.
#[must_use]
pub fn is_ancestor(repo: &Path, rev: &str, of: &str) -> Option<bool> {
    let out = Command::new("git").current_dir(repo).args(["merge-base", "--is-ancestor", rev, of]).output().ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None, // 128 etc.: not a repo, unknown revision — no answer, not a "no"
    }
}

/// The repository's integration branch, best effort: what `origin/HEAD` points at when the remote has declared one,
/// else a local `main`, else a local `master`, else `None`. Config (`main_branch`) beats this — see
/// [`crate::config::Config::main_branch`].
#[must_use]
pub fn detect_main_branch(repo: &Path) -> Option<String> {
    if let Ok(sym) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        // "origin/main" → "main"
        if let Some((_, name)) = sym.split_once('/') {
            return Some(name.to_owned());
        }
    }
    ["main", "master"].into_iter().find(|b| branch_exists(repo, b)).map(str::to_owned)
}

/// The branch currently checked out in `dir`; `None` on a detached HEAD or outside a repo.
#[must_use]
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty())
}

/// Every local branch name. `None` when git can't answer.
#[must_use]
pub fn local_heads(repo: &Path) -> Option<HashSet<String>> {
    git(repo, &["for-each-ref", "refs/heads", "--format=%(refname:short)"])
        .ok()
        .map(|out| out.lines().map(str::to_owned).collect())
}

/// Whether the local branch exists.
#[must_use]
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--quiet", "--verify", &format!("refs/heads/{branch}")]).is_ok()
}

/// Whether every commit reachable from `tip` but not from `upstream` is patch-equivalent to a commit already in
/// `upstream` (`git cherry` reports each as `-`). This is what proves a rebase-then-fast-forward landing *after* the
/// original branch ref is gone: the rebased copies in `upstream` carry the same patch-ids. `None` when git can't answer
/// (no repo, or the tip's objects have been gc'd).
#[must_use]
pub fn cherry_equivalent(repo: &Path, upstream: &str, tip: &str) -> Option<bool> {
    git(repo, &["cherry", upstream, tip]).ok().map(|out| out.lines().all(|l| l.starts_with('-')))
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

    fn commit_in(repo: &Path, msg: &str) {
        let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
        let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
        git(repo, &args).unwrap();
    }

    #[test]
    fn is_ancestor_distinguishes_yes_no_and_cannot_answer() {
        let repo = scratch_repo();
        commit_in(repo.path(), "seed");
        git(repo.path(), &["checkout", "-q", "-b", "k-1/work"]).unwrap();
        commit_in(repo.path(), "work");
        git(repo.path(), &["checkout", "-q", "main"]).unwrap();

        assert_eq!(is_ancestor(repo.path(), "main", "k-1/work"), Some(true));
        assert_eq!(is_ancestor(repo.path(), "k-1/work", "main"), Some(false));
        assert_eq!(is_ancestor(repo.path(), "no-such-rev", "main"), None, "an unknown rev is no answer, not a no");
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(is_ancestor(plain.path(), "a", "b"), None);
    }

    #[test]
    fn detect_main_branch_prefers_origin_head_then_local_names() {
        // A repo whose default branch is neither main nor master: only origin/HEAD can name it.
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "trunk"]).unwrap();
        commit_in(upstream.path(), "seed");

        let clone_parent = tempfile::tempdir().unwrap();
        let clone = clone_parent.path().join("clone");
        git(clone_parent.path(), &["clone", "-q", upstream.path().to_str().unwrap(), clone.to_str().unwrap()]).unwrap();
        assert_eq!(detect_main_branch(&clone).as_deref(), Some("trunk"), "clone sets origin/HEAD");

        // No remote: fall back to a local main…
        let repo = scratch_repo();
        commit_in(repo.path(), "seed");
        assert_eq!(detect_main_branch(repo.path()).as_deref(), Some("main"));

        // …then master, then nothing.
        let older = tempfile::tempdir().unwrap();
        git(older.path(), &["init", "-q", "-b", "master"]).unwrap();
        commit_in(older.path(), "seed");
        assert_eq!(detect_main_branch(older.path()).as_deref(), Some("master"));

        let odd = tempfile::tempdir().unwrap();
        git(odd.path(), &["init", "-q", "-b", "trunk"]).unwrap();
        commit_in(odd.path(), "seed");
        assert_eq!(detect_main_branch(odd.path()), None);
    }

    #[test]
    fn current_branch_and_local_heads_answer_and_degrade() {
        let repo = scratch_repo();
        commit_in(repo.path(), "seed");
        git(repo.path(), &["branch", "-q", "k-2/extra"]).unwrap();

        assert_eq!(current_branch(repo.path()).as_deref(), Some("main"));
        let heads = local_heads(repo.path()).unwrap();
        assert!(heads.contains("main") && heads.contains("k-2/extra"));
        assert!(branch_exists(repo.path(), "k-2/extra") && !branch_exists(repo.path(), "k-3/nope"));

        git(repo.path(), &["checkout", "-q", "--detach"]).unwrap();
        assert_eq!(current_branch(repo.path()), None, "detached HEAD is no branch");
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(local_heads(plain.path()), None);
    }

    #[test]
    fn cherry_equivalent_proves_a_rebase_then_ff_landing() {
        // The merge.sh shape: work on a branch, main moves on, the branch is rebased onto main and main fast-forwarded.
        // The *original* (pre-rebase) tip's commits are then patch-equivalent to what landed, even though the tip itself
        // is no ancestor of main.
        let repo = scratch_repo();
        std::fs::write(repo.path().join("f"), "seed\n").unwrap();
        git(repo.path(), &["add", "f"]).unwrap();
        commit_in(repo.path(), "seed");

        git(repo.path(), &["checkout", "-q", "-b", "k-4/work"]).unwrap();
        std::fs::write(repo.path().join("work"), "work\n").unwrap();
        git(repo.path(), &["add", "work"]).unwrap();
        commit_in(repo.path(), "work");
        let original_tip = git(repo.path(), &["rev-parse", "k-4/work"]).unwrap();

        git(repo.path(), &["checkout", "-q", "main"]).unwrap();
        std::fs::write(repo.path().join("other"), "other\n").unwrap();
        git(repo.path(), &["add", "other"]).unwrap();
        commit_in(repo.path(), "mainline moves");

        assert_eq!(cherry_equivalent(repo.path(), "main", &original_tip), Some(false), "not landed yet");

        git(repo.path(), &["checkout", "-q", "k-4/work"]).unwrap();
        git(repo.path(), &["-c", "user.name=t", "-c", "user.email=t@example.com", "rebase", "-q", "main"]).unwrap();
        git(repo.path(), &["checkout", "-q", "main"]).unwrap();
        git(repo.path(), &["merge", "-q", "--ff-only", "k-4/work"]).unwrap();
        git(repo.path(), &["branch", "-q", "-D", "k-4/work"]).unwrap();

        assert_eq!(cherry_equivalent(repo.path(), "main", &original_tip), Some(true), "patch-ids prove the landing");
        assert_eq!(cherry_equivalent(repo.path(), "main", "no-such-rev"), None);
    }

    #[test]
    fn failed_commands_carry_stderr() {
        let repo = scratch_repo();
        let err = git(repo.path(), &["worktree", "add", "/nonexistent/target", "no-such-ref"]).unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }), "{err}");
    }
}
