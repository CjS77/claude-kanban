# Changelog

All notable changes to this project are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.7.0] - 2026-08-06

### Added

- In-app documentation: a 📖 button in the board header opens a two-pane modal — a file-based table of contents on the
  left, rendered markdown on the right. The `docs/` tree is baked into the binary with `rust-embed`, so the server stays
  self-contained, and rendering reuses the `data-md-src` → `marked`/`DOMPurify` pipeline that already draws ticket and
  epic bodies. Four read-only routes (`/docs`, `/docs/page/{name}`, `/raw/docs/{name}`, `/docs/assets/{*path}`), each
  rejecting path traversal. Seeded with getting-started, workflow and search pages (#5).
- README: how to reach a board served on a remote machine.

## [2.6.0] - 2026-08-05

### Added

- Minesweeper delegation (`"minesweeper": true` in `.kanban/config.json`, off by default): a ready ticket entering
  doing is mirrored by the binary to a GitHub issue wearing `"minesweeper_label"` (default `autofix`) for an external
  minesweeper daemon — kanban writes no code. The serve poller batch-queries every delegated issue per tick
  (GraphQL `closedByPullRequestsReferences`, one call): the daemon's PR moves the card to review with the PR recorded,
  flag labels (`"minesweeper_flag_labels"`, default `minesweeperFailed`/`possiblyDangerous`) and closed-without-a-PR
  issues flag the card in place with a note, and a refine split is mirrored as claimed child tickets the parent
  depends on. A refined parent lands by a new rule 6: every mirrored child done-and-kept **and** the parent issue
  closed by a human.
- `Op::Delegate` and `Op::MirrorSubIssues` in the write funnel; `External` grows optional `closed`/`flag`/`sub_issues`
  observations (additive, no schema bump). `kanban_board` reports `minesweeper` so `/kanban:work` knows to hand off
  instead of implementing; cards and the detail pane wear a `⚠ minesweeper` badge while flagged.
- Cargo feature `minesweeper` (default-on) — `--no-default-features` compiles the whole delegation egress out for
  installs that want the binary's network surface limited to the Create PR click and the landing poll.
- Settings pane: the delegation toggle, eligibility label, and flag labels.
- A **Hand to minesweeper** checkbox on the New-ticket and Edit modals: sets the ticket `ready` and immediately
  mirrors, claims, and binds it for the daemon — its own per-ticket opt-in, working with or without the project
  toggle. The edit form offers it only for unbound todo tickets. A failed handoff releases the claim back to todo
  with a note explaining why.

### Fixed

- `.claude-plugin/plugin.json` and `marketplace.json` versions had fallen behind Cargo.toml (2.3.0 vs 2.5.0), failing
  the manifests test.

## [2.5.0] - 2026-07-29

### Added

- A local, GitHub-style diff viewer for review-column tickets. A View diff button computes the branch's own changes
  against main (three-dot range), the `unidiff` crate parses them, and an Askama template renders file/hunk/line tables
  in a theme-aware GitHub palette, with highlight.js colouring the code (and the ticket markdown code blocks) in the
  browser. Purely local — no push, no remote — unlike Create PR. Adds `GET /ui/ticket/{id}/diff` behind a `can_diff`
  gate on the detail pane.
- A file table of contents and collapsible files in the diff modal. A sticky left-hand TOC lists every changed file with
  anchor links to each file's section, and each file is a `<details>` whose header collapses its hunks. Both are pure
  HTML/CSS, no new JavaScript; the modal keeps its width and the code rows their size.
- `kanban:init` seeds `.kanban/merge.sh` (the manual land helper) alongside board.json, config.json and .gitignore,
  embedding the repo's canonical merge.sh via `include_str!` so the shipped copy never drifts. It is written executable
  and committed rather than gitignored, and a hand-edited copy survives re-init (seed-if-absent).

## [2.4.0] - 2026-07-28

### Added

- Squash-landing detection by content containment: `git::contained_in` asks whether main already holds everything a
  branch adds, so a GitHub squash-merge (which collapses N commits into one with a fresh patch-id) now lands instead of
  parking forever in review. It declines an empty branch, so a no-op branch never manufactures a landing.
- `kanban_next` explains a stalled board instead of going quiet: an empty answer now carries `waiting.todo` and
  `waiting.review` with the reasons work is held back, capped by an explicit `not_shown`, so an idle board and a stuck
  one no longer read alike.

### Fixed

- The branch tip is observed the moment a ticket enters review, via `observe_entering_review` on both the MCP close-out
  and the browser drop. Previously the observations sidecar only held what a sweep happened to see, so the last ticket
  of a `/kanban:work` run could enter review unobserved and, once rebased and its branch deleted, disarm every landing
  proof at once — parking the card and blocking its dependents.

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

[2.7.0]: https://github.com/CjS77/claude-kanban/compare/v2.6.0...v2.7.0
[2.6.0]: https://github.com/CjS77/claude-kanban/compare/v2.5.0...v2.6.0
[2.5.0]: https://github.com/CjS77/claude-kanban/compare/v2.4.0...v2.5.0
[2.4.0]: https://github.com/CjS77/claude-kanban/compare/v2.3.0...v2.4.0
[2.3.0]: https://github.com/CjS77/claude-kanban/compare/v2.2.0...v2.3.0
[2.2.0]: https://github.com/CjS77/claude-kanban/compare/v2.1.0...v2.2.0
[2.1.0]: https://github.com/CjS77/claude-kanban/compare/v2.0.1...v2.1.0
[2.0.1]: https://github.com/CjS77/claude-kanban/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/CjS77/claude-kanban/compare/v1.2.0...v2.0.0
[1.2.0]: https://github.com/CjS77/claude-kanban/compare/v1.1.1...v1.2.0
[1.1.1]: https://github.com/CjS77/claude-kanban/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/CjS77/claude-kanban/releases/tag/v1.1.0
