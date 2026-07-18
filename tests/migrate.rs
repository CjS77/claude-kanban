//! The v1→v2 schema upgrade as the real binary performs it at startup. Every face resolves the store and upgrades before
//! dispatching, so this drives the two that matter to a user with an old board: `mcp` (what `/kanban:work` talks to) and
//! the `worktree` CLI. The board must come back at schema 2 with the original preserved beside it.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

/// A board as v1 wrote it: no `schema`, three columns, a done ticket carrying no `discarded` flag.
const V1_BOARD: &str = r#"{
  "version": 7,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "done", "title": "Done" }
  ],
  "epics": [],
  "tickets": [
    {
      "id": "K-1",
      "title": "Old finished work",
      "status": "ready",
      "column": { "id": "done", "branch": "k-1/old", "completed_at": "2026-07-14T09:12:00Z" }
    }
  ]
}"#;

/// A store directory holding exactly a v1 `board.json`.
fn v1_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join(".kanban");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("board.json"), V1_BOARD).unwrap();
    (dir, store)
}

/// Assert the store was upgraded on disk: schema 2, the review column present, the ticket untouched, and the original
/// kept byte-for-byte as `board-v1.json`.
fn assert_migrated(store: &std::path::Path) {
    let backup = std::fs::read_to_string(store.join("board-v1.json")).expect("the original must be preserved");
    assert_eq!(backup, V1_BOARD, "the backup is the original bytes, verbatim");

    let raw = std::fs::read_to_string(store.join("board.json")).unwrap();
    let board: serde_json::Value = serde_json::from_str(&raw).expect("the migrated board must be intact JSON");
    assert_eq!(board["schema"], 2, "the upgrade is on disk, not just in memory: {raw}");
    assert_eq!(board["version"], 8, "the upgrade counts as exactly one mutation");

    let columns: Vec<&str> = board["columns"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(columns, ["todo", "doing", "review", "done"], "review slots in before done");
    assert_eq!(board["tickets"][0]["column"]["id"], "done", "a v1 done ticket stays done");
}

#[test]
fn the_mcp_face_migrates_a_v1_board_on_disk() {
    let (_dir, store) = v1_store();

    let mut child = Command::new(env!("CARGO_BIN_EXE_claude-kanban"))
        .args(["--store", store.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("binary must start");

    // The initialize round trip is the synchronisation point: a reply means startup, and so the upgrade, is done.
    let mut stdin = child.stdin.take().unwrap();
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "migrate", "version": "0" } }
    });
    writeln!(stdin, "{init}").unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).expect("the reply must be protocol JSON");
    assert!(reply["result"]["capabilities"]["tools"].is_object(), "the server must have come up: {reply}");

    assert_migrated(&store);

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_worktree_cli_migrates_a_v1_board_on_disk() {
    let (dir, store) = v1_store();
    // `worktree list` insists on a repo around the store, so give it one.
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir.path()).output().expect("git must run");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "--quiet"]);

    let out = Command::new(env!("CARGO_BIN_EXE_claude-kanban"))
        .args(["--store", store.to_str().unwrap(), "worktree", "list", "--json"])
        .output()
        .expect("binary must run");
    assert!(out.status.success(), "worktree list must succeed: {}", String::from_utf8_lossy(&out.stderr));

    assert_migrated(&store);
}

#[test]
fn a_corrupt_board_is_refused_on_stderr_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join(".kanban");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("board.json"), "not json").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_claude-kanban"))
        .args(["--store", store.to_str().unwrap(), "worktree", "list", "--json"])
        .output()
        .expect("binary must run");

    assert!(!out.status.success(), "a board that cannot be parsed must not be papered over");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("board.json"), "the failure names the file, on stderr: {stderr}");
    assert!(out.stdout.is_empty(), "stdout belongs to the protocol — errors never go there");
    assert_eq!(std::fs::read_to_string(store.join("board.json")).unwrap(), "not json", "the board is untouched");
    assert!(!store.join("board-v1.json").exists(), "an unreadable board is never backed up");
}
