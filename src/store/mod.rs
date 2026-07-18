//! The store: one board, two files, one write discipline.
//!
//! `.kanban/board.json` (committed) holds the durable board; `.kanban/claims.json` (gitignored) holds machine-local live
//! claims. Every mutation — from the HTTP server, the MCP server, or the worktree CLI, possibly in different processes —
//! goes through [`Store::mutate`]: take the cross-process advisory lock, read fresh state, check the caller's expected
//! version, apply, validate, bump the version, and write atomically. A failing step leaves both files untouched.
//!
//! Reads never lock: writes replace files by rename, so a reader always sees a complete file (see [`io`]).
//!
//! # Where the store lives
//!
//! The store always resolves to the **main working tree's** `.kanban/`, wherever the process runs — the binary asks git
//! rather than trusting its own working directory, so a process launched deep inside a ticket worktree still reads and
//! writes the one true board. `--store` / `KANBAN_STORE` remain as explicit overrides, and outside a git repo the store
//! falls back to `./.kanban`.

mod claims;
pub mod derive;
mod io;
mod lock;
pub mod model;
mod validate;

use std::{
    fs,
    path::{Path, PathBuf},
};

pub use claims::{Claim, find as find_claim, remove as remove_claim, upsert as upsert_claim};
use model::Board;

/// Write a seed file, keeping whatever is already there — `init` seeds defaults, it never overrules a choice.
fn seed_if_absent(path: &Path, contents: &str) -> Result<(), StoreError> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents).map_err(|source| StoreError::Io { path: path.to_path_buf(), source })
}

/// Lenient JSON read for sibling store files (e.g. `config.json`): the default value when the file is absent, an error when
/// it exists but doesn't parse.
pub(crate) fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T, StoreError> {
    io::read_json(path).map(Option::unwrap_or_default)
}

/// File name of the board inside the store directory.
const BOARD_FILE: &str = "board.json";
/// File name of the live-claims sidecar inside the store directory.
const CLAIMS_FILE: &str = "claims.json";
/// File name of the store config inside the store directory.
pub(crate) const CONFIG_FILE: &str = "config.json";
/// Store-local gitignore written by `init`, covering the runtime artifacts (never the board itself).
const STORE_GITIGNORE: &str = "# claude-kanban runtime artifacts — machine-local, never committed. (board.json and this file ARE committed.)\n.lock\n*.tmp\nserve.pid\nclaims.json\nland-state.json\n";

/// Store-local config written by `init`, and committed like the board. Every dial appears explicitly at its default so
/// the file documents itself. `port` is `null`: an explicit port fails loudly when taken, while no choice means "try
/// 4747, then hunt". `main_branch` is detected from the surrounding repo and pinned concretely, falling back to
/// `"main"` outside one. Strict JSON — [`crate::config::Config`] is serde-parsed and a malformed file is a loud error,
/// so this cannot carry comments.
fn seeded_config(store_dir: &Path) -> String {
    let main = store_dir.parent().and_then(crate::git::detect_main_branch).unwrap_or_else(|| "main".to_owned());
    format!(
        "{{\n  \"worktree_root\": {root},\n  \"copy_to_worktrees\": [],\n  \"max_workers\": 1,\n  \"idle_time\": 300,\n  \
         \"port\": null,\n  \"main_branch\": {main},\n  \"poll_interval\": 60\n}}\n",
        root = serde_json::json!(crate::config::DEFAULT_WORKTREE_ROOT),
        main = serde_json::json!(main),
    )
}

