# Changelog

All notable changes to this project are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.3.0] - 2026-07-21

### Added

- `auto_merge` flag on tickets and epics. The stored flag is the ticket's (or epic's) own say; the effective value is
  derived at read time, so an epic's grant is never written onto its tickets and clearing it takes the permission back
  from all of them at once. Both fields default to false and skip serialization when false, so a board written before
  them is unchanged bytes and the schema stays at 2.
- `auto-merge:true` and `auto-merge:false` search filters, with `automerge:` and `auto_merge:` as aliases. They match on
  the derived value, so a ticket inheriting the flag from its epic is selected.
- Auto-merge toggle in the board UI: a dedicated button per ticket and per epic, each with its own confirm, rather than
  a checkbox on the edit form — that form has one blanket Save, so the dialog would fire on every unrelated edit. Cards
  and both detail panes wear a warning badge when the effective flag is on, reading "auto-merge (epic)" when the grant
  is the epic's alone.
- `/kanban:work` rebases and lands auto-merge tickets into main at close-out, without a human seeing the merge. The
  rebase uses `--autostash`, since the move to review leaves the tracked `board.json` dirty.

## [2.2.0] - 2026-07-20

### Added

- Monotonic id counters: a deleted ticket never frees its number for reuse.

### Changed

- Deleting an epic now cascades to its tickets.

## [2.1.0] - 2026-07-20

### Added

- `kanban_update_ticket`, so MCP can rewire dependencies after creation.
- Per-ticket `model` and `effort`, honoured by `/kanban:work`.
- `epic:none` and `epic:null` filters, for tickets with no epic.

## [2.0.1] - 2026-07-18

### Fixed

- The launcher requests the checksum file name the releases actually publish (#1).

## [2.0.0] - 2026-07-18

The review-column release: done means landed, and dependencies unblock only then.

### Added

- A review column between doing and done, carrying PR bindings, a branch-gone flag and a Discard button. Landing
  detection runs as an offline ancestry sweep plus a `gh` PR poll — once at startup, then on every tick of `serve`.
- A search grammar for the filter bar, reachable from a magnifier in the header with a popup documenting the keys.
- A settings pane: `.kanban/config.json` is editable from the board.
- Merge detection anchored to a configured main branch, with `init` seeding a full config.

### Changed

- `kanban_board` omits done tickets by default, returning a summary of their ids instead.
- The v1-to-v2 board upgrade persists at startup instead of being re-derived on each read.
- The merged badge, its filter and the column hint are withdrawn; the review column subsumes them.

### Fixed

- The search box gets a real width.
- The `windows-msvc` release target resolves from the sh launcher under git-bash.

## [1.2.0] - 2026-07-17

### Changed

- Plugin version bump only; no functional changes.

## [1.1.1] - 2026-07-17

### Added

- `/kanban:init` and `/kanban:open`, which get a user to a board in one step. `init` seeds `config.json`, and `serve`
  opens the existing board rather than starting a second server.
- A header badge showing the plugin version and linking the repo.

## [1.1.0] - 2026-07-17

First tagged release, covering the plugin's initial publication.

### Added

- Installable-plugin packaging: `marketplace.json`, a first-run build, and a launcher that downloads the pinned release
  binary, with `cargo build` as the fallback. A tag push cross-builds binaries onto a GitHub Release.
- A `kanban-mcp.cmd` shim so the prebuilt binary runs on Windows.
- A Create PR button on eligible done tickets, and a purple badge on merged ones behind a filter toggle.
- `max_workers` config driving a parallel `/kanban:work` loop, and `idle_time` for how long the loop sleeps when the
  board is dry.
- Claimable stubs for refinement: pink in doing, back to todo as review.
- `serve` auto-selects a free port, so projects coexist.
- `RUST_LOG`-driven diagnostics across the codebase, plus console diagnostics for SSE, requests, refreshes and toasts.

### Fixed

- The status and note actions return their pane-refresh responses the right way round.
- The create-ticket epic dropdown stays in sync with the board.
- Markdown panes that arrive as top-level swap elements render.

[2.3.0]: https://github.com/CjS77/claude-kanban/compare/v2.2.0...v2.3.0
[2.2.0]: https://github.com/CjS77/claude-kanban/compare/v2.1.0...v2.2.0
[2.1.0]: https://github.com/CjS77/claude-kanban/compare/v2.0.1...v2.1.0
[2.0.1]: https://github.com/CjS77/claude-kanban/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/CjS77/claude-kanban/compare/v1.2.0...v2.0.0
[1.2.0]: https://github.com/CjS77/claude-kanban/compare/v1.1.1...v1.2.0
[1.1.1]: https://github.com/CjS77/claude-kanban/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/CjS77/claude-kanban/releases/tag/v1.1.0
