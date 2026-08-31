# Using the board with opencode

\opencode-specific instructions ship in the
[opencode plugin](https://opencode.ai/docs/plugins/) at `opencode/index.js` that injects the same surface through
opencode's config hook:

- the `kanban` MCP server, launched through the same first-run launcher (`bin/kanban-mcp`, the `.cmd` shim on
  Windows) that downloads or builds the binary on demand;
- the four commands, under the same names: `/kanban:init`, `/kanban:open`, `/kanban:work`, `/kanban:delegate`;
- the five `kanban-effort-*` subagents, reusing the Claude Code agent prompts verbatim with the effort level carried
  as a `reasoningEffort` model option — plus model-pinned `kanban-model-*` twins for every `provider/model` entry in
  the board's `models` config (see below);
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
- **Per-ticket `model` is honoured through the board config.** opencode's task tool takes no per-call model
  override, so the pin must live in an agent definition: for every `provider/model` entry in the `models` list of
  `.kanban/config.json`, the plugin injects six `kanban-model-<slug>` subagents (a base one, plus one per effort
  level) that the work loop dispatches to when a ticket names that model. A ten-model list means sixty injected
  subagent entries — cheap in config, but worth knowing when reading `opencode debug config`. The Claude aliases the
  board suggests by default (`opus`, `sonnet`, …) carry no provider prefix, so they are not addressable here: a
  ticket naming an unconfigured or bare-alias model keeps the old behaviour — the loop dispatches by `effort` alone
  and notes the deviation on the card. The fix when the model matters: add it to `models` (as `provider/model`) and
  restart opencode, or start a session on that model and run `/kanban:work <ticket-id>` there — a ticket-id argument
  works that one ticket and ends.
- **The board's role defaults pick the model for tickets that name none.** `implement_model` and `refine_model` in
  `.kanban/config.json` (or the settings pane, under Models) say what works a card that carries no `model` of its
  own — the first for implementing and reworking, the second for refining a stub. They get pinned agents exactly like
  a `models` entry does, and need not be listed in `models` themselves: that list is the vocabulary a *ticket* draws
  on, while these are the board's fallback. A ticket's own `model` always wins. Setting an `implement_model` means
  even an unadorned ticket is delegated to a subagent, since a session cannot change its own model mid-run.
- **Per-ticket `effort` maps to `reasoningEffort`.** Each `kanban-effort-<level>` subagent carries its level as a
  `reasoningEffort` model option, passed through to whatever provider the session runs on. `max` maps to `xhigh`
  (the highest value providers accept), and providers that don't support the option ignore it — either way the loop
  notes on the card when a dial was mapped rather than applied exactly.
- **No working directory survives between shell calls, so ticket commands name their worktree.** Claude Code's Bash
  tool keeps its cwd across calls, so its work command can say "`cd` into the worktree and stay there". Here the next
  command is back in the project root, which would silently run `git commit` and the test suite against the main
  checkout while the edits landed in the worktree — the branch ends up empty and the card cannot close out. The
  opencode work command therefore roots every ticket command explicitly (`cd <path> && …`, or `git -C <path> …`) and
  has the subagent prove the worktree branch before it writes anything. It is the one place the two command files
  deliberately differ in procedure rather than wording.
- **`serve` runs via `nohup`.** opencode's shell tool has no background mode, so `/kanban:open` starts the server
  detached (`nohup … &`) and reads the URL back from a log file. The server outlives the opencode session either
  way; stop it with the pid recorded in `.kanban/serve.pid`.
- **Rules load after the board exists.** The workflow contract is injected as an instructions file only when the
  project has a `.kanban/` directory, so unrelated projects don't carry kanban rules. After the very first
  `/kanban:init` in a project, restart opencode to pick the rules up. The `models` list is read the same way — at
  session start — so editing it (or seeing rules at all on a fresh board) needs an opencode restart.

Overriding any injected piece is supported: an `mcp.kanban`, `command["kanban:work"]`, `agent["kanban-effort-…"]`,
or `agent["kanban-model-…"]` entry you define in your own opencode config wins over the plugin's.

## Worked example: a different model per role

The three models a kanban session runs on are set in two different places, because two of them are the board's
business and one is yours:

```jsonc
// ~/.config/opencode/opencode.json — your session, not the board
{
  "$schema": "https://opencode.ai/config.json",
  "model": "venice/z-ai-glm-5-3-flash",       // the orchestrator: the loop you drive in the terminal
  "small_model": "venice/z-ai-glm-5-3-flash", // titles and other lightweight calls
  "plugin": ["~/tools/claude-kanban/opencode"]
}
```

```jsonc
// <project>/.kanban/config.json — the board, shared with Claude Code
{
  "implement_model": "venice/deepseek-v4-flash", // writes the code
  "refine_model": "venice/z-ai-glm-5-3-flash"    // writes the specs
}
```

That gives you a cheap fast model writing code, a stronger one writing specs, and whatever you like driving the loop.
The orchestrator model is opencode's own dial — the plugin neither reads nor sets it, and `/models` switches it
mid-session. The two role defaults are board config, so they travel with the project and apply under Claude Code too.

Restart opencode after editing `.kanban/config.json`: the pinned agents and the dispatch table the work command reads
are both frozen at session start, exactly as for `models`. `opencode debug config` shows the injected agents — expect
six per distinct `provider/model` named anywhere in the three keys, plus the five `kanban-effort-*`.

## Uninstall

Remove the `plugin` entry from your opencode config and delete the clone. The board files (`.kanban/`) belong to
your projects, not the plugin, and keep working with Claude Code or a later reinstall.
