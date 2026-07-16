//! `.kanban/config.json` — the store's optional, committed configuration.
//!
//! All keys are optional and the file itself may be absent. Settings that also exist as flags or environment variables
//! resolve as **flag beats env beats config** — clap merges flag and env, so helpers here only arbitrate against the config.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::store::StoreError;

/// Default worktree root when nothing overrides it. Deliberately volatile: worktrees are meant to be expendable —
/// every commit, the branch, the claim, and the card all survive a wipe.
pub const DEFAULT_WORKTREE_ROOT: &str = "/tmp/claude-kanban";

/// The config file's shape. Unknown keys are ignored (the file is hand-written; a typo shouldn't brick the tool — though it
/// also won't warn. Keep the schema small.)
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where ticket worktrees are created: `<worktree_root>/<repo-name>-<hash>/<ticket-id>`.
    pub worktree_root: Option<PathBuf>,
    /// Gitignored files (relative to the repo root) to copy into each new worktree — `.env`, local certs. Only files git
    /// actually ignores are copied; anything else is skipped with a warning, so the config can't smuggle tracked files.
    pub copy_to_worktrees: Vec<String>,
}

impl Config {
    /// Load the config from `store_dir`, or defaults when the file is absent. A malformed file is an error — silently
    /// ignoring a broken config would be worse than stopping.
    pub fn load(store_dir: &Path) -> Result<Config, StoreError> {
        crate::store::read_json_or_default(&store_dir.join("config.json"))
    }

    /// The effective worktree root: `--dir` flag / `KANBAN_WORKTREE_DIR` (already merged by clap) > config > default.
    #[must_use] 
    pub fn worktree_root(&self, flag: Option<PathBuf>) -> PathBuf {
        flag.or_else(|| self.worktree_root.clone()).unwrap_or_else(|| PathBuf::from(DEFAULT_WORKTREE_ROOT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(cfg.worktree_root.is_none());
        assert!(cfg.copy_to_worktrees.is_empty());
    }

    #[test]
    fn precedence_is_flag_then_config_then_default() {
        let cfg = Config { worktree_root: Some(PathBuf::from("/from/config")), copy_to_worktrees: vec![] };
        assert_eq!(cfg.worktree_root(Some(PathBuf::from("/from/flag"))), PathBuf::from("/from/flag"));
        assert_eq!(cfg.worktree_root(None), PathBuf::from("/from/config"));
        assert_eq!(Config::default().worktree_root(None), PathBuf::from(DEFAULT_WORKTREE_ROOT));
    }

    #[test]
    fn a_malformed_config_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{ worktree_root: oops }").unwrap();
        assert!(Config::load(dir.path()).is_err());
    }

    #[test]
    fn a_valid_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{ "worktree_root": "/data/wt", "copy_to_worktrees": [".env"] }"#).unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.worktree_root.unwrap(), PathBuf::from("/data/wt"));
        assert_eq!(cfg.copy_to_worktrees, vec![".env"]);
    }
}
