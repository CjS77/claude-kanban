# Automation

Claude Kanban can automate almost the entire workflow.

You ask Claude Code or Opencode to write a ticket; they works it, you look at the result, review it, possibly from another harness completely
and land it. 

## Stubs: Claude writes the spec

The first thing to stop doing is writing detailed tickets. Drop a one-liner on the board and set its status to `stub`.
The next time a `/kanban:work` loop runs dry of ready work, `kanban_next` hands it the stub as `action: "refine"`.

What happens then:

- The card turns pink in `doing` while Claude reads the codebase. No worktree, no branch: refining produces a spec,
  not commits, and nothing in your checkout is touched.
- When it has enough to write a precise, implementable description, it calls `kanban_refine`. The ticket comes back to
  the top of `todo` with a fleshed-out body, possibly a sharper title, and its status set to `review`.
- If the stub turns out to be several pieces of work, the same call splits it. New tickets, or a new epic grouping
  them, appear beside it, all at status `review`, with any dependencies between the pieces set as part of the split.
  Everything the split created goes through the same vetting as the original.

Your part is the status. A `review` spec is waiting for you to read it. Push it back to `stub` if you want another
pass, with a note saying what was missing, or promote it to `ready` so a loop can build it. Claude never promotes its
own specs, and it never touches anything at status `draft`.

If you want a stronger or cheaper model for this research than the one your session runs, set `refine_model` in the
settings pane. A stub can also name its own `model`, which wins.

## Minesweeper: someone else writes the code

