//! Cross-process behavior of `claude-kanban init`: two racing processes must produce exactly one board — one winner, one
//! clean "already exists" refusal, never a clobber. This drives the real binary, so it exercises the same advisory-lock path
//! two concurrent Claude/browser processes use.

use std::process::Command;

fn init(store: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_claude-kanban"))
        .args(["--store", store.to_str().unwrap(), "init"])
        .output()
        .expect("binary must run")
}

#[test]
fn two_inits_produce_exactly_one_board() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join(".kanban");

    let (a, b) = std::thread::scope(|s| {
        let ta = s.spawn(|| init(&store));
        let tb = s.spawn(|| init(&store));
        (ta.join().unwrap(), tb.join().unwrap())
    });

    let successes = [&a, &b].iter().filter(|o| o.status.success()).count();
    assert_eq!(successes, 1, "exactly one init may win.\nA: {a:?}\nB: {b:?}");

    let loser = [&a, &b].into_iter().find(|o| !o.status.success()).unwrap();
    let stderr = String::from_utf8_lossy(&loser.stderr);
    assert!(stderr.contains("already exists"), "the loser must refuse cleanly, got: {stderr}");

    let board = std::fs::read_to_string(store.join("board.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&board).expect("the surviving board must be intact JSON");
    assert_eq!(parsed["version"], 0);
}
