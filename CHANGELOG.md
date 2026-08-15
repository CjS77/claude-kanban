# Changelog

All notable changes to this project are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.3.0] - 2026-08-15

### Added

- **Inline comments in the diff.** Click any line number in the diff pane and write a note against that line. The
  comments collect per ticket and drain into the Review pane's comment box as `path:line — text` bullets, so whichever
  verdict you press carries them: a Request changes note now tells the agent *where* to look, not just what to fix.
  Deleted lines are quoted by their old-file number and say so, since they have no counterpart in the new file to
  point at.

  There is deliberately no server side to this. No new routes, no new ops, no stored review state: the diff stays
  stateless, the ticket note remains the record, and `changes requested:` is already the rework spec `kanban_next`
  hands the agent — the line references only tell it where to read. Once drained the bullets are ordinary text in the
  box, so they can be reworded or deleted before the verdict goes out, and nothing reaches the agent unseen. Coming
  back to a line already commented on replaces its bullet rather than stacking a second one under the same reference,
  so the box never carries two contradictory remarks about one line. The Review pane also grew its own **View diff**
  button, since that is where the flow starts and where the comments land.

### Changed

- The ticket card leads with its actions. Both panes put their button row directly under the Details/Review switch
  instead of at the foot: a card's body, dependency list and progress log all grow without bound, so the longer a
  ticket had been worked, the further you had to scroll to press the one thing you opened it for. The status switch
  (draft/stub/review/ready) stays at the foot — it settles what the ticket *is*, which is a decision you make after
  reading it. The Details/Review switch itself is now the size of an action button and three times as wide, since it
  swaps the entire pane and had been reading as one more small control among many.
- **Accept no longer asks twice.** It is permission for a work loop to land the branch, and that permission can be
  withdrawn right up until the landing happens, so a confirm dialog in front of the review pane's headline verdict
  bought a second click and nothing else. Discard — the verdict that cannot be taken back — keeps its confirm, and
  Request changes never had one.

## [3.2.0] - 2026-08-10

### Added

- **Accept clears the work to land.** The Review pane's Accept button no longer closes a card on the reviewer's word:
  it marks the ticket `accepted` and leaves it in `review`, and `kanban_next` then surfaces it to `/kanban:work` as a
  new `action: "land"` — ranked **first**, ahead of rework and any new work — which rebases the branch onto the main
  branch and fast-forwards main into it. The card reaches `done` the only way any card does, with the landing sweep
  proving the code arrived. This removes the feature's sharpest edge: accepting used to unblock dependents onto code
  that was nowhere near main, and now nothing unblocks until the code is provably there.

  The rebase stays out of the binary deliberately. `src/land.rs` asks git questions and moves no refs; resolving a
  conflict is judgement work over the code, and there is no agent behind a browser click. Handing it to the work loop
  puts the job where somebody can read the diff — and makes it the *same* procedure `auto_merge` already used, now
  documented once as *Landing a branch* with two ways in: permission granted ahead of time, or given after the fact.

  When a landing cannot finish, the agent aborts the rebase and calls a new `kanban_block_landing`: the ticket stays
  in `review` wearing `landing_blocked` — a red card with a `⚠` badge — and the newest note names what git refused and
  which paths collided. A blocked ticket is no longer offered for landing, so no loop retries the same conflict; only
  a human puts it back, by resolving it and pressing **Accept again**, which clears the flag and re-arms the landing.
  Both flags are additive and serde-default to false, so existing boards read unchanged and the schema stays at 2.

  An in-flight landing is visible and interlocked. `kanban_start_landing` takes a marker that renders as a quiet
  "⟳ … is landing this" and refuses when the ticket is not cleared to land, is flagged, or another worker holds it —
  so two parallel loops cannot rebase the same branch onto a moving main. It lives in the claims sidecar under a new
  `ClaimKind` rather than on the ticket: it is machine-local live state, it must never put an owner on a review ticket,
  and ops can write that file transactionally with the board, which is what lets the marker retire with the landing
  itself. A marker whose agent died is ignored after 15 minutes, so a crash costs one stalled card rather than a stuck
  one, and the board says "stalled, it will be retried" instead of hiding it.
- A **Review** pane on the ticket card, reached by a Details/Review switch on any non-external review ticket: the
  branch, the worktree still on disk (badged when it holds uncommitted changes), the landing sweep's own verdict, the
  progress log, a comment box, and the three verdicts — **Accept**, **Request changes**, **Discard**. An *On accept*
  line says what the click will hand to the work loop, read from the same gate the loop's own eligibility check applies
  (a new `land::explain`), so the button is never a surprise; an accepted ticket then says it is waiting to land.
  Discard grows the shared comment box, folding it into the reason, while its existing bodyless POST keeps
  working. Server-rendered htmx throughout — no new JavaScript, and no classes outside the committed Tailwind build.
  Cards and detail panes badge `changes requested`, and a `changes-requested:` search key filters on it.
