//! The plugin's install surface: plugin.json, marketplace.json, and .mcp.json must parse, agree on names and
//! versions (including Cargo.toml's), and point at a launcher that actually exists and is executable.

use std::{fs, path::Path};

use serde_json::Value;

fn manifest(rel: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} must be readable: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{rel} must be valid JSON: {e}"))
}

#[test]
fn plugin_and_marketplace_manifests_agree() {
    let plugin = manifest(".claude-plugin/plugin.json");
    let marketplace = manifest(".claude-plugin/marketplace.json");

    assert_eq!(plugin["name"], "kanban");
    assert_eq!(marketplace["name"], "claude-kanban", "the marketplace name is what users type after @");

    let entry = &marketplace["plugins"][0];
    assert_eq!(entry["name"], plugin["name"], "install addresses plugins[0] by the plugin.json name");
    assert_eq!(entry["source"], "./", "the repo root is the plugin");
    assert_eq!(entry["version"], plugin["version"], "marketplace and plugin versions must move in lockstep");
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"), "--version must agree with the plugin manifest");
}

#[test]
fn mcp_manifest_launches_the_first_run_builder() {
    let mcp = manifest(".mcp.json");
    let server = &mcp["mcpServers"]["kanban"];
    assert_eq!(server["command"], "${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp");
    assert_eq!(server["args"], serde_json::json!(["mcp"]));

    let launcher = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/kanban-mcp");
    assert!(launcher.is_file(), "bin/kanban-mcp must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = launcher.metadata().unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "bin/kanban-mcp must be executable, mode is {mode:o}");
    }
}
