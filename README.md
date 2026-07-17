# claude-kanban

A Claude Code plugin that gives a project a Kanban board — three columns, draggable cards, backed by a plain JSON file in your repo. No server accounts, no network: the board lives in `.kanban/board.json` and is committed with your code.

You drag cards around in a browser to say what matters. Claude reads the same board over MCP, picks up tickets, works each one in its own git worktree, and moves the cards across as it goes. Both sides see the same thing, live.

## Install

You need git — nothing else on the five released platforms (Linux x86_64/aarch64, macOS Intel/Apple silicon, Windows x86_64): prebuilt binaries ship with each release. In Claude Code:

```
/plugin marketplace add CjS77/claude-kanban
/plugin install kanban@claude-kanban
```

Restart Claude Code (or `/reload-plugins`). The plugin registers the `kanban` MCP server and adds the `/kanban:work` and `/kanban:delegate` commands. On first run the launcher downloads the release binary matching your platform and plugin version, verifies its checksum, and installs it — seconds, not a compile.

**Fallback / building from source.** On any other platform, offline, or when checksum verification refuses the download, the launcher falls back to `cargo build --release`, which needs a Rust toolchain ([rustup.rs](https://rustup.rs)) — everything that worked before the prebuilt binaries still works. If that first-run build takes long enough that MCP startup gives up waiting, the build carries on and the next session attaches normally. Running `cargo build --release` in the plugin directory yourself always works too — and is the required route on Windows, where the launcher script needs a POSIX shell (or unpack the Windows release zip into `target/release/` by hand).

To hack on the plugin itself, load your clone directly:

```bash
git clone https://github.com/CjS77/claude-kanban && cd claude-kanban
cargo build --release        # self-contained — the web UI is embedded, no node required
claude --plugin-dir .        # start Claude Code with the plugin loaded
```

Releasing (maintainer): push `main` to origin, tag the version, and push the tag — installs and updates pull from the repo, and the tag publishes prebuilt binaries (see Development → Releasing).

## Use

Seed a board in your project (commit the file it creates), then open the UI:

```bash
claude-kanban init     # creates .kanban/board.json
claude-kanban serve    # opens the board at http://127.0.0.1:4747
```

Several projects can serve at once: an explicit port (`--port`, `KANBAN_PORT`, or `"port"` in `.kanban/config.json`) is honoured or fails loudly; with no explicit choice, `serve` tries 4747 and otherwise picks a free port — and if this project is already being served, it prints that URL instead of starting a duplicate.

The workflow:

1. **Write tickets** on the board — or drop one-line ideas as `stub`s for Claude to flesh out into specs.
2. **Prioritise by dragging.** Column is workflow state (`todo` / `doing` / `done`); position in the column is priority. A ticket's `status` says how well-defined it is: `draft` (yours, untouchable) → `stub` (flesh me out) → `review` (vet the spec) → `ready` (implementable). Promoting to `ready` is your call, made on the card.
3. **Run `/kanban:work`** in Claude Code. Claude claims the top eligible ticket, works it in its own worktree on its own branch, notes progress on the card, and moves it to `done` — then takes the next. When the board runs dry the loop doesn't exit: it sleeps and polls again, so you can keep dropping tickets while it runs — interrupt it to stop. Your checkout is never touched; integrating the reported branch is your explicit step — merge it locally, or click **Create PR** on the done ticket's detail pane to push the branch and open a GitHub PR via `gh`, with the PR URL recorded as a note on the card. `.kanban/config.json` tunes the loop: `"max_workers": N` fans out to N tickets at once, `"idle_time"` sets the sleep in seconds (default 300).
4. **Or `/kanban:delegate`** a ticket to an external worker: it's mirrored to a GitHub issue and the board tracks it as worked elsewhere.

Dependencies (`depends_on`) block a ticket until they're all done, however high it sits. Epics group tickets, colour their cards, and move themselves — their column is derived from their tickets.

## Features

- Three-column board: create, edit, drag, and delete tickets and epics; filter by epic, label, and status
- Live updates over SSE — cards move the moment Claude moves them
- Claimed cards show who's working, the branch, and the worktree; blocked tickets wear a badge
- Typed MCP tools for Claude (`kanban_board`, `kanban_next`, `kanban_claim`, `kanban_move`, `kanban_refine`, …) — every write goes through the same validated operations as the UI, guarded by an advisory lock and an optimistic version counter
- One ticket, one git worktree, one branch (`k-7/rate-limit-login`) — parallel sessions can't trample each other or your checkout
- Everything local: one binary, a JSON file, a loopback server; nothing leaves the machine except on an explicit action of yours — pushing yourself, or clicking a done ticket's **Create PR** button

The reasoning behind these choices — store shape, worktree anchoring, statuses, interop — is in [design.md](design.md).

## Development

```bash
cargo build            # build (self-contained — web assets are embedded)
cargo test             # tests: store contract, ops, HTTP handlers, MCP wire, worktree lifecycle
cargo clippy           # lints (warnings only, never fails the build)
cargo +nightly fmt     # formatting — nightly, the config uses unstable options
just css               # regenerate assets/app.css after template edits (Tailwind standalone CLI + vendored daisyUI)
just css-watch         # …or continuously, while hacking on the UI (pair with `serve --assets-dir assets`)
```

Diagnostics go to stderr (stdout belongs to the MCP protocol) and are filtered with `RUST_LOG` — the default is
`claude_kanban=info,tower_http=warn`. `info` covers lifecycle milestones and every applied op; `warn` covers refusals
(stale versions, the security guard, error toasts); `debug` adds op payloads, store writes, and SSE broadcasts;
`trace` logs every git invocation. E.g. `RUST_LOG=claude_kanban=debug claude-kanban serve`.

Several projects can serve at once with zero coordination. An explicit port (`--port`, `KANBAN_PORT`, or `"port"` in `.kanban/config.json` — in that order) is honoured or fails loudly when taken. With no explicit choice, `serve` tries 4747; if another project holds it, the OS picks a free port, and if *this* project already holds it, `serve` prints the running board's URL and exits instead of starting a duplicate (liveness judged by `serve.pid`).

Installed as a Claude Code plugin, `.mcp.json` registers the MCP server automatically and `/kanban:work` drives the loop.

Releasing: bump the version in `Cargo.toml`, `.claude-plugin/plugin.json`, and `.claude-plugin/marketplace.json` in lockstep
(tests/manifests.rs pins the agreement), then `just release <version>` — it verifies the lockstep and creates the `v<version>`
tag without pushing anything. Pushing the tag (`git push origin v<version>`) fires `.github/workflows/release.yml`, which builds
`claude-kanban` for Linux (x86_64/arm64, static musl), macOS (arm64/x86_64), and Windows (x86_64) and attaches
`claude-kanban-<target>.tar.gz` archives (`.zip` on Windows), their `.sha256` checksums, and build-provenance attestations to a
GitHub Release. Binaries live only in Releases, never in git.

## Layout

```
.claude-plugin/plugin.json   plugin manifest
.mcp.json                    registers the `kanban` MCP server with Claude Code
commands/                    the plugin skills: /kanban:work, /kanban:delegate
src/
  store/                     model, atomic IO, advisory lock, validation, derived read model
  ops.rs                     the single typed-mutation funnel both faces share
  server/                    axum routes, Askama views, SSE, loopback hardening
  mcp.rs                     the rmcp stdio server: kanban_* tools → ops
  worktree.rs                one-ticket-one-checkout: start / finish / list
templates/                   Askama templates (the whole UI)
assets/                      embedded web assets: htmx, SortableJS, marked, DOMPurify,
                             glue.js (the only hand-written JS), app.css (generated, committed)
css/app.tailwind.css         Tailwind v4 source — rebuilt with `just css` (standalone CLI, no node)
vendor/daisyui.js            daisyUI bundle, used only at CSS build time
design.md                    the v1 design record: decisions and their reasons
.kanban/board.json           the board (created by `init`; committed)
.kanban/claims.json          live claims — machine-local, gitignored
```

## Licence

MIT OR Apache-2.0.
