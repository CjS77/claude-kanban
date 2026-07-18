//! `.kanban/config.json` — the store's optional, committed configuration.
//!
//! All keys are optional and the file itself may be absent. Settings that also exist as flags or environment variables
//! resolve as **flag beats env beats config** — clap merges flag and env, so helpers here only arbitrate against the config.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::store::StoreError;

/// Default worktree root when nothing overrides it. Deliberately volatile: worktrees are meant to be expendable —
/// every commit, the branch, the claim, and the card all survive a wipe.
pub const DEFAULT_WORKTREE_ROOT: &str = "/tmp/claude-kanban";

/// How long `/kanban:work` idles between polls when the board has nothing eligible: 5 minutes.
pub const DEFAULT_IDLE_TIME_SECS: u64 = 300;

/// How often `serve` runs the landing sweep and gh PR poll when `poll_interval` says nothing: 1 minute.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// The config file's shape. Unknown keys are ignored (the file is hand-written; a typo shouldn't brick the tool — though it
/// also won't warn. Keep the schema small.)
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where ticket worktrees are created: `<worktree_root>/<repo-name>-<hash>/<ticket-id>`.
    pub worktree_root: Option<PathBuf>,
    /// Gitignored files (relative to the repo root) to copy into each new worktree — `.env`, local certs. Only files git
    /// actually ignores are copied; anything else is skipped with a warning, so the config can't smuggle tracked files.
    pub copy_to_worktrees: Vec<String>,
    /// How many tickets `/kanban:work` may drive concurrently. Absent or `0` means `1` (the sequential loop). Config-only:
    /// no flag or env var.
    pub max_workers: Option<usize>,
    /// How many **seconds** `/kanban:work` sleeps when nothing is eligible before polling the board again. Absent or `0`
    /// means 300 (5 minutes) — `0` collapses to the default so a typo can't spin a hot loop. Config-only: no flag or env var.
    pub idle_time: Option<u64>,
    /// Port for `serve`. An explicit port — here, `--port`, or `KANBAN_PORT` — is honoured or fails loudly; when none is
    /// given, `serve` tries 4747 and hunts for a free port if another project holds it.
    pub port: Option<u16>,
    /// The integration branch that defines "landed": a review ticket moves to done when its code reaches this branch
    /// *locally*. Absent → auto-detect (what `origin/HEAD` names, else a local `main`, else `master`).
    pub main_branch: Option<String>,
    /// **Seconds** between `serve`'s landing sweeps and gh PR polls. Absent → 60. `0` disables polling entirely —
    /// unlike `idle_time`, this dial needs a real off switch, so zero is honoured rather than collapsed to the default.
    pub poll_interval: Option<u64>,
}

impl Config {
    /// Load the config from `store_dir`, or defaults when the file is absent. A malformed file is an error — silently
    /// ignoring a broken config would be worse than stopping.
    pub fn load(store_dir: &Path) -> Result<Config, StoreError> {
        crate::store::read_json_or_default(&store_dir.join(crate::store::CONFIG_FILE))
    }

    /// The effective worktree root: `--dir` flag / `KANBAN_WORKTREE_DIR` (already merged by clap) > config > default.
    #[must_use]
    pub fn worktree_root(&self, flag: Option<PathBuf>) -> PathBuf {
        flag.or_else(|| self.worktree_root.clone()).unwrap_or_else(|| PathBuf::from(DEFAULT_WORKTREE_ROOT))
    }

    /// The effective worker count for `/kanban:work`: absent or `0` collapses to `1`.
    #[must_use]
    pub fn max_workers(&self) -> usize {
        self.max_workers.filter(|&n| n > 0).unwrap_or(1)
    }

    /// The effective idle sleep for `/kanban:work`, in seconds: absent or `0` collapses to [`DEFAULT_IDLE_TIME_SECS`].
    #[must_use]
    pub fn idle_time(&self) -> u64 {
        self.idle_time.filter(|&n| n > 0).unwrap_or(DEFAULT_IDLE_TIME_SECS)
    }

