//! The cross-process advisory lock serializing every read-modify-write cycle.
//!
//! Both faces of the binary — the HTTP server and the MCP server — plus the worktree CLI can write the store concurrently,
//! from different processes. An exclusive [flock]-style lock on `.kanban/.lock` (via `fs4`) makes each mutation cycle atomic
//! with respect to the others. Plain reads never take it: atomic-rename writes mean a reader always sees a complete file.
//!
//! The lock is *advisory*: an editor writing `board.json` by hand doesn't take it, which is exactly why the store also keeps
//! the optimistic `version` counter. And note the fine print: flock is per open-file-description, so the guard owns its
//! [`File`] for its whole scope, and every acquisition opens the file afresh. Network filesystems (NFS) degrade flock —
//! keep the repo on a local disk.
//!
//! [flock]: https://man7.org/linux/man-pages/man2/flock.2.html

use std::{fs::File, path::Path};

use fs4::FileExt;

use super::StoreError;

/// Name of the lock file inside the store directory. Gitignored (see `init`).
const LOCK_FILE: &str = ".lock";

/// An acquired exclusive lock on a store. Held for the duration of one read-modify-write cycle; released on drop.
#[derive(Debug)]
pub(crate) struct StoreLock {
    file: File,
}

/// Block until the store's exclusive lock is acquired. The lock file is created on first use and never removed.
pub(crate) fn acquire(dir: &Path) -> Result<StoreLock, StoreError> {
    let path = dir.join(LOCK_FILE);
    let io_err = |source| StoreError::Io { path: path.clone(), source };
    let file = File::create(&path).map_err(io_err)?;
    FileExt::lock(&file).map_err(io_err)?;
    Ok(StoreLock { file })
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Dropping the File would release the lock anyway (last close of the description); unlocking explicitly just makes
        // the release prompt and the intent clear. Nothing sensible to do with a failure here.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquisition_waits_for_the_first_to_drop() {
        let dir = tempfile::tempdir().unwrap();
        let guard = acquire(dir.path()).unwrap();

        // A distinct open file description must contend: try_lock from a second handle fails while the guard lives.
        let probe = File::create(dir.path().join(LOCK_FILE)).unwrap();
        assert!(FileExt::try_lock(&probe).is_err(), "lock must be held");

        drop(guard);
        assert!(FileExt::try_lock(&probe).is_ok(), "lock must be free after drop");
        FileExt::unlock(&probe).unwrap();
    }
}