Turn on **Delegate ready tickets** in the settings pane (`"minesweeper": true` in `.kanban/config.json`) and the board
stops writing code at all. It becomes a queue in front of a [minesweeper](https://github.com/CjS77/minesweeper)
daemon, which watches GitHub issues carrying a label and runs an agent per issue.

With the toggle on:

- A ready ticket entering `doing`, whether a loop claimed it or you dragged it, is mirrored to a GitHub issue wearing
  the `minesweeper_label` (default `autofix`). The card records the issue and is re-owned to `minesweeper`. The loop
  that claimed it moves on to the next ticket without opening a worktree.
- The board's poller (every `poll_interval` seconds, default 60) watches the issue. When the daemon opens a PR that
  closes it, the card moves to `review` with the PR recorded. When that PR is merged and you pull, the card lands like
  any other.
- If the daemon gives up, it labels the issue (`minesweeperFailed` or `possiblyDangerous` by default; `minesweeper_flag_labels`
  in the config) and the card is flagged in `doing` for you to look at. An issue closed without a PR is flagged the
  same way.
- If the daemon splits an issue into sub-issues, the board mirrors them as child tickets the parent depends on. The
  parent parks in `review` until every child has landed and you close the parent issue by hand.

Stubs still work the normal way with the toggle on. Refining is how a one-liner becomes something a daemon could
build, and that stays Claude's job in your session.

Two smaller ways to do the same thing: the **Hand to minesweeper** checkbox on the new-ticket form delegates one ticket
as you create it, toggle or no toggle; and `/kanban:delegate` does the mirroring by hand for a ticket already on the
board, after which you move the card to `review` yourself when the PR shows up.

Things worth knowing before you rely on it:

- Delegated tickets have no Review tab and no rework path on the board. Feedback on the daemon's PR happens on GitHub.
  `Request changes` refuses them, and a `/kanban:review` loop skips them.
- The labels have to agree with the daemon's own configuration, or tickets are delegated into the void. The daemon
  also only looks at the 30 most recent open issues.
- This is one of exactly three places the binary talks to the network, alongside the Create PR button and the PR
  polls. A build with `--no-default-features` compiles it out.

## `auto_review`: a second loop judges the code

`/kanban:review` is the mirror image of `/kanban:work`. Where the work loop watches `todo` and moves cards forward,
the review loop watches `review` and forms an opinion about what arrives there. For each new card it launches a
reviewer agent that finds the worktree, diffs the branch against main, and checks the result against the ticket's
body. The findings go onto the card's review log, opening with a recommended verdict and with line-anchored comments
as entries of their own. They appear under your comment box while you type, and nothing you or an earlier round wrote
is overwritten.

That much happens whether or not you turn anything on. The **Let /kanban:review press verdicts** toggle
(`"auto_review": true`) decides one thing only: whether the loop is allowed to press the button too.

- With it off, the default, the review sits on the card and the verdict is yours. The loop remembers which cards it
  has already looked at this round and does not review the same unchanged card twice.
- With it on, the loop presses `Accept`, `Request changes` or `Discard` itself. An accept clears the branch to land and
  a running work loop picks it up next. A request for changes sends the card back to the top of `doing` with the
  review as the spec for the next round, and a work loop takes that ahead of any new work. A discard retires the
  card without landing it.

That last one deserves a pause. A discarded ticket never unblocks its dependents, and nothing un-discards it. The loop
is told to keep discards for work that is genuinely dead and to say why, but if you would rather no machine ever made
that call, leave the toggle off and read the recommendations instead. Reviews still get written either way.

Running both loops at once is the intended shape: one builds, one judges, and the accept from one is the landing
instruction for the other.

## `auto_merge`: land it without asking

The last handoff is the merge into main. Every card in `review` is waiting for the same thing: someone to rebase its
branch onto main and fast-forward main into it. Normally that someone is a work loop acting on your `Accept`. With
`auto_merge` set, the permission is granted in advance.

You set it per ticket, with the toggle on the card's detail pane, or per epic, which covers every ticket in it. The
toggle's confirmation spells out what you are agreeing to, because the effect is that main moves with nobody looking
at the merge.

What actually happens:

- When a work loop finishes a flagged ticket and moves it to `review`, it carries straight on to the landing
  procedure: remove the worktree, rebase the branch onto main, fast-forward main, and let the board's landing sweep
  prove the code arrived. The card reaches `done` by the same evidence as any other card. Only after that is the
  branch deleted.
- A rebase conflict the agent cannot resolve with confidence is aborted, not guessed at. Main and the branch are left
  as they were, the card stays in `review` shaded red with a **landing blocked** badge, and the newest progress note
  names the files that collided. Nothing retries it until you sort the branch out and press `Accept` again, or send it
  back for changes.
- Any conflict it did resolve is written on the card, file by file, so a quiet resolution can be reviewed afterwards.

The flag only means anything inside a running `/kanban:work` loop. It is a preference stored on the board and honoured
by the loop, not something the binary does on its own. A card you drag to `review` by hand will not merge itself, and
neither will one that is `external`. The `.kanban/merge.sh` script is still there for landing a branch yourself.

## Putting it together

Each handoff removes one of your touches. Here is what is left at each level, for a ticket that starts as a one-line
stub:

| What you have turned on              | What you still do                                                   |
|--------------------------------------|----------------------------------------------------------------------|
| Nothing                              | Write the spec, promote to `ready`, read the code, press a verdict, land it or accept it. |
| Stubs                                | Promote the spec to `ready`, read the code, press a verdict, accept.  |
| Stubs + auto_review                  | Promote the spec to `ready`. The loops build, judge, accept and land. |
| Stubs + auto_merge                   | Promote the spec to `ready`. The work loop builds and lands, unreviewed. |
| Minesweeper                          | Promote the spec to `ready`, review the PR on GitHub, merge it, pull. |

A few things about how they combine:

- **auto_merge wins over auto_review.** A flagged ticket is landed by the work loop the moment it finishes, before any
  review loop has seen it. Use one or the other for a given ticket or epic: auto_review when you want a second pair of
  eyes before main moves, auto_merge when you trust the work enough to skip that.
- **Minesweeper tickets ignore both flags.** The board never has a local branch for them, so there is nothing for a
  review loop to diff or a work loop to rebase. Review and merge happen on the PR, and the board watches for the result.
- **Dependencies still wait for landings.** However automated the path, a ticket blocked on another stays in `todo`
  until that other one is `done`, which means merged into local main. A fully automated chain of dependent tickets
  therefore builds itself in order, each one starting from a worktree that already contains its predecessors.
- **Every automation only runs inside a loop you started.** No card is claimed, reviewed, judged or merged by a session
  you did not point at the board with `/kanban:work` or `/kanban:review`. Stopping the loop stops all of it.

And what stays yours regardless: the `draft` status, which no loop touches; the promotion from `review` to `ready`,
which no tool can make on Claude's behalf; and the check that turns `review` into `done`, which is always the board's
proof rather than anyone's say-so.

## Configuration

All of these are settings in `.kanban/config.json`, editable from the settings pane behind the board's gear icon.
`/kanban:init` seeds every key at its default, and never overwrites your edits.

| Key                        | Default                                | Turns on                                         |
|----------------------------|----------------------------------------|--------------------------------------------------|
| `refine_model`             | inherit the session's model            | A different model for refining stubs             |
| `minesweeper`              | `false`                                | Delegating ready tickets to the daemon           |
| `minesweeper_label`        | `autofix`                              | The label the daemon watches for                 |
| `minesweeper_flag_labels`  | `minesweeperFailed`, `possiblyDangerous` | The labels that flag a card as needing you     |
| `auto_review`              | `false`                                | Letting the review loop press verdicts           |
| `poll_interval`            | `60`                                   | How often delegated issues and PRs are checked   |

`auto_merge` is not a board setting. It lives on the ticket or the epic, because the decision to land something unseen
belongs with the piece of work, not with the project.
