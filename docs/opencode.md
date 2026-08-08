# Using the board with opencode

\opencode-specific instructions ship in the
[opencode plugin](https://opencode.ai/docs/plugins/) at `opencode/index.js` that injects the same surface through
opencode's config hook:

- the `kanban` MCP server, launched through the same first-run launcher (`bin/kanban-mcp`, the `.cmd` shim on
  Windows) that downloads or builds the binary on demand;
- the four commands, under the same names: `/kanban:init`, `/kanban:open`, `/kanban:work`, `/kanban:delegate`;
- the five `kanban-effort-*` subagents, reusing the Claude Code agent prompts verbatim with the effort level carried
  as a `reasoningEffort` model option;
- the workflow rules (the text Claude Code receives as MCP server instructions, which opencode doesn't surface),
  injected as an instructions file — only in projects that actually have a `.kanban/` board.

## Install

You need git and [opencode](https://opencode.ai) ≥ 1.18. Clone the repo somewhere permanent (it hosts the launcher
and the binary it materialises — not a temp dir):

```bash
git clone https://github.com/CjS77/claude-kanban ~/tools/claude-kanban
```

Then add the plugin to your opencode config — globally in `~/.config/opencode/opencode.json` to have it in every
project, or in a single project's `opencode.json`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["~/tools/claude-kanban/opencode"]
}
```

(Use the expanded absolute path if your opencode version doesn't resolve `~` in plugin paths.) Restart opencode, then
run `/kanban:init` in your project — it seeds `.kanban/`, opens the board in your browser, and tells you what to
commit. On first use the launcher downloads the release binary for your platform and verifies its checksum; on
unmatched platforms or offline it falls back to `cargo build --release`, which needs a
[Rust toolchain](https://rustup.rs).

`opencode mcp list` should show `kanban` connected; `opencode debug config` shows everything the plugin injected.

## Differences from Claude Code

The board, the files, the workflow, and the landing rules are identical — a board created under one harness is the
same board under the other, and sessions from both can share it. What differs is harness plumbing:

- **Tool names wear the server prefix.** opencode registers MCP tools as `<server>_<tool>`, so `kanban_board`
  appears in the tool list as `kanban_kanban_board`, `kanban_claim` as `kanban_kanban_claim`, and so on. The
  commands and rules the plugin injects say this explicitly; you'll see the prefixed names in permission prompts.
- **Per-ticket `model` is not honoured.** opencode's task tool takes no per-call model override and the effort
  agents deliberately pin no model, so a ticket's `model` field can't switch models mid-loop the way Claude Code's
  Agent tool can. The work loop notes the deviation on the card instead of silently ignoring it. The workaround when
  the model matters: start a session on that model and run `/kanban:work <ticket-id>` there — a ticket-id argument
  works that one ticket and ends.
- **Per-ticket `effort` maps to `reasoningEffort`.** Each `kanban-effort-<level>` subagent carries its level as a
  `reasoningEffort` model option, passed through to whatever provider the session runs on. `max` maps to `xhigh`
  (the highest value providers accept), and providers that don't support the option ignore it — either way the loop
  notes on the card when a dial was mapped rather than applied exactly.
- **`serve` runs via `nohup`.** opencode's shell tool has no background mode, so `/kanban:open` starts the server
  detached (`nohup … &`) and reads the URL back from a log file. The server outlives the opencode session either
  way; stop it with the pid recorded in `.kanban/serve.pid`.
- **Rules load after the board exists.** The workflow contract is injected as an instructions file only when the
  project has a `.kanban/` directory, so unrelated projects don't carry kanban rules. After the very first
  `/kanban:init` in a project, restart opencode to pick the rules up.

Overriding any injected piece is supported: an `mcp.kanban`, `command["kanban:work"]`, or `agent["kanban-effort-…"]`
entry you define in your own opencode config wins over the plugin's.

## Uninstall

Remove the `plugin` entry from your opencode config and delete the clone. The board files (`.kanban/`) belong to
your projects, not the plugin, and keep working with Claude Code or a later reinstall.
