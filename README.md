# claude-kanban

A Claude Code plugin that gives a project a Kanban board — four columns, draggable cards, backed by a plain JSON file in your repo. No server accounts: the board lives in `.kanban/board.json` and is committed with your code.

You drag cards around in a browser to say what matters. Claude reads the same board over MCP, picks up tickets, works each one in its own git worktree, and moves the cards across as it goes. Both sides see the same thing, live.

## Install

You need git — nothing else on the five released platforms (Linux x86_64/aarch64, macOS Intel/Apple silicon, Windows x86_64): prebuilt binaries ship with each release. In Claude Code:

```
/plugin marketplace add CjS77/claude-kanban
/plugin install kanban@claude-kanban
```

Restart Claude Code (or `/reload-plugins`), then run `/kanban:init` — it seeds the board and opens it in your browser. The plugin registers the `kanban` MCP server and adds the `/kanban:init`, `/kanban:open`, `/kanban:work`, and `/kanban:delegate` commands. On first run the launcher downloads the release binary matching your platform and plugin version, verifies its checksum, and installs it — seconds, not a compile.

**Fallback / building from source.** On any other platform, offline, or when checksum verification refuses the download, the launcher falls back to `cargo build --release`, which needs a Rust toolchain ([rustup.rs](https://rustup.rs)) — everything that worked before the prebuilt binaries still works. If that first-run build takes long enough that MCP startup gives up waiting, the build carries on and the next session attaches normally. Running `cargo build --release` in the plugin directory yourself always works too. Windows follows the normal flow: `bin/kanban-mcp.cmd` (PowerShell underneath) downloads, verifies, and installs the same way, with the same cargo fallback — only Windows arm64, which has no published binary, needs the toolchain.

To hack on the plugin itself, load your clone directly:

```bash
git clone https://github.com/CjS77/claude-kanban && cd claude-kanban
cargo build --release        # self-contained — the web UI is embedded, no node required
claude --plugin-dir .        # start Claude Code with the plugin loaded
```

Releasing (maintainer): push `main` to origin, tag the version, and push the tag — installs and updates pull from the repo, and the tag publishes prebuilt binaries (see Development → Releasing).

## Use

In Claude Code, `/kanban:init` seeds the board and opens it — that's the whole setup. Commit the two files it creates (`.kanban/board.json` and `.kanban/config.json`); `/kanban:open` puts the board back on screen later, reusing the running server if there is one.

From a clone, the binary does the same two steps directly:

```bash
claude-kanban init     # creates .kanban/board.json and .kanban/config.json
claude-kanban serve    # opens the board at http://127.0.0.1:4747
```

Several projects can serve at once: an explicit port (`--port`, `KANBAN_PORT`, or `"port"` in `.kanban/config.json`) is honoured or fails loudly; with no explicit choice, `serve` tries 4747 and otherwise picks a free port — and if this project is already being served, it prints that URL instead of starting a duplicate.

The workflow:

1. **Write tickets** on the board — or drop one-line ideas as `stub`s for Claude to flesh out into specs.
2. **Prioritise by dragging.** Column is workflow state (`todo` / `doing` / `review` / `done`); position in the column is priority. A ticket's `status` says how well-defined it is: `draft` (yours, untouchable) → `stub` (flesh me out) → `review` (vet the spec) → `ready` (implementable). Promoting to `ready` is your call, made on the card. A card can also name the **model** and **effort** its work deserves; leave them blank and it inherits whatever the worker session is running.
3. **Run `/kanban:work`** in Claude Code. Claude claims the top eligible ticket, works it in its own worktree on its own branch, notes progress on the card, and moves it to `review` — code-complete, waiting to land — then takes the next. When the board runs dry the loop doesn't exit: it sleeps and polls again, so you can keep dropping tickets while it runs — interrupt it to stop. Your checkout is never touched; integrating the branch is your explicit step — merge it locally, or click **Create PR** on the review ticket's detail pane to push the branch and open a GitHub PR via `gh`.
4. **Done happens by itself.** Done means *landed in your local main branch*: the board watches review tickets and moves each to `done` the moment its branch — or its PR's merge commit, once you pull — reaches local main, with a note saying why. A PR merged only on GitHub shows "PR merged — pull main" until the merge arrives locally. Work that will never land is retired with the card's **Discard** button; a discarded ticket closes but keeps its dependents blocked. `.kanban/config.json` tunes everything (editable from the board's ⚙ settings pane): `"main_branch"` anchors landing, `"poll_interval"` sets the PR-poll cadence in seconds (0 turns it off), `"max_workers"` fans `/kanban:work` out to N tickets at once, `"idle_time"` sets the dry-board sleep. `init` seeds every key at its default and never overwrites your edits; `"port"` seeds as `null` on purpose — no port means `serve` tries 4747 and hunts for a free one, whereas naming a port makes a busy port a hard failure.
5. **Or `/kanban:delegate`** a ticket to an external worker: it's mirrored to a GitHub issue and the board tracks it as worked elsewhere; once its PR opens, move the card to `review` with the PR's head branch and the board lands it like any other.

Dependencies (`depends_on`) block a ticket until they're all done — and since done means landed, a dependent's fresh worktree is guaranteed to contain its predecessors' code. Epics group tickets, colour their cards, and move themselves — their column is derived from their tickets; deleting an epic deletes its tickets.

**Upgrading from 1.x:** a v1 board upgrades itself in memory on first read and persists on the first write — commit the changed `board.json`. Collaborators still on a 1.x plugin can't read a v2 board, so upgrade together. Old done tickets stay done; drag one back to review if you want the landing machinery to re-judge it.

## Features

- Four-column board: create, edit, drag, and delete tickets and epics; one search box narrows it — `landed: true, label: ux, realtime
  results` is three ANDed terms. Bare text matches anywhere in a ticket (id, title, body, labels, branch, external binding, PR); the keys
  are `text:` `label:` `epic:` `id:` `note:` `status:` `col:` `model:` `effort:` `landed:` `discarded:` `blocked:`. `epic:none` (or
  `epic:null`) finds the tickets filed under no epic at all. Quote a value to keep a comma inside it
  (`label:"foo, bar"`); anything the grammar doesn't recognise is searched as plain text. The `?` beside the box opens the same
  reference in the app
- Live updates over SSE — cards move the moment Claude moves them
- **Done means landed**: review tickets move to done automatically when their branch or PR provably reaches local main (ancestry, or patch-equivalence for rebase-then-delete flows) — never on guesswork; ambiguous cards get flagged for you
- PR tracking: the Create PR button binds the PR to the ticket, a config-gated `gh` poll follows it to the merge, and daemon- or skill-created PRs are discovered by branch
- Claimed cards show who's working, the branch, and the worktree; blocked tickets wear a badge
- Per-ticket **model and effort**: a ticket can say what its work is worth running at — `opus` and `xhigh` for the hairy refactor, nothing at all for the one-liner — and `/kanban:work` dispatches it that way instead of using whatever your session happens to be on. Set both on the card; leave them blank to inherit
- Typed MCP tools for Claude (`kanban_board`, `kanban_next`, `kanban_claim`, `kanban_move`, `kanban_refine`, …) — every write goes through the same validated operations as the UI, guarded by an advisory lock and an optimistic version counter. `kanban_board` omits done tickets by default, summarizing their ids instead, so a work loop can poll cheaply; `include_done=true` or `column="done"` reads them in full, and your browser board is unaffected
- One ticket, one git worktree, one branch (`k-7/rate-limit-login`) — parallel sessions can't trample each other or your checkout
- A settings pane (⚙) editing `.kanban/config.json` from the board
- Everything local: one binary, a JSON file, a loopback server. Network happens in exactly two places, both yours to control: the explicit **Create PR** click, and the read-only `gh` PR poll (`"poll_interval": 0` switches it off)

The reasoning behind these choices — store shape, worktree anchoring, landing proofs, statuses, interop — is in [design.md](design.md).

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
commands/                    the plugin skills: /kanban:init, /kanban:open, /kanban:work, /kanban:delegate
agents/                      one ticket-worker per effort level, the only place effort is settable
src/
  store/                     model, atomic IO, advisory lock, validation, derived read model
  ops.rs                     the single typed-mutation funnel both faces share
  land.rs                    landing detection: the offline sweep and the gh PR poll
  server/                    axum routes, Askama views, SSE, loopback hardening, the landing loop
  mcp.rs                     the rmcp stdio server: kanban_* tools → ops
  worktree.rs                one-ticket-one-checkout: start / finish / list
templates/                   Askama templates (the whole UI)
assets/                      embedded web assets: htmx, SortableJS, marked, DOMPurify,
                             glue.js (the only hand-written JS), app.css (generated, committed)
css/app.tailwind.css         Tailwind v4 source — rebuilt with `just css` (standalone CLI, no node)
vendor/daisyui.js            daisyUI bundle, used only at CSS build time
design.md                    the design record: decisions and their reasons
.kanban/board.json           the board (created by `init`; committed)
.kanban/claims.json          live claims — machine-local, gitignored
.kanban/land-state.json      the landing sweep's branch observations — machine-local, gitignored
```

## Licence

MIT OR Apache-2.0.
