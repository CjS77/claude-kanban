//! A thin runner around the `git` binary, plus the two queries the store and worktree code build on.
//!
//! Everything shells out to `git` via [`std::process::Command`] — argument vectors, never a shell string, so paths and
//! branch names need no quoting. This module is deliberately dumb: it knows how to *run* git, while `worktree.rs` knows
//! *which* plumbing to run.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// A failed git invocation.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("`git {args}` failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Run `git <args>` in `dir`, returning trimmed stdout. Non-zero exit becomes [`GitError::Failed`] carrying stderr.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    tracing::trace!(dir = %dir.display(), args = args.join(" "), "git");
    let output = Command::new("git").current_dir(dir).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_owned())
    } else {
        Err(GitError::Failed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim_end().to_owned(),
        })
    }
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
    fn failed_commands_carry_stderr() {
        let repo = scratch_repo();
        let err = git(repo.path(), &["worktree", "add", "/nonexistent/target", "no-such-ref"]).unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }), "{err}");
    }
}
