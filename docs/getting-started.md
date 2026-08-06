# Getting started

A Claude Code plugin that gives a project a Kanban board — four columns, draggable cards, backed by a plain JSON file
in your repo. No server accounts: the board lives in `.kanban/board.json` and is committed with your code.

You drag cards around in a browser to say what matters. Claude reads the same board over MCP, picks up tickets, works
each one in its own git worktree, and moves the cards across as it goes. Both sides see the same thing, live.

## Install

You need git — nothing else on the five released platforms (Linux x86_64/aarch64, macOS Intel/Apple silicon, Windows
x86_64): prebuilt binaries ship with each release. In Claude Code:

```
/plugin marketplace add CjS77/claude-kanban
/plugin install kanban@claude-kanban
```

Restart Claude Code (or `/reload-plugins`), then run `/kanban:init` — it seeds the board and opens it in your browser.
The plugin registers the `kanban` MCP server and adds the `/kanban:init`, `/kanban:open`, `/kanban:work`, and
`/kanban:delegate` commands. On first run the launcher downloads the release binary matching your platform and plugin
version, verifies its checksum, and installs it — seconds, not a compile.

**Fallback / building from source.** On any other platform, offline, or when checksum verification refuses the
download, the launcher falls back to `cargo build --release`, which needs a Rust toolchain ([rustup.rs](https://rustup.rs)) —
everything that worked before the prebuilt binaries still works. If that first-run build takes long enough that MCP
startup gives up waiting, the build carries on and the next session attaches normally.

To hack on the plugin itself, load your clone directly:

```bash
git clone https://github.com/CjS77/claude-kanban && cd claude-kanban
cargo build --release        # self-contained — the web UI is embedded, no node required
claude --plugin-dir .        # start Claude Code with the plugin loaded
```

## Use

In Claude Code, `/kanban:init` seeds the board and opens it — that's the whole setup. Commit the two files it creates
(`.kanban/board.json` and `.kanban/config.json`); `/kanban:open` puts the board back on screen later, reusing the
running server if there is one.

From a clone, the binary does the same two steps directly:

```bash
claude-kanban init     # creates .kanban/board.json and .kanban/config.json
claude-kanban serve    # opens the board at http://127.0.0.1:4747
```

Several projects can serve at once: an explicit port (`--port`, `KANBAN_PORT`, or `"port"` in `.kanban/config.json`)
is honoured or fails loudly; with no explicit choice, `serve` tries 4747 and otherwise picks a free port — and if
this project is already being served, it prints that URL instead of starting a duplicate.