    /// The explicitly chosen serve port, if any: `--port` flag / `KANBAN_PORT` (already merged by clap) > config.
    /// `None` means nobody chose — the server tries the default port and hunts instead of failing.
    #[must_use]
    pub fn port(&self, flag: Option<u16>) -> Option<u16> {
        flag.or(self.port)
    }

    /// The effective integration branch: config beats [`crate::git::detect_main_branch`]. `None` — nothing configured
    /// and nothing detectable — degrades landing detection to a no-op.
    #[must_use]
    pub fn main_branch(&self, repo: &Path) -> Option<String> {
        self.main_branch.clone().or_else(|| crate::git::detect_main_branch(repo))
    }

    /// The effective poll cadence for `serve`: absent → [`DEFAULT_POLL_INTERVAL_SECS`]; `0` → `None`, polling off.
    #[must_use]
    pub fn poll_interval(&self) -> Option<Duration> {
        match self.poll_interval {
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
            None => Some(Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)),
        }
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
        let cfg = Config { worktree_root: Some(PathBuf::from("/from/config")), ..Config::default() };
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

    #[test]
    fn max_workers_absent_and_zero_collapse_to_one() {
        assert_eq!(Config::default().max_workers(), 1);
        assert_eq!(Config { max_workers: Some(0), ..Config::default() }.max_workers(), 1);
        assert_eq!(Config { max_workers: Some(4), ..Config::default() }.max_workers(), 4);
    }

    #[test]
    fn max_workers_parses_from_config_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{ "max_workers": 3 }"#).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap().max_workers(), 3);
    }

    #[test]
    fn idle_time_absent_and_zero_collapse_to_five_minutes() {
        assert_eq!(Config::default().idle_time(), DEFAULT_IDLE_TIME_SECS);
        assert_eq!(Config { idle_time: Some(0), ..Config::default() }.idle_time(), DEFAULT_IDLE_TIME_SECS);
        assert_eq!(Config { idle_time: Some(30), ..Config::default() }.idle_time(), 30);
    }

    #[test]
    fn idle_time_parses_from_config_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{ "idle_time": 60 }"#).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap().idle_time(), 60);
    }

    #[test]
    fn poll_interval_defaults_to_a_minute_and_zero_switches_it_off() {
        assert_eq!(Config::default().poll_interval(), Some(Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)));
        assert_eq!(Config { poll_interval: Some(0), ..Config::default() }.poll_interval(), None, "0 is the off switch");
        assert_eq!(Config { poll_interval: Some(15), ..Config::default() }.poll_interval(), Some(Duration::from_secs(15)));
    }

    #[test]
    fn main_branch_config_beats_detection() {
        // The tempdir is no git repo, so detection yields nothing — config is the only voice.
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { main_branch: Some("trunk".into()), ..Config::default() };
        assert_eq!(cfg.main_branch(dir.path()).as_deref(), Some("trunk"));
        assert_eq!(Config::default().main_branch(dir.path()), None, "no config, no repo: no answer");
    }

    #[test]
    fn config_serializes_every_key_for_the_settings_pane() {
        let json = serde_json::to_value(Config::default()).unwrap();
        let obj = json.as_object().unwrap();
        let keys = ["worktree_root", "copy_to_worktrees", "max_workers", "idle_time", "port", "main_branch", "poll_interval"];
        let missing: Vec<_> = keys.iter().filter(|key| !obj.contains_key(**key)).collect();
        assert!(missing.is_empty(), "missing {missing:?}");
        assert!(obj["port"].is_null(), "unset options serialize as null, preserving 'nobody chose'");
    }

    #[test]
    fn port_precedence_is_flag_env_then_config_then_nobody() {
        let cfg = Config { port: Some(5050), ..Config::default() };
        assert_eq!(cfg.port(Some(8080)), Some(8080), "flag/env (merged by clap) beats config");
        assert_eq!(cfg.port(None), Some(5050), "config beats the default");
        assert_eq!(Config::default().port(None), None, "None means: try 4747, hunt on AddrInUse");
    }
}
