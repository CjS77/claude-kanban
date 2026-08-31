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
    for name in ["init", "open"] {
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
    }
}

/// The fenced `bash` blocks of a markdown document — what the model is told to actually run, as opposed to prose that
/// may legitimately name `claude-kanban` (quoting its stderr, or warning against calling it directly).
fn bash_blocks(body: &str) -> impl Iterator<Item = &str> {
    body.split("```bash").skip(1).filter_map(|rest| rest.split_once("```")).map(|(block, _)| block)
}

/// The opencode surface: `opencode/index.js` injects the MCP server, commands, and effort agents through opencode's
/// config hook (opencode reads none of the Claude Code manifests). These tests pin the pieces the hook assembles at
/// runtime: the command templates it reads, the launcher placeholder it substitutes, and the agent files it shares
/// with the Claude Code plugin. See docs/opencode.md for the install story.
#[test]
fn opencode_plugin_wires_the_same_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin = fs::read_to_string(root.join("opencode/index.js")).expect("opencode/index.js is the opencode entry point");

    for name in ["kanban:init", "kanban:open", "kanban:work", "kanban:delegate"] {
        assert!(plugin.contains(&format!("\"{name}\"")), "index.js must register the {name} command");
    }
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert!(plugin.contains(&format!("{level}:")), "index.js must map effort level {level}");
        let agent = root.join(format!("agents/kanban-effort-{level}.md"));
        assert!(agent.is_file(), "index.js reads the shared agent prompt {}", agent.display());
    }
    assert!(plugin.contains("kanban-mcp.cmd"), "index.js must pick the .cmd launcher on Windows");
    assert!(plugin.contains("kanban-rules.md"), "index.js must inject the workflow rules file");
    assert!(plugin.contains("config.json"), "index.js must read the board config for the models vocabulary");
    assert!(plugin.contains("kanban-model-"), "index.js must inject model-pinned agents for configured provider/model entries");
    assert!(plugin.contains("{{KANBAN_MODELS}}"), "index.js must substitute the model dispatch table into the command templates");
    for key in ["implement_model", "refine_model"] {
        assert!(plugin.contains(key), "index.js must read the board's {key} role default and give it a pinned agent");
    }
}

/// The role defaults are a board dial both harnesses honour, so both command templates have to say how — the opencode
/// one routes through the pinned agents, the Claude Code one passes a per-call model.
#[test]
fn both_work_commands_resolve_the_boards_role_defaults() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in ["commands/work.md", "opencode/command/work.md"] {
        let body = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel} must exist: {e}"));
        for key in ["implement_model", "refine_model"] {
            assert!(body.contains(key), "{rel} must tell the loop to resolve the board's {key}");
        }
        assert!(
            body.contains("else the role default") || body.contains("else the board's"),
            "{rel} must state the precedence: the ticket's own model wins over the role default"
        );
    }
}

/// Same contract as `the_setup_commands_drive_the_binary_through_the_launcher`, for the opencode templates: every
/// runnable block goes through the launcher via the `{{KANBAN_ROOT}}` placeholder index.js substitutes at load.
#[test]
fn the_opencode_commands_drive_the_binary_through_the_launcher() {
    for name in ["init", "open"] {
        let rel = format!("opencode/command/{name}.md");
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} must exist: it is an opencode command template: {e}"));

        let frontmatter = body.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")).map(|(front, _)| front);
        let frontmatter = frontmatter.unwrap_or_else(|| panic!("{rel} must open with a --- frontmatter block"));
        assert!(frontmatter.contains("description:"), "{rel}'s frontmatter must carry a description: index.js refuses to load without one");

        assert!(
            body.contains("{{KANBAN_ROOT}}/bin/kanban-mcp"),
            "{rel} must invoke the launcher through the {{{{KANBAN_ROOT}}}} placeholder — the binary is not on PATH"
        );
        bash_blocks(&body).for_each(|block| {
            assert!(
                block.contains("{{KANBAN_ROOT}}/bin/kanban-mcp"),
                "{rel} has a runnable block that doesn't go through the launcher:\n{block}"
            );
        });
    }
    for name in ["work", "delegate"] {
        let rel = format!("opencode/command/{name}.md");
        let body = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel)).unwrap_or_else(|e| panic!("{rel} must exist: {e}"));
        assert!(body.contains("description:"), "{rel} must carry a description");
        assert!(body.contains("kanban_kanban_"), "{rel} must explain opencode's server-name tool prefix");
    }
    // No cwd survives between shell calls on this harness, so the work command must root ticket commands at the
    // worktree path rather than telling the agent to cd into it once. See docs/opencode.md.
    let work = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("opencode/command/work.md")).unwrap();
    assert!(
        work.contains("git -C <path>"),
        "opencode/command/work.md must root git at the worktree path — a bare git command runs in the main checkout"
    );
    assert!(
        work.contains("cd <path> && "),
        "opencode/command/work.md must show the rooted prefix form for non-git commands"
    );
    assert!(
        !work.contains("`cd` into the reported worktree path and stay there"),
        "opencode/command/work.md must not tell the agent to cd once — that wording assumes a cwd this harness drops"
    );
    let work = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("opencode/command/work.md")).unwrap();
    assert!(work.contains("{{KANBAN_MODELS}}"), "work.md must carry the placeholder index.js fills with the model dispatch table");
}

/// opencode never surfaces MCP server instructions, so the rules file the plugin injects via `instructions` must
/// carry the same workflow contract the Rust server ships — compared word-for-word, ignoring line wrapping.
#[test]
fn opencode_rules_carry_the_mcp_instructions() {
    let rules = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("opencode/kanban-rules.md"))
        .expect("opencode/kanban-rules.md must exist: index.js injects it as an instructions file");
    let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalize(&rules).contains(&normalize(claude_kanban::mcp::INSTRUCTIONS)),
        "opencode/kanban-rules.md must contain the MCP INSTRUCTIONS text verbatim (line wrapping aside) — \
         src/mcp.rs changed without the rules file following"
    );
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
