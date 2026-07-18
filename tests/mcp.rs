//! MCP smoke test: drive the real binary's `mcp` subcommand over stdio with raw JSON-RPC frames — the same wire Claude
//! Code speaks. Covers initialize, tools/list, a read tool, a mutating tool, and the version-conflict recovery message.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
};

use serde_json::{Value, json};

struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(store: &std::path::Path) -> McpSession {
        let mut child = Command::new(env!("CARGO_BIN_EXE_claude-kanban"))
            .args(["--store", store.to_str().unwrap(), "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("binary must start");
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut session = McpSession { child, reader };

        let init = session.request(
            1,
            "initialize",
            &json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "smoke", "version": "0" } }),
        );
        assert!(init["result"]["capabilities"]["tools"].is_object(), "server must advertise tools: {init}");
        let instructions = init["result"]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("kanban_next"), "instructions must carry the workflow contract");
        assert!(
            instructions.contains("kanban_move to review") && instructions.contains("Done is not yours to declare"),
            "instructions must carry the v2 close-out contract: {instructions}"
        );
        session.notify("notifications/initialized", &json!({}));
        session
    }

    fn send(&mut self, value: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "server closed the stream waiting for a reply to {method}");
            let value: Value = serde_json::from_str(&line).expect("every stdout line must be protocol JSON");
            if value["id"] == json!(id) {
                return value;
            }
        }
    }

    fn call_tool(&mut self, id: u64, name: &str, args: &Value) -> Value {
        let res = self.request(id, "tools/call", &json!({ "name": name, "arguments": args }));
        assert!(res["error"].is_null(), "tools/call {name} must not be a protocol error: {res}");
        res["result"].clone()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_mcp_face_reads_claims_and_recovers_from_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join(".kanban");
    let store = claude_kanban::store::Store::at(&store_dir);
    store.init().unwrap();

    let mut mcp = McpSession::start(&store_dir);

    // The advertised tool set.
    let tools = mcp.request(2, "tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in ["kanban_board", "kanban_next", "kanban_claim", "kanban_release", "kanban_move", "kanban_create_ticket", "kanban_create_epic", "kanban_note", "kanban_refine"] {
        assert!(names.contains(&expected), "missing tool {expected}; got {names:?}");
    }

    // Create a ready ticket (explicit status overrides the review default), then read the board.
    let created = mcp.call_tool(3, "kanban_create_ticket", &json!({ "title": "From Claude", "status": "ready", "expected_version": 0 }));
    assert_eq!(created["structuredContent"]["id"], "K-1", "{created}");
    assert_eq!(created["structuredContent"]["version"], 1);

    let board = mcp.call_tool(4, "kanban_board", &json!({}));
    assert_eq!(board["structuredContent"]["tickets"][0]["title"], "From Claude");
    assert_eq!(board["structuredContent"]["max_workers"], 1, "unconfigured board defaults to one worker: {board}");
    assert_eq!(board["structuredContent"]["idle_time"], 300, "unconfigured board defaults to a five-minute idle: {board}");

    // max_workers and idle_time come from config.json at read time — no server restart needed.
    std::fs::write(store_dir.join("config.json"), r#"{ "max_workers": 2, "idle_time": 60 }"#).unwrap();
    let board = mcp.call_tool(40, "kanban_board", &json!({}));
    assert_eq!(board["structuredContent"]["max_workers"], 2, "{board}");
    assert_eq!(board["structuredContent"]["idle_time"], 60, "{board}");

    // kanban_next nominates it; claim it; next then reports nothing eligible.
    let next = mcp.call_tool(5, "kanban_next", &json!({}));
    assert_eq!(next["structuredContent"]["ticket"]["id"], "K-1");
    let claimed = mcp.call_tool(6, "kanban_claim", &json!({ "ticket": "K-1", "expected_version": 1 }));
    assert_eq!(claimed["structuredContent"]["owner"], "claude");
    let next = mcp.call_tool(7, "kanban_next", &json!({}));
    assert!(next["structuredContent"]["ticket"].is_null(), "{next}");

    // A stale expected_version is a TOOL error (Claude reads it), naming the current version.
    let conflicted = mcp.call_tool(8, "kanban_move", &json!({ "ticket": "K-1", "to": "done", "expected_version": 0 }));
    assert_eq!(conflicted["isError"], true, "{conflicted}");
    let text = conflicted["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("expected_version=2"), "the recovery instruction names the live version: {text}");

    // Tickets created without an explicit status land in review — the human's gate.
    let vetted = mcp.call_tool(9, "kanban_create_ticket", &json!({ "title": "Needs vetting", "expected_version": 2 }));
    assert_eq!(vetted["structuredContent"]["id"], "K-2");
    let board = store.read_board().unwrap();
    assert_eq!(board.ticket(&claude_kanban::store::model::TicketId("K-2".into())).unwrap().status, claude_kanban::store::model::Status::Review);

    // And a refine splits atomically over MCP.
    let refined = mcp.call_tool(
        10,
        "kanban_refine",
        &json!({
            "target": "K-2", "body": "# Real spec", "expected_version": 3,
            "split_tickets": [ { "title": "part one" }, { "title": "part two", "depends_on": ["new:0"] } ]
        }),
    );
    assert_eq!(refined["structuredContent"]["created"], json!(["K-3", "K-4"]), "{refined}");
}

/// A board with one landed ticket carrying a fat spec and a progress log — the shape that dominates a real board — plus
/// one live ticket to prove the default read still answers with the work that matters. Returns the store directory.
fn seed_a_fat_done_ticket(dir: &std::path::Path) -> std::path::PathBuf {
    use claude_kanban::{
        ops::{self, Op},
        store::model::{ColumnId, Status, TicketId},
    };

    let store_dir = dir.join(".kanban");
    let store = claude_kanban::store::Store::at(&store_dir);
    store.init().unwrap();

    let create = |title: &str, body: String| Op::CreateTicket {
        title: title.into(),
        body,
        epic: None,
        labels: vec![],
        depends_on: vec![],
        status: Status::Ready,
    };
    ops::apply(&store, None, create("landed work", "## Refined spec\n".to_owned() + &"every decision, recorded. ".repeat(600))).unwrap();
    let k1 = TicketId("K-1".into());
    let note = |text: &str| Op::AddNote { id: k1.clone(), text: text.into(), author: Some("claude".into()) };
    ops::apply(&store, None, note("claimed it, starting on the parser")).unwrap();
    ops::apply(&store, None, note("parser done, wiring the views up next")).unwrap();
    ops::apply(&store, None, Op::MoveTicket { id: k1, to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();
    ops::apply(&store, None, create("still to do", "short".into())).unwrap();
    store_dir
}

#[test]
fn kanban_board_omits_done_by_default_and_returns_it_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = seed_a_fat_done_ticket(dir.path());
    let mut mcp = McpSession::start(&store_dir);

    let ids = |board: &Value| -> Vec<String> {
        board["structuredContent"]["tickets"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect()
    };

    // The default: the done ticket is gone, and a summary says so and names the way back.
    let default = mcp.call_tool(2, "kanban_board", &json!({}));
    assert_eq!(ids(&default), ["K-2"], "the default read carries only unfinished work: {default}");
    let done = &default["structuredContent"]["done"];
    assert_eq!(done["count"], 1, "the summary accounts for the omitted ticket: {default}");
    assert_eq!(done["landed"], json!(["K-1"]), "a plain move to done lands it, so it can unblock dependents");
    assert_eq!(done["discarded"], json!([]));
    assert!(done["note"].as_str().unwrap().contains("include_done=true"), "the summary must name the way back: {done}");

    // include_done=true: the whole board, spec text intact.
    let full = mcp.call_tool(3, "kanban_board", &json!({ "include_done": true }));
    let mut restored = ids(&full);
    restored.sort(); // array order is board priority, not this tool's concern
    assert_eq!(restored, ["K-1", "K-2"], "include_done restores the done ticket: {full}");
    let landed = full["structuredContent"]["tickets"].as_array().unwrap().iter().find(|t| t["id"] == "K-1").unwrap();
    assert!(landed["body"].as_str().unwrap().contains("Refined spec"), "and its body comes back whole");
    assert_eq!(landed["notes"].as_array().unwrap().len(), 2, "notes too");
    assert!(full["structuredContent"].get("done").is_none(), "nothing was omitted, so there is no summary");

    // column="done" is the other way in, unchanged by this ticket.
    let column = mcp.call_tool(4, "kanban_board", &json!({ "column": "done" }));
    assert_eq!(ids(&column), ["K-1"], "asking for the done column answers it verbatim: {column}");
    assert!(column["structuredContent"]["tickets"][0]["body"].as_str().unwrap().contains("Refined spec"));
    assert!(column["structuredContent"].get("done").is_none(), "one column was asked for, so no summary is bolted on");

    // The version means the board's version either way — both are valid expected_version tokens.
    assert_eq!(default["structuredContent"]["version"], full["structuredContent"]["version"], "version is the board's, not the subset's");
}

#[test]
fn the_default_board_read_is_dramatically_smaller() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = seed_a_fat_done_ticket(dir.path());
    let mut mcp = McpSession::start(&store_dir);

    let default = mcp.call_tool(2, "kanban_board", &json!({}));
    let full = mcp.call_tool(3, "kanban_board", &json!({ "include_done": true }));
    let (small, big) = (default.to_string().len(), full.to_string().len());
    assert!(small * 10 < big, "the default read must be an order of magnitude smaller than the full board: {small} vs {big}");
}

#[test]
fn kanban_next_lands_merged_review_work_and_the_move_records_companion_branches() {
    use claude_kanban::{
        git::git,
        ops::{self, Op},
        store::model::{Column, ColumnId, Status, TicketId},
    };

    // A real repo: K-1 sits in review on a branch main already contains; K-2 depends on K-1.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]).unwrap();
    git(repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "seed"]).unwrap();
    git(repo, &["branch", "k-1/work"]).unwrap();
    let store_dir = repo.join(".kanban");
    let store = claude_kanban::store::Store::at(&store_dir);
    store.init().unwrap();

    let seed = |title: &str, deps: Vec<TicketId>| {
        ops::apply(
            &store,
            None,
            Op::CreateTicket { title: title.into(), body: String::new(), epic: None, labels: vec![], depends_on: deps, status: Status::Ready },
        )
        .unwrap();
    };
    seed("predecessor", vec![]);
    let k1 = TicketId("K-1".into());
    ops::apply(&store, None, Op::Claim { id: k1.clone(), agent: "claude".into() }).unwrap();
    ops::apply(&store, None, Op::StampWorktree { id: k1.clone(), branch: "k-1/work".into(), path: "/tmp/unused".into() }).unwrap();
    ops::apply(&store, None, Op::MoveTicket { id: k1, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
    seed("dependent", vec![TicketId("K-1".into())]); // board version is now 5

    let mut mcp = McpSession::start(&store_dir);

    // kanban_next sweeps first: K-1 lands (its branch tip is an ancestor of main), which unblocks K-2 — and the
    // returned version reflects the landing, so the follow-up claim uses it directly.
    let next = mcp.call_tool(2, "kanban_next", &json!({}));
    assert_eq!(next["structuredContent"]["ticket"]["id"], "K-2", "{next}");
    assert_eq!(next["structuredContent"]["version"], 6, "the sweep's landing bumped the version");
    let board = store.read_board().unwrap();
    assert!(matches!(board.ticket(&TicketId("K-1".into())).unwrap().column, Column::Done { discarded: false, .. }));

    let claimed = mcp.call_tool(3, "kanban_claim", &json!({ "ticket": "K-2", "expected_version": 6 }));
    assert_eq!(claimed["structuredContent"]["owner"], "claude");

    // Closing out a companion: the move to review accepts a branch and records it; the claim drops on entry.
    let moved = mcp.call_tool(4, "kanban_move", &json!({ "ticket": "K-2", "to": "review", "branch": "k-1/work", "expected_version": 7 }));
    assert_eq!(moved["structuredContent"]["column"], "review", "{moved}");
    let board = store.read_board().unwrap();
    assert!(matches!(&board.ticket(&TicketId("K-2".into())).unwrap().column, Column::Review { branch: Some(b) } if b == "k-1/work"));
    assert!(store.read_claims().unwrap().is_empty(), "entering review drops the claim");
}
