//! The plugin's install surface: plugin.json, marketplace.json, and .mcp.json must parse, agree on names and
//! versions (including Cargo.toml's), and point at a launcher that actually exists and is executable. The setup
//! commands are part of that surface: they are what a user runs before knowing anything else about the tool.

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

/// The launcher path is the whole point: `claude-kanban` is not on `PATH` and a fresh install has no binary at all
/// until `bin/kanban-mcp` fetches or builds one. A command file that drifts to a bare `claude-kanban` still reads
/// fine and still passes review — it just fails for every user who hasn't built from source. So: every runnable
/// block in a command file must go through the launcher.
#[test]
fn the_setup_commands_drive_the_binary_through_the_launcher() {
    ["init", "open"].into_iter().for_each(|name| {
        let rel = format!("commands/{name}.md");
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} must exist: /kanban:{name} is a command file: {e}"));

        let frontmatter = body.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")).map(|(front, _)| front);
        let frontmatter = frontmatter.unwrap_or_else(|| panic!("{rel} must open with a --- frontmatter block"));
        assert!(frontmatter.contains("description:"), "{rel}'s frontmatter must carry a description: it is the command's /help line");

        assert!(
            body.contains("${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp"),
            "{rel} must invoke the launcher — the binary is not on PATH and may not exist yet"
        );
        bash_blocks(&body).for_each(|block| {
            assert!(
                block.contains("${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp"),
                "{rel} has a runnable block that doesn't go through the launcher — a bare claude-kanban only works \
                 for someone who built from source:\n{block}"
            );
        });
    });
}

/// The ```bash blocks of a markdown document — what the model is told to actually run, as opposed to prose that may
/// legitimately name `claude-kanban` (quoting its stderr, or warning against calling it directly).
fn bash_blocks(body: &str) -> impl Iterator<Item = &str> {
    body.split("```bash").skip(1).filter_map(|rest| rest.split_once("```")).map(|(block, _)| block)
}

#[test]
fn windows_shim_backs_the_same_mcp_command() {
    let bin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin");
    let cmd = fs::read_to_string(bin_dir.join("kanban-mcp.cmd"))
        .expect("bin/kanban-mcp.cmd must exist: Windows resolves .mcp.json's extensionless command to it via PATHEXT");
    assert!(cmd.contains("kanban-mcp.ps1"), "the .cmd is a trampoline and must hand off to the PowerShell launcher");

    let shim = fs::read_to_string(bin_dir.join("kanban-mcp.ps1")).expect("bin/kanban-mcp.ps1 must exist: the .cmd only trampolines");
    assert!(shim.contains("x86_64-pc-windows-msvc"), "the shim must pin the published Windows release target");
    assert!(shim.contains("claude-kanban.exe"), "the shim must install the exe name the release zip ships");
    assert!(shim.is_ascii(), "kanban-mcp.ps1 must stay ASCII: Windows PowerShell 5.1 reads an unmarked .ps1 as ANSI");
}
