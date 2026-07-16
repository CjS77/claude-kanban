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
        assert!(
            init["result"]["instructions"].as_str().unwrap_or_default().contains("kanban_next"),
            "instructions must carry the workflow contract"
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