- Review rounds: a human reviewing a ticket can send it back instead of only accepting or discarding it. The card
  returns to the top of `doing` carrying a new `changes_requested` flag, with its feedback as the newest
  `changes requested:` note and its branch and worktree untouched, and `kanban_next` surfaces it as a new
  `action: "rework"` — ranked **above** todo work, because somebody is waiting on near-finished code whose worktree
  already exists. The flag clears on reaching `review`, landing or discarding, but deliberately survives a release:
  feedback does not stop existing because a worker handed the ticket back. Claiming is relaxed for exactly that state
  (the card is owned by the reviewer and worked by nobody), and external tickets are refused throughout — a delegated
  ticket's review feedback belongs on its issue, which is what keeps minesweeper's tickets out of the rework queue by
  construction. `changes_requested` is additive and serde-defaults to false, so existing boards read unchanged and the
  schema stays at 2.

### Changed

- `.kanban/merge.sh` is **deprecated**, though still shipped and still working. Pressing Accept and letting the work
  loop land the branch does the same thing with an agent that can resolve a conflict, and flags the card when it
  cannot. The script is kept for the case that flow doesn't cover: landing a branch with no loop running, or from a
  session that only speaks MCP — Accept alone never moves main.

- A ticket's worktree is kept through `review` instead of being removed at close-out, and is retired when the ticket
  reaches `done` — landed, or discarded. Review is now something you can review *from*: the code is on disk to read,
  and reworking a card re-attaches to the same checkout rather than rebuilding one. Retirement is best-effort and can
  never fail a landing; a worktree holding uncommitted changes is kept, with a `kanban` note on the card naming the
  path. The cost, accepted deliberately: worktrees live for the whole review period rather than seconds.
- The close-out is `kanban_move to=review` and nothing else — agents no longer call `kanban_worktree_finish`, and the
  dirty-worktree refusal that tool performed moves onto the move itself (MCP only; a human dragging the card in the
  browser stays the override, as with discard). `kanban_worktree_finish` is unchanged and remains the *deliberate*
  removal: the human's CLI, abandoning a checkout early, and the first step of any merge.
- `merge.sh` removes a clean ticket worktree instead of refusing to run. It refused whenever the branch was checked
  out in a linked worktree, which now describes every review ticket; it stops only when that worktree is dirty.

### Fixed

- The diff view's file list stays in place while the diff scrolls. The modal was one scroll box, so a long diff carried
  the "sticky" TOC off the top with it; the two panes now scroll independently inside a fixed-height frame, and a click
  on a file entry moves the diff pane alone rather than every scrollable ancestor.

## [3.1.0] - 2026-08-08

### Added

- Configurable model vocabulary: a `"models"` list in `.kanban/config.json` (editable from the ⚙ settings pane)
  replaces the default Claude aliases (`opus`/`sonnet`/`haiku`/`fable`) everywhere they surface — the web UI's model
  datalists and placeholders, and the `kanban_board` MCP response, which now returns the effective list as `models`
  so board-driving agents suggest ids their harness can actually run. Empty or absent keeps the defaults. Under
  opencode the list also unlocks per-ticket model dispatch: for every `provider/model` entry the plugin injects six
  model-pinned `kanban-model-<slug>` subagents (base + one per effort level) and substitutes the model → agent table
  into `/kanban:work` via a new `{{KANBAN_MODELS}}` placeholder, so a ticket's `model` field is honoured instead of
  only noted as a deviation. Entries without a provider prefix, and models not in the list, keep the old
  note-the-deviation behaviour; the list is read at session start, so edits need an opencode restart.

## [3.0.0] - 2026-08-08

### Added

- opencode support: `opencode/index.js` — a dependency-free plugin for [opencode](https://opencode.ai) that injects
  the `kanban` MCP server (through the existing launcher, `.cmd` on Windows), the four `/kanban:*` commands (from
  templates in `opencode/command/`, launcher path substituted at load), the five `kanban-effort-*` subagents (sharing
  the `agents/*.md` prompts, effort carried as a `reasoningEffort` model option, `max` saturating at `xhigh`), and the
  workflow rules (`opencode/kanban-rules.md`, injected as an instructions file only where a `.kanban/` board exists,
  since opencode never surfaces MCP server instructions — tests/manifests.rs pins it to `mcp::INSTRUCTIONS`). Install
  is one `"plugin"` line in opencode config; the walkthrough and harness differences (server-name tool prefix,
  per-ticket `model` not honourable per call, `serve` detached via `nohup`) are in `docs/opencode.md`, which also
  ships in the in-app 📖 docs.

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

[3.3.0]: https://github.com/CjS77/claude-kanban/compare/v3.2.0...v3.3.0
[3.2.0]: https://github.com/CjS77/claude-kanban/compare/v3.1.0...v3.2.0
[3.1.0]: https://github.com/CjS77/claude-kanban/compare/v3.0.0...v3.1.0
[3.0.0]: https://github.com/CjS77/claude-kanban/compare/v2.7.0...v3.0.0
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
