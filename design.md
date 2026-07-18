# claude-kanban — design

The design record for v1: what was decided and why. For installation and usage, see the [README](README.md).

## Why

Task tracking for AI-assisted work usually lives either in the chat transcript (lost on the next session) or in a SaaS tracker (heavyweight,
off-machine, and Claude can't see it). This puts the tracker in the repo, in a format both a human and Claude can edit, with a UI that makes
prioritising a drag&drop action.

## How it fits together

One binary with two faces over one store:

```
   browser                          Claude Code
  (human)                            (agent)
      │                                 │
      │ HTTP + SSE                      │ MCP (stdio)
      ▼                                 ▼
┌──────────────┐                 ┌──────────────┐
│ kanban serve │                 │  kanban mcp  │
└──────┬───────┘                 └───────┬──────┘
       │                                 │
       └────────────┬────────────────────┘
                    ▼
          .kanban/board.json
      (+ a gitignored claims sidecar)
```

- **`claude-kanban serve`** — an HTTP server bound to loopback. Serves the card UI, a JSON API, and an SSE stream so the board live-updates
  the moment Claude moves a card.
- **`claude-kanban mcp`** — a stdio MCP server (built on `rmcp`, the official Rust MCP SDK) that Claude Code launches automatically. Gives
  Claude typed tools instead of letting it hand-edit JSON.

Routing every write through typed operations is the point: Claude editing the raw file with a text edit is how a task tracker silently
corrupts itself.

## The board

Three columns, in the classic arrangement. A ticket's **column is its workflow state** and its **position in that column is its priority** —
top of the column is next up. Dragging a card to the top is how you tell Claude what to do next.

| Column  | Meaning                                                        |
|---------|----------------------------------------------------------------|
| `todo`  | Ready to be worked. Claude takes the highest unblocked ticket. |
| `doing` | Claimed and in progress.                                       |
| `done`  | Finished.                                                      |

**Tickets** are the units of work. A ticket can declare dependencies — `depends_on`, a list of ticket ids — and until every one of those is
`done` it is not eligible to be picked up, however high it sits in `todo`. A **claim** on a ticket records who is working it and since
when — that's what surfaces "Claude is working on this right now" on the card face.

**Epics** group tickets and give cards their colour, but they are meta-tasks, not work: nobody claims an epic and nothing is ever
developed "on" one. An epic doesn't even store a column — its place on the board is derived from its tickets: `doing` once at least one of
its tickets reaches `doing` or `done`, `done` only when every one of them is done, `todo` otherwise. Tickets move independently; the epic
follows. On the board an epic renders as a simple checklist, one line per ticket, ticked when done, each linking to the ticket's card.

Every ticket and epic also carries a **`status`** field saying how well-defined it is. This is orthogonal to the column: the column is where
the work sits in the workflow, `status` is whether the work is defined enough to do at all.

| `status` | Meaning                                                                                                                                                                                                                                           |
|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `draft`  | Still being defined by the user. Ignored from a work point of view — Claude neither picks it up nor touches it.                                                                                                                                   |
| `stub`   | A rough outline the user wants fleshed out. Claude expands it into a detailed description (a planning-mode pass), splitting a ticket into subtasks — or an epic into sub-epics — if it turns out to be too much work for one unit.                |
| `review` | Fleshed out and awaiting the user's verdict. When Claude expands a stub, that ticket or epic — and everything newly created by the split — becomes `review`. The user either pushes it back to `stub` for another pass or promotes it to `ready`. |
| `ready`  | Fully specified and ready to be picked up by the LLM harness and implemented.                                                                                                                                                                     |

### Store layout

The board is one file, `.kanban/board.json`, committed to your repo:

```json
{
  "schema": 2,
  "version": 12,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "review", "title": "Review" },
    { "id": "done", "title": "Done" }
  ],
  "epics": [
    { "id": "EP-1", "title": "Auth", "color": "#7c9cf5", "status": "ready" }
  ],
  "tickets": [
    {
      "id": "K-1",
      "title": "Add session refresh",
      "epic": "EP-1",
      "status": "ready",
      "column": { "id": "doing", "owner": "claude", "branch": "k-1/session-refresh" }
    },
    {
      "id": "K-4",
      "title": "Audit log for sign-ins",
      "epic": "EP-1",
      "status": "ready",
      "pr": { "number": 12, "url": "https://github.com/acme/myrepo/pull/12", "state": "merged", "merged_commit": "8f7d3a2c1b" },
      "column": { "id": "review", "branch": "k-4/audit-log" }
    },
    {
      "id": "K-3",
      "title": "Password reset flow",
      "epic": "EP-1",
      "status": "ready",
      "external": { "provider": "github", "kind": "issue", "number": 42 },
      "column": { "id": "done", "branch": "myrepo-issue0042", "completed_at": "2026-07-14T09:12:00Z" }
    },
    {
      "id": "K-2",
      "title": "Rate-limit the login route",
      "epic": "EP-1",
      "status": "stub",
      "depends_on": ["K-1"],
      "column": { "id": "todo" }
    }
  ]
}
```

Six properties of this shape are load-bearing:

- **A ticket's `column` is a tagged object, not a bare string.** The `id` names the workflow state and that state's data nests inside it:
  `doing` carries `owner` and `branch`, `done` carries `branch` and `completed_at`, `todo` carries nothing extra. This maps directly onto an
  internally-tagged Rust enum, so a ticket structurally cannot sit in one column while carrying another column's fields. Every `column.id`
  must name a column defined in `columns`, and that's checked on load.
- **Epics store no column at all.** An epic's column is a pure function of its tickets — `done` iff every ticket is done, `doing` once any
  ticket is in `doing` or `done`, `todo` otherwise — computed on read, never written, so it can never disagree with the tickets.
- **`depends_on` must form a DAG.** Every referenced id must exist and cycles are rejected on load. A ticket with unmet dependencies is
  *blocked*: visible in `todo`, skipped by `kanban_next`.
- **Top-level `columns` holds metadata only** — title, colour. Not membership lists that could drift out of sync with the tickets.
- **Priority is the order of the `tickets` array.** Among tickets sharing a column, earlier in the array means higher on the board.
  Reprioritising is moving a ticket object up the array — no rank numbers to rebalance, and still hand-editable.
- **`version` is an optimistic-concurrency counter.** Two processes write this file — the browser and Claude. Every mutation reads the
  version, and a write whose version no longer matches is rejected rather than silently clobbering the other side. Writes go to a temp file
  and are renamed into place, so a crash mid-write can't leave a half-written board.

One optional field looks outward: `external` binds a ticket to a work item in another system — `{provider, kind, number}`, e.g. the GitHub
issue a minesweeper daemon is chewing on. The binding is just an address for other tools to act on; the binary itself never touches the
network. See [interop](#interop-minesweeper-and-friends) below.

One thing deliberately does *not* live in this file: the live claim. `.kanban/claims.json` — gitignored, like the lock and pid files — holds
`{ticket, agent, since, path}` for work in flight, because a worktree path on your machine means nothing on a collaborator's, and nobody
wants a phantom "Claude is working on this" ghosting across the team's boards. Durable, shareable outcomes (`owner`, `branch`,
`completed_at`) nest under the ticket's `column`; machine-local live facts stay in the sidecar. Both files sit under the same advisory lock;
the `version` counter belongs to `board.json`.

The files are meant to be readable and hand-editable. If you fix something in an editor, it still loads.

`done` is deliberately uncapped: the column, and with it the file, just grows. An archive can come later if size ever becomes a problem in
practice. And when two humans edit the board on parallel branches, the per-ticket objects keep any merge conflict in `board.json` small and
local — resolving it is ordinary git conflict work, no special tooling required, though a custom merge driver could close even that gap one
day. (Ticket branches themselves can never conflict over the board — see the sparse checkout below.)

## Worktrees: one ticket, one checkout

Claude does not work in your checkout. Every ticket is worked in its own git worktree on its own branch, and your working copy stays yours.
This is also what makes the board multiplayer: n worktrees, n Claude sessions, one board — the advisory lock and version counter already
assume concurrent writers.

The lifecycle of a ticket, as the plugin's `/kanban:work` skill drives it — each step one small, explicit operation:

1. **Claim** — `kanban_claim` moves the card to `doing` with the agent as `owner` and records the live claim in the sidecar. A pure board
   mutation; git is untouched.
2. **Start** — `claude-kanban worktree start K-7` (Claude uses the matching `kanban_worktree_start` tool) creates the ticket's branch from
   the main checkout's current `HEAD` (`--base` overrides) and a worktree for it under the worktree root, then fills in `branch` on the
   ticket's `column` and the worktree `path` on the live claim. Idempotent: if the ticket already has a `k-7/*` branch, it re-attaches a
   worktree to it instead of failing.
3. **Work** — Claude shifts into the worktree for the ticket's lifetime and commits after logical work chunks; commits land in the repo's
   shared `.git`, so the worktree directory itself is expendable. Don't spam commits. Typically commit after every task, or logically
   sized sub-tasks. Board operations (`kanban_note`, `kanban_move`, …) issued from inside
   the worktree still land on the main checkout's board — see anchoring below.
4. **Finish** — `worktree finish K-7` refuses if the worktree is dirty (`--force-discard` to override), removes the worktree, and runs
   `git worktree prune`. The branch survives and its name is reported.
5. **Close out** — the card moves to `done` (keeping `branch`, gaining `completed_at`) and the live claim is dropped. Integrating
   `k-7/rate-limit-login` — a local merge, or push and PR — is your explicit next step, never a side effect. `worktree finish --merge`exists
   for when you do want the merge in one motion.

**Subtasks stay in their parent's worktree.** Basing the branch on the main checkout's `HEAD` is the right default for fresh, top-level
work, but when a ticket in progress spawns a subtask, the work is already sitting in a worktree — the subtask is worked there too, on the
same branch. Worktrees are never created from inside worktrees. This means one worktree, and one branch, can end up resolving several
tickets; each of those tickets records the shared `branch` on its `column` as usual.

Branches are named **`{id}/{slug}`** — the ticket id lowercased, then a *short* kebab-case digest of the title: K-7 "Rate-limit the
login route" → `k-7/rate-limit-login`, and a long title like "Add authorization based on OAuth from Google" condenses to
`k-8/google-oauth`, not a slugging of the whole mouthful. `worktree start` derives a slug from the title; `--slug` (or the tool's `slug`
argument) overrides it when a human or Claude can condense better (use haiku for rapid naming). The id prefix is the part that matters — it
makes branch → ticket unambiguous, which is how `start` finds the branch to re-attach and how anyone reading `git branch` knows whose work
is whose.

### The board never moves

The store always resolves to the **main working tree's** `.kanban/`, wherever the process runs. The binary asks git — the first entry of
`git worktree list --porcelain` — instead of trusting its own working directory, so `kanban mcp` invoked from deep inside a worktree still
reads and writes the one true board. `--store` / `KANBAN_STORE` remain as explicit overrides, and outside a git repo the store falls back to
`./.kanban` as before. (This is also why `.mcp.json` no longer pins `KANBAN_STORE` to the session's project directory: a session started
inside a worktree would pin it to the wrong place and split the board in two.)

Anchoring is the correctness mechanism — with it, a copy of `board.json` inside a worktree would be inert, because nothing ever reads it
there. But to avoid even the confusion, worktrees created by the binary exclude `.kanban/` from disk entirely, with a per-worktree sparse
checkout:

```bash
git worktree add --no-checkout <path> -b k-7/rate-limit-login <base>
git -C <path> sparse-checkout set --no-cone '/*' '!/.kanban/'
git -C <path> checkout
```

This needs git ≥ 2.36 — on older gits, enabling sparse checkout from a worktree can flip `core.sparseCheckout` in the shared repo config and
blank files out of your main checkout, so below that floor the binary falls back to a plain worktree with a warning (safe, thanks to
anchoring). One pleasant consequence: a ticket branch still carries `board.json` in its *tree*, but no worktree can ever modify it, so
ticket branches cannot conflict with each other — or with you — over the board.

### Where worktrees live

`<root>/<repo-name>-<hash>/<ticket-id>`, where the root defaults to `/tmp/claude-kanban` and the short hash of the main checkout's path
keeps two repos both named `api` apart. Override the root with `--dir`, `KANBAN_WORKTREE_DIR`, or a `worktree_root` entry in
`.kanban/config.json` — flag beats env beats config.

`/tmp` is deliberate — worktrees are meant to be expendable — but it is volatile: reboots wipe it, and on many systems it is RAM-backed
tmpfs. What survives a wipe: every commit, the branch, the claim, the card. What dies: uncommitted changes. `worktree start` always prunes
stale registrations first and re-attaches to the ticket's existing `<id>/*` branch, so recovery is just running `start` again. This is also
why the skill commits as it completes each logical chunk rather than hoarding work in the tree. If that trade-off doesn't suit a machine,
the config override is the place to move the root somewhere persistent.

### Honest costs

- Every worktree is a cold start: `target/`, `node_modules/` and friends are per-directory. Shared caches (`CARGO_TARGET_DIR`, sccache) are
  the user-side mitigation.
- `git worktree add` does not populate submodules; `start` runs `git submodule update --init --recursive` when `.gitmodules` exists.
- `.env`, local certs — are copied into the worktree if, and only if, they are authorised in the config (concretely, `start` copies
  any gitignored files named in `.kanban/config.json`'s `"copy_to_worktrees": ["./filea", "./config/fileb"]`).

## Interop: minesweeper and friends

The board is designed to feed more than one kind of worker. A Claude Code session driving `/kanban:work` is one
consumer; [minesweeper](../minesweeper) — a daemon that polls GitHub issues and runs an agent per issue in its own worktree — is the shape
of another. Three choices keep that door open:

- **The handoff contract is `ready` and unblocked.** Whatever the worker — an interactive session or a daemon — it takes tickets that are
  `ready`, sit in `todo`, and have every dependency `done`. Dependency ordering is the board's job, never the worker's: minesweeper
  processes its queue FIFO and is dependency-blind, so it must only ever be fed tickets that are already unblocked.
- **`external` tickets are worked elsewhere, and the board knows it.** Delegating a ticket means mirroring it to a GitHub issue, applying
  the eligibility label minesweeper watches for (`autofix` / `tryFix`), and recording `{provider, kind, number}` on the ticket — the
  plugin's skill does this with `gh`; the binary stays offline. A claimed `external` ticket moves to `doing` and simply sits there:
  claude-kanban never creates a worktree or branch for it.  
- **`branch` is data, not a format.** Tickets worked by this plugin get `{id}/{slug}` branches; an external ticket's `branch`, if recorded
  at all, is whatever the delegate created (minesweeper's `myrepo-issue0042`). Nothing in the board assumes it can parse a branch name.

## Distribution: one MCP command, three launchers

`.mcp.json` names a single command on every platform — `${CLAUDE_PLUGIN_ROOT}/bin/kanban-mcp` — because the plugin manifest offers no
per-platform mechanism: neither the plugins reference nor the MCP docs define os-conditional `mcpServers` entries (checked July 2026,
code.claude.com/docs/en/plugins-reference and /en/mcp; the official plugin-dev skill is silent too). What makes the one entry work
everywhere is how Claude Code spawns stdio servers: its bundled MCP SDK transport launches the command through a vendored cross-spawn
with `shell: false`. On unix that execs the POSIX-sh `bin/kanban-mcp` via its shebang. On Windows, cross-spawn resolves the
extensionless path with node-which + PATHEXT — landing on `bin\kanban-mcp.cmd` — and, because a `.cmd` is not directly executable,
re-wraps the launch as `cmd.exe /d /s /c`, whose own PATHEXT search is a second net for the same resolution. (Read out of the 2.1.212
binary, where the Linux build's vendored cross-spawn/which visibly carries the win32 branches constant-folded away — the Windows build
keeps them. If a future Claude Code drops cross-spawn for a raw `CreateProcess`, no shim filename could save the extensionless entry,
and only then would a manifest change be on the table.)

The `.cmd` itself stays a four-line trampoline into `bin/kanban-mcp.ps1` (`powershell -NoProfile -NonInteractive -ExecutionPolicy
Bypass -File`, falling back to `pwsh`), which mirrors the sh launcher decision for decision: version pinned from plugin.json, the
`KANBAN_RELEASE_BASE_URL` seam, first-field `.sha256` parsing, staging inside `target/release` so the install is a same-filesystem
rename, staleness arbitrated by `--version`, every failure one stderr line then the cargo fallback. The one Windows-only wrinkle is the
final launch: PowerShell's call operator re-decodes a native child's stdout through its object pipeline, so the shim hands over with
`Start-Process -NoNewWindow -Wait`, which passes the raw inherited stdio handles straight to `claude-kanban.exe` — stdout carries
nothing but JSON-RPC. A batch-only or polyglot single file was rejected: batch has no `Get-FileHash`/`Expand-Archive`, and sh/batch
polyglots die on the first parser quirk. Windows arm64 has no published binary and falls through to cargo with a clear stderr line.

## What v1 does

The implementation checklist, kept as the record of scope.

### Board UI (`serve`)

- [x] Three-column board of cards, epic-coloured
- [x] Drag a card between columns to move it through the workflow
- [x] Drag within a column to reprioritise
- [x] Create, edit, and delete tickets and epics from the board
- [x] Card detail pane — markdown body, labels, progress notes
- [x] Live updates via SSE when Claude changes something
- [x] Blocked badge on tickets whose `depends_on` is not yet all `done`; dependencies editable from the detail pane
- [x] Epic cards are checklists — one line per ticket, ticked when done, each linking to its ticket's card; epics aren't draggable, they
  move themselves
- [x] "Claude is working on this" indicator on claimed cards
- [x] Claimed cards show owner, branch, and worktree path; a claim whose worktree has vanished reads "worktree missing — restore with
  `worktree start`" rather than looking like live work
- [x] Show and set `status` (draft / stub / review / ready) on cards — promoting `review` to `ready` or pushing back to `stub` is the user's
  call, made here
- [x] Filter by epic, label, and status
- [x] Create PR button on eligible done tickets (branch still exists, repo has a remote, not external) — pushes the branch and opens a
  GitHub PR via `gh` with a body templated from the card, recording the URL as a progress note; the binary's one network egress, behind the
  explicit click
- [x] Merged badge on done tickets, hidden by default behind a "merged" filter toggle with a count hint in the Done header — one
  `git branch --no-merged HEAD` per render, so merged means ancestor-of-HEAD *or* branch deleted (the common rebase/squash-then-delete
  flows); a squash-merged branch kept alive locally reads as not merged

### Claude's side (`mcp`)

- [x] `kanban_board` — read the board, optionally one column
- [x] `kanban_next` — the top unclaimed `ready` (action: implement) or `stub` (action: refine) ticket in `todo` whose dependencies are all
  `done`
- [x] `kanban_claim` / `kanban_release` — take and give back a ticket
- [x] `kanban_move` — move a ticket to a column, at a position
- [x] `kanban_create_ticket` / `kanban_create_epic`
- [x] `kanban_note` — append to a ticket's progress log
- [x] `kanban_bind_external` — record (or clear) a ticket's binding to an external work item; used by `/kanban:delegate`
- [x] `kanban_refine` — flesh out a `stub` ticket or epic into a detailed spec, splitting into subtasks or sub-epics when it's too big;
  everything it touches or creates lands in `review`. A stub claimed for refinement sits pink in `doing` while the spec is written;
  `kanban_refine` returns it to the top of `todo` and drops the claim
- [x] `kanban_worktree_start` / `kanban_worktree_finish` — the worktree lifecycle, mirroring the CLI (claiming itself stays a pure board
  mutation)

### Store

- [x] Load, validate, and atomically save `board.json`
- [x] Optimistic-concurrency rejection on stale writes
- [x] Cross-process advisory lock around read-modify-write
- [x] Resolve the store to the main working tree (`git worktree list --porcelain`) from anywhere in any worktree
- [x] Claims sidecar `.kanban/claims.json` — gitignored, guarded by the same lock
- [x] Validate column-dependent ticket fields per column (the `column` tagged object)
- [x] Validate `depends_on`: every referenced ticket exists, the graph is acyclic
- [x] Derive epic columns from their tickets on read — never stored

### Worktrees (CLI)

- [x] `claude-kanban worktree start <ticket>` — branch `<id>/<slug>` + sparse worktree + claim stamping, idempotent; refuses `external`
  tickets (they're worked elsewhere); `--base`, `--slug`, `--dir`, `--no-sparse`, `--json`
- [x] `claude-kanban worktree finish <ticket>` — refuse if dirty (`--force-discard` overrides), remove worktree, prune; branch kept,
  `--merge` opt-in
- [x] `claude-kanban worktree list` — worktrees joined with claims: ticket, branch, path, dirty / missing state

### Plugin glue

- [x] `/kanban:work` — the policy loop: claim the next `ready`, unblocked ticket, start its worktree, implement with sensibly-sized commits,
  note progress, finish, report the branch. Starting the loop is the user's opt-in: inside a running loop Claude claims tickets on its own,
  one after the next, but it never claims spontaneously outside one. Pushing and PR creation for the agent path live here in the skill; the
  binary's one network egress is the serve face's single handler `POST /ui/ticket/{id}/create-pr` — the Create PR button — which runs only
  on an explicit button click. The principle was always "nothing leaves the machine without explicit user action", and the click is that
  action. With `"max_workers": N` (N > 1) in `.kanban/config.json`, the loop goes parallel: the session claims tickets itself and fans each
  one out to a subagent in its own worktree, keeping at most N in flight; `kanban_board` reports the effective value.
- [x] `/kanban:delegate` — mirror a `ready`, unblocked ticket to a GitHub issue, apply the eligibility label, record the `external` binding,
  and claim it into `doing` on the daemon's behalf; the skill owns the `gh` calls

## Open design questions

Things deliberately left undecided:

1. **Should `worktree finish --merge` ever become the default,** and what is *the* target branch for a local-only tool?

# claude kanban v2

The big change: The Review column.

- Add a 4th column, "Review" that sits between doing and "Done"

The definition of "Done" is when the code (either from a local branch, and external merge) lands back in the local `main` branch, or is 
discarded. 

If the code is still a PR, or on a branch off of main, it stays in "REview" even if the local worktree has been removed.

Importantly, in practice the behavioral change affest dependencies: dependencies are still triggered by the Done state - but in practice 
this means that they are merged into main rather than just having their worktrees removed. Blocked tickets will now have the intended 
behaviour. A ticket is blocked until their predecessors land in `main`.

The "Review" column is the new category that catches tickets that have been "code completed", but not yet merged. This matches the 
current flow, essentially. It also catches tockets that are external or have PRs on gitihub that are being reviewed and waiting for a merge.

We need to add a `gh` poll to track the status of PRs and detect when they get merged (and pulled back into local main). "Done" is only 
when PRs land locally, not in `origin/main`.

V2 flow might break the logic of subtasks running in a parent worktree. Do a deep dive to untangle any potential issues that may arise 
there.

Extra housekeeping: Create a fully defined `config.json` when initialising the kanban board.
Expose settings on the UI