/// Everything that can go wrong below the operation layer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no board at {} — run `claude-kanban init` first", .0.display())]
    NotInitialized(PathBuf),
    #[error("{} already exists — refusing to overwrite an existing board", .0.display())]
    AlreadyExists(PathBuf),
    #[error("board changed underneath this write: version is {actual}, expected {expected}")]
    VersionConflict { expected: u64, actual: u64 },
    #[error("{} was written by a newer claude-kanban (schema {found}; this binary understands {supported}) — update the plugin", path.display())]
    SchemaTooNew { path: PathBuf, found: u32, supported: u32 },
    #[error("invalid board in {}:\n  - {problems}", path.display())]
    Invalid { path: PathBuf, problems: String },
    #[error("failed to parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A resolved store location. Cheap to clone; holds no open files — locks are taken per mutation.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Resolve where the store lives: explicit override (`--store` / `KANBAN_STORE`, merged by clap) beats the git anchor
    /// (the main working tree's `.kanban/`) beats `./.kanban` outside a repo.
    #[must_use] 
    pub fn resolve(flag: Option<PathBuf>) -> Store {
        let dir = flag
            .or_else(|| crate::git::main_worktree(Path::new(".")).map(|root| root.join(".kanban")))
            .unwrap_or_else(|| PathBuf::from(".kanban"));
        Store { dir }
    }

    /// A store at exactly `dir`, no resolution. Tests and explicit callers only.
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    /// The store directory itself.
    #[must_use] 
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of `board.json`.
    #[must_use] 
    pub fn board_path(&self) -> PathBuf {
        self.dir.join(BOARD_FILE)
    }

    /// Path of the claims sidecar.
    #[must_use] 
    pub fn claims_path(&self) -> PathBuf {
        self.dir.join(CLAIMS_FILE)
    }

    /// Path of the store config.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// Create the store directory, seed an empty board, a default `config.json`, and a store-local `.gitignore` covering
    /// the runtime artifacts. Refuses to touch an existing board; an existing config or gitignore is left alone, so a
    /// hand-edited one survives. Runs under the lock so two racing `init`s can't both win.
    pub fn init(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir).map_err(|source| StoreError::Io { path: self.dir.clone(), source })?;
        let _lock = lock::acquire(&self.dir)?;
        if self.board_path().exists() {
            return Err(StoreError::AlreadyExists(self.board_path()));
        }
        io::write_json_atomic(&self.board_path(), &Board::empty())?;
        seed_if_absent(&self.config_path(), &seeded_config(&self.dir))?;
        seed_if_absent(&self.dir.join(".gitignore"), STORE_GITIGNORE)?;
        tracing::info!(path = %self.board_path().display(), "board initialised");
        Ok(())
    }

    /// Read, migrate (in memory — the next mutation persists the upgrade), and validate the board. Lock-free — see the
    /// module docs. A board written by a *newer* binary refuses to load: misreading it would be worse than stopping.
    pub fn read_board(&self) -> Result<Board, StoreError> {
        let mut board: Board =
            io::read_json(&self.board_path())?.ok_or_else(|| StoreError::NotInitialized(self.board_path()))?;
        model::migrate(&mut board).map_err(|found| StoreError::SchemaTooNew {
            path: self.board_path(),
            found,
            supported: model::CURRENT_SCHEMA,
        })?;
        validate::validate(&board).map_err(|problems| StoreError::Invalid {
            path: self.board_path(),
            problems: problems.join("\n  - "),
        })?;
        Ok(board)
    }

    /// Read the live claims. A missing sidecar is an empty one. Lock-free.
    pub fn read_claims(&self) -> Result<Vec<Claim>, StoreError> {
        Ok(io::read_json(&self.claims_path())?.unwrap_or_default())
    }

    /// The one write cycle: lock → read fresh state → check `expected_version` → run `f` → validate → bump version →
    /// atomic write. Returns `f`'s output and the new version.
    ///
    /// `expected_version` is the optimistic-concurrency check: `Some(v)` rejects the write with
    /// [`StoreError::VersionConflict`] if the board on disk is no longer at `v` — the caller was looking at a stale board.
    /// `None` skips the check for server-derived mutations that don't depend on a client's view.
    ///
    /// The claims sidecar is rewritten only when `f` actually changed it; the board is always rewritten (its version bumps
    /// every time). Any error out of `f` or validation leaves both files byte-identical.
    pub fn mutate<T, E>(
        &self,
        expected_version: Option<u64>,
        f: impl FnOnce(&mut Board, &mut Vec<Claim>) -> Result<T, E>,
    ) -> Result<(T, u64), E>
    where
        E: From<StoreError>,
    {
        let _lock = lock::acquire(&self.dir)?;
        let mut board = self.read_board()?;
        if let Some(expected) = expected_version
            && board.version != expected
        {
            tracing::warn!(expected, actual = board.version, "version conflict — the caller acted on a stale board");
            return Err(StoreError::VersionConflict { expected, actual: board.version }.into());
        }
        let mut claims = self.read_claims()?;
        let claims_before = claims.clone();

        let out = f(&mut board, &mut claims)?;

        validate::validate(&board).map_err(|problems| StoreError::Invalid {
            path: self.board_path(),
            problems: problems.join("\n  - "),
        })?;
        board.version += 1;
        let claims_changed = claims != claims_before;
        io::write_json_atomic(&self.board_path(), &board)?;
        if claims_changed {
            io::write_json_atomic(&self.claims_path(), &claims)?;
        }
        tracing::debug!(version = board.version, claims_changed, "store written");
        Ok((out, board.version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{Column, Status, Ticket, TicketId};

    fn scratch_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join(".kanban"));
        store.init().unwrap();
        (dir, store)
    }

    fn some_ticket(id: &str) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: "a ticket".into(),
            epic: None,
            status: Status::Ready,
            body: String::new(),
            labels: vec![],
            depends_on: vec![],
            notes: vec![],
            external: None,
            pr: None,
            column: Column::Todo,
        }
    }

    #[test]
    fn init_seeds_an_empty_board_and_refuses_to_run_twice() {
        let (_dir, store) = scratch_store();
        let board = store.read_board().unwrap();
        assert_eq!(board.version, 0);
        assert_eq!(board.columns.len(), 4);
        assert!(matches!(store.init(), Err(StoreError::AlreadyExists(_))));
        assert!(store.dir().join(".gitignore").exists());

        assert!(store.config_path().exists(), "init seeds a config beside the board");
        let config = crate::config::Config::load(store.dir()).expect("the seeded config must parse");
        assert_eq!(config.max_workers(), 1);
        assert_eq!(config.idle_time(), 300);
        assert!(config.port.is_none(), "port seeds as null: that is what lets serve try 4747 and then hunt");
        assert_eq!(config.poll_interval(), Some(std::time::Duration::from_secs(60)));

        // The seeded file is fully defined: every dial present, port explicitly null, main_branch pinned concretely.
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(store.config_path()).unwrap()).unwrap();
        let obj = raw.as_object().unwrap();
        let keys = ["worktree_root", "copy_to_worktrees", "max_workers", "idle_time", "port", "main_branch", "poll_interval"];
        let missing: Vec<_> = keys.iter().filter(|key| !obj.contains_key(**key)).collect();
        assert!(missing.is_empty(), "seeded config missing {missing:?}");
        assert!(obj["port"].is_null());
        assert_eq!(obj["main_branch"], "main", "no surrounding repo in this test: the fallback is 'main'");

        let gitignore = std::fs::read_to_string(store.dir().join(".gitignore")).unwrap();
        assert!(gitignore.contains("land-state.json"), "the landing observations sidecar is machine-local");
    }

    #[test]
    fn init_leaves_an_existing_config_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join(".kanban"));
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.config_path(), r#"{ "max_workers": 4, "port": 5050 }"#).unwrap();

        store.init().unwrap();

        let config = crate::config::Config::load(store.dir()).unwrap();
        assert_eq!(config.max_workers(), 4, "a hand-edited config survives init");
        assert_eq!(config.port, Some(5050));
    }

    #[test]
    fn a_v1_board_file_loads_migrated_and_disk_changes_only_on_the_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join(".kanban"));
        std::fs::create_dir_all(store.dir()).unwrap();
        let v1 = r#"{
  "version": 7,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "done", "title": "Done" }
  ],
  "epics": [],
  "tickets": []
}"#;
        std::fs::write(store.board_path(), v1).unwrap();

        let board = store.read_board().unwrap();
        assert_eq!(board.schema, crate::store::model::CURRENT_SCHEMA);
        assert_eq!(board.columns.len(), 4, "review arrives in memory");
        assert_eq!(std::fs::read_to_string(store.board_path()).unwrap(), v1, "a read never rewrites the file");

        store.mutate(None, |_, _| Ok::<_, StoreError>(())).unwrap();
        let raw = std::fs::read_to_string(store.board_path()).unwrap();
        assert!(raw.contains("\"schema\": 2") && raw.contains("\"review\""), "the first write persists the upgrade: {raw}");
    }

    #[test]
    fn a_board_from_a_newer_binary_refuses_to_load_with_update_advice() {
        let (_dir, store) = scratch_store();
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.board_path()).unwrap()).unwrap();
        value["schema"] = serde_json::json!(9);
        std::fs::write(store.board_path(), value.to_string()).unwrap();

        let err = store.read_board().unwrap_err();
        assert!(matches!(err, StoreError::SchemaTooNew { found: 9, .. }), "{err}");
        assert!(err.to_string().contains("update the plugin"), "{err}");
    }

    #[test]
    fn reading_an_uninitialised_store_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join(".kanban"));
        assert!(matches!(store.read_board(), Err(StoreError::NotInitialized(_))));
        assert!(store.read_claims().unwrap().is_empty(), "missing claims file reads as empty");
    }

    #[test]
    fn mutate_bumps_the_version_and_persists() {
        let (_dir, store) = scratch_store();
        let ((), v) = store
            .mutate(None, |board, _| {
                board.tickets.push(some_ticket("K-1"));
                Ok::<_, StoreError>(())
            })
            .unwrap();
        assert_eq!(v, 1);
        let board = store.read_board().unwrap();
        assert_eq!(board.version, 1);
        assert_eq!(board.tickets.len(), 1);
    }

    #[test]
    fn a_stale_expected_version_is_rejected() {
        let (_dir, store) = scratch_store();
        store.mutate(Some(0), |_, _| Ok::<_, StoreError>(())).unwrap();
        let err = store.mutate(Some(0), |_, _| Ok::<_, StoreError>(())).unwrap_err();
        assert!(matches!(err, StoreError::VersionConflict { expected: 0, actual: 1 }), "{err}");
    }

    #[test]
    fn a_failing_mutation_leaves_both_files_byte_identical() {
        let (_dir, store) = scratch_store();
        let board_before = std::fs::read(store.board_path()).unwrap();
        // Failure via the closure…
        let err = store.mutate(None, |_, _| Err::<(), _>(StoreError::NotInitialized(store.board_path()))).unwrap_err();
        assert!(matches!(err, StoreError::NotInitialized(_)));
        // …and failure via validation (a dangling dependency), even though the closure "succeeded" and touched claims.
        let err = store
            .mutate(None, |board, claims| {
                let mut t = some_ticket("K-1");
                t.depends_on = vec![TicketId("K-99".into())];
                board.tickets.push(t);
                claims.push(Claim { ticket: TicketId("K-1".into()), agent: "x".into(), since: chrono::Utc::now(), path: None });
                Ok::<_, StoreError>(())
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid { .. }), "{err}");
        assert_eq!(std::fs::read(store.board_path()).unwrap(), board_before);
        assert!(!store.claims_path().exists(), "claims must not be written when the mutation fails");
    }

    #[test]
    fn claims_are_only_rewritten_when_changed() {
        let (_dir, store) = scratch_store();
        store.mutate(None, |_, _| Ok::<_, StoreError>(())).unwrap();
        assert!(!store.claims_path().exists(), "an untouched sidecar is not created");
        store
            .mutate(None, |_, claims| {
                claims.push(Claim { ticket: TicketId("K-1".into()), agent: "claude".into(), since: chrono::Utc::now(), path: None });
                Ok::<_, StoreError>(())
            })
            .unwrap();
        assert_eq!(store.read_claims().unwrap().len(), 1);
    }

    #[test]
    fn contending_writers_never_lose_an_update() {
        // Each mutate opens its own lock file handle, so threads exercise the same flock path distinct processes use.
        let (_dir, store) = scratch_store();
        let threads = 8;
        let writes_per_thread = 10;
        std::thread::scope(|s| {
            (0..threads).for_each(|_| {
                let store = store.clone();
                s.spawn(move || {
                    (0..writes_per_thread).for_each(|_| {
                        store.mutate(None, |_, _| Ok::<_, StoreError>(())).unwrap();
                    });
                });
            });
        });
        assert_eq!(store.read_board().unwrap().version, threads * writes_per_thread, "every write must be serialized, none lost");
    }

    #[test]
    fn hand_edited_garbage_fails_loudly_with_every_problem_listed() {
        let (_dir, store) = scratch_store();
        store
            .mutate(None, |board, _| {
                board.tickets.push(some_ticket("K-1"));
                Ok::<_, StoreError>(())
            })
            .unwrap();
        // A hand edit introduces two problems at once; both must be reported.
        let text = std::fs::read_to_string(store.board_path()).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["tickets"][0]["depends_on"] = serde_json::json!(["K-404"]);
        value["tickets"][0]["epic"] = serde_json::json!("EP-404");
        std::fs::write(store.board_path(), value.to_string()).unwrap();
        let err = store.read_board().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("K-404") && msg.contains("EP-404"), "all problems in one report: {msg}");
    }
}
