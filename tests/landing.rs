//! The v2 story end to end, no network: a dependent ticket stays blocked until its predecessor's code actually lands
//! in local main — and once it does (via the user's own rebase/fast-forward/delete flow), the dependent's fresh
//! worktree is guaranteed to contain the predecessor's work. This is the exact flaw v1 had: it unblocked dependents at
//! worktree-finish, handing them worktrees without the code they were promised.

use std::{fs, path::Path, process::Command};

use claude_kanban::{
    land,
    ops::{self, Op},
    store::{
        Store,
        derive,
        model::{Column, ColumnId, Status, TicketId},
    },
    worktree::{self, StartOpts},
};

struct Scratch {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    wt_root: std::path::PathBuf,
    store: Store,
}

fn sh(dir: &Path, cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A committed repo on `main` with a board holding K-1 (ready) and K-2 (ready, depends on K-1).
fn scratch() -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("myrepo");
    fs::create_dir(&repo).unwrap();
    sh(&repo, "git", &["init", "-q", "-b", "main"]);
    sh(&repo, "git", &["config", "user.email", "t@example.com"]);
    sh(&repo, "git", &["config", "user.name", "Tester"]);
    fs::write(repo.join("README.md"), "# scratch\n").unwrap();

    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    sh(&repo, "git", &["add", "-A"]);
    sh(&repo, "git", &["commit", "-qm", "initial"]);

    let create = |title: &str, deps: Vec<TicketId>| {
        ops::apply(
            &store,
            None,
            Op::CreateTicket { title: title.into(), body: String::new(), epic: None, labels: vec![], depends_on: deps, status: Status::Ready, model: None, effort: None, auto_merge: false },
        )
        .unwrap();
    };
    create("Lay the foundation", vec![]);
    create("Build on the foundation", vec![TicketId("K-1".into())]);

    let wt_root = dir.path().join("worktrees");
    Scratch { _dir: dir, repo, wt_root, store }
}

fn opts(s: &Scratch) -> StartOpts {
    StartOpts { dir: Some(s.wt_root.clone()), ..StartOpts::default() }
}

/// Claim K-1, work it in its worktree (one real commit), and close out to review. Returns the branch.
///
/// No `worktree::finish` here: moving to review *is* the close-out now, and the worktree deliberately survives it.
fn work_k1_to_review(s: &Scratch) -> String {
    let id = TicketId("K-1".into());
    ops::apply(&s.store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    let report = worktree::start(&s.store, &id, &opts(s)).unwrap();
    fs::write(report.path.join("foundation.txt"), "the foundation\n").unwrap();
    sh(&report.path, "git", &["add", "-A"]);
    sh(&report.path, "git", &["commit", "-qm", "feat: foundation"]);
    ops::apply(&s.store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
    report.branch
}

/// Step one of every by-hand integration now that worktrees outlive review: git flatly refuses to check out a branch
/// that is live in a linked worktree, so the checkout has to go first. This is what `merge.sh` does for the user and
/// what `/kanban:work`'s auto-merge does via `kanban_worktree_finish` — both refuse on uncommitted work, as here.
fn release_worktree_for_merge(s: &Scratch) {
    worktree::finish(&s.store, &TicketId("K-1".into()), false, false).unwrap();
}

#[test]
fn a_dependent_unblocks_only_when_the_code_lands_and_then_its_worktree_contains_it() {
    let s = scratch();
    let branch = work_k1_to_review(&s);

    // Code-complete is not done: K-1 sits in review, K-2 stays blocked, the board has nothing to offer.
    let board = s.store.read_board().unwrap();
    assert!(matches!(board.ticket(&TicketId("K-1".into())).unwrap().column, Column::Review { .. }));
    assert!(derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board));
    assert!(derive::next_ticket(&board, &[]).is_none(), "nothing is eligible while the predecessor is unlanded");
    assert_eq!(land::sweep(&s.store).unwrap(), 0, "no proof yet — the sweep must not move anything");

    // A sweep observed the live branch tip above; now the user lands it exactly the way merge.sh does: release the
    // ticket's worktree, rebase onto main (main has moved, so every sha is rewritten), fast-forward, delete the branch.
    release_worktree_for_merge(&s);
    fs::write(s.repo.join("drift.txt"), "mainline moved\n").unwrap();
    sh(&s.repo, "git", &["add", "-A"]);
    sh(&s.repo, "git", &["commit", "-qm", "chore: mainline moves on"]);
    sh(&s.repo, "git", &["checkout", "-q", &branch]);
    sh(&s.repo, "git", &["rebase", "-q", "main"]);
    sh(&s.repo, "git", &["checkout", "-q", "main"]);
    sh(&s.repo, "git", &["merge", "-q", "--ff-only", &branch]);
    sh(&s.repo, "git", &["branch", "-q", "-d", &branch]);

    // The sweep proves the landing by patch-equivalence and K-1 lands; K-2 unblocks.
    assert_eq!(land::sweep(&s.store).unwrap(), 1);
    let board = s.store.read_board().unwrap();
    let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
    assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
    assert!(k1.notes.last().unwrap().text.contains("rebased into main"), "{:?}", k1.notes);
    assert_eq!(derive::next_ticket(&board, &[]).unwrap().id.0, "K-2");

    // And the point of it all: K-2's fresh worktree, based off main, CONTAINS K-1's work.
    let id = TicketId("K-2".into());
    ops::apply(&s.store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    assert!(report.path.join("foundation.txt").exists(), "the dependent's worktree must contain the predecessor's landed code");
}

/// The auto-merge close-out of `/kanban:work`, in the order the command file insists on: rebase, fast-forward, and
/// leave the branch alone until the sweep has landed the card. The live ref is what makes rule 1 — plain ancestry —
/// available, so the landing needs no earlier observation and no `land-state.json` entry at all.
#[test]
fn auto_merge_lands_by_ancestry_while_the_branch_is_still_alive() {
    let s = scratch();
    let branch = work_k1_to_review(&s);

    // Mainline moves under the ticket, so the rebase genuinely rewrites its shas.
    fs::write(s.repo.join("drift.txt"), "mainline moved\n").unwrap();
    sh(&s.repo, "git", &["add", "drift.txt"]);
    sh(&s.repo, "git", &["commit", "-qm", "chore: mainline moves on"]);

    // Steps 3-4 of the procedure. Step 5's `git branch -d` deliberately does NOT run yet.
    release_worktree_for_merge(&s);
    sh(&s.repo, "git", &["checkout", "-q", &branch]);
    sh(&s.repo, "git", &["rebase", "-q", "--autostash", "main"]);
    sh(&s.repo, "git", &["checkout", "-q", "main"]);
    sh(&s.repo, "git", &["merge", "-q", "--ff-only", &branch]);

    // No prior sweep ever ran, so there is no observed tip to fall back on — ancestry alone carries this.
    assert!(s.store.read_land_state().unwrap().is_empty(), "nothing observed: the landing must rest on the live ref");
    assert_eq!(land::sweep(&s.store).unwrap(), 1);

    let board = s.store.read_board().unwrap();
    let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
    assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
    assert!(k1.notes.last().unwrap().text.contains("merged into main"), "{:?}", k1.notes);
    assert!(!derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board));
    assert_eq!(derive::next_ticket(&board, &[]).unwrap().id.0, "K-2", "the dependent unblocked");
}

/// The negative twin, and the whole reason step 5 deletes the branch last: delete before any sweep has ticked and the
/// merge leaves no proof behind. Ancestry is unavailable (the ref is gone) and the observed-tip fallback was never
/// populated, so the card sits in review wearing "branch gone" and waits for a human.
#[test]
fn auto_merge_that_deletes_the_branch_too_early_strands_the_ticket_in_review() {
    let s = scratch();
    let branch = work_k1_to_review(&s);

    fs::write(s.repo.join("drift.txt"), "mainline moved\n").unwrap();
    sh(&s.repo, "git", &["add", "drift.txt"]);
    sh(&s.repo, "git", &["commit", "-qm", "chore: mainline moves on"]);

    release_worktree_for_merge(&s);
    sh(&s.repo, "git", &["checkout", "-q", &branch]);
    sh(&s.repo, "git", &["rebase", "-q", "--autostash", "main"]);
    sh(&s.repo, "git", &["checkout", "-q", "main"]);
    sh(&s.repo, "git", &["merge", "-q", "--ff-only", &branch]);
    sh(&s.repo, "git", &["branch", "-q", "-d", &branch]); // out of order: the sweep never got to see the ref

    assert_eq!(land::sweep(&s.store).unwrap(), 0, "the code is in main, but nothing left in the repo proves it");
    let board = s.store.read_board().unwrap();
    assert!(matches!(board.ticket(&TicketId("K-1".into())).unwrap().column, Column::Review { .. }));
    assert!(derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board), "the dependent stays blocked");
}

#[test]
fn a_discarded_predecessor_keeps_its_dependent_blocked_for_good() {
    let s = scratch();
    work_k1_to_review(&s);

    ops::apply(&s.store, None, Op::DiscardTicket { id: TicketId("K-1".into()), reason: "superseded".into() }).unwrap();
    let board = s.store.read_board().unwrap();
    assert!(matches!(board.ticket(&TicketId("K-1".into())).unwrap().column, Column::Done { discarded: true, .. }));
    assert!(derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board), "the promised code never landed");
    assert!(derive::next_ticket(&board, &[]).is_none());

    // Sweeps change nothing: the ticket is closed, and even its branch later merging would not resurrect it.
    assert_eq!(land::sweep(&s.store).unwrap(), 0);
    let board = s.store.read_board().unwrap();
    assert!(derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board));

    // Claiming the blocked dependent is refused — the human has to untangle a discarded dependency deliberately.
    let err = ops::apply(&s.store, None, Op::Claim { id: TicketId("K-2".into()), agent: "claude".into() }).unwrap_err();
    assert!(err.to_string().contains("blocked"), "{err}");
}

// ---- the worktree's own lifecycle ----------------------------------------------------------------------------------

/// The change this file's helper now encodes: closing out to review leaves the worktree standing, so a reviewer has the
/// code on disk and a rework round costs nothing to resume.
#[test]
fn an_unlanded_review_ticket_keeps_its_worktree() {
    let s = scratch();
    let branch = work_k1_to_review(&s);
    let id = TicketId("K-1".into());

    let path = worktree::path_for(&s.store, &id).unwrap().expect("the worktree must survive the close-out");
    assert!(path.exists(), "review is where a human reads the code — it has to still be there");
    assert!(path.join("foundation.txt").exists(), "and it must still hold the ticket's work");

    // Re-claiming for rework re-attaches to the very same checkout rather than building a second one.
    ops::apply(&s.store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    let again = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    assert!(again.reattached, "rework must re-attach, not re-create");
    assert_eq!(again.path, path);
    assert_eq!(again.branch, branch);
}

/// Landing is what finally retires it — and the branch, being the provenance the card keeps, is left alone.
#[test]
fn a_landed_ticket_loses_its_worktree_and_keeps_its_branch() {
    let s = scratch();
    let branch = work_k1_to_review(&s);
    let id = TicketId("K-1".into());
    let path = worktree::path_for(&s.store, &id).unwrap().unwrap();

    sh(&s.repo, "git", &["merge", "-q", "--ff-only", &branch]);
    assert_eq!(land::sweep(&s.store).unwrap(), 1);

    let board = s.store.read_board().unwrap();
    let k1 = board.ticket(&id).unwrap();
    assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
    assert_eq!(k1.column.branch(), Some(branch.as_str()), "the branch is provenance and survives");
    assert!(!path.exists(), "the worktree retires with the landing");
    assert!(worktree::path_for(&s.store, &id).unwrap().is_none(), "and git no longer registers one");
}

/// Discard is the other terminal verdict, so it retires the worktree the same way.
#[test]
fn a_discarded_ticket_loses_its_worktree_too() {
    let s = scratch();
    work_k1_to_review(&s);
    let id = TicketId("K-1".into());
    let path = worktree::path_for(&s.store, &id).unwrap().unwrap();

    ops::apply(&s.store, None, Op::DiscardTicket { id: id.clone(), reason: "superseded".into() }).unwrap();
    worktree::retire(&s.store, &id);

    assert!(!path.exists(), "a discarded ticket's checkout is finished with too");
}

/// The one thing retirement will not do. Uncommitted work outlives the landing, and the card says where it is — the
/// ticket is already `done`, so refusing is not an option, and deleting somebody's work is never one.
#[test]
fn a_dirty_worktree_survives_landing_and_the_card_says_so() {
    let s = scratch();
    let branch = work_k1_to_review(&s);
    let id = TicketId("K-1".into());
    let path = worktree::path_for(&s.store, &id).unwrap().unwrap();
    fs::write(path.join("half-finished.txt"), "not committed\n").unwrap();

    sh(&s.repo, "git", &["merge", "-q", "--ff-only", &branch]);
    assert_eq!(land::sweep(&s.store).unwrap(), 1, "a stubborn worktree must never block the landing");

    let board = s.store.read_board().unwrap();
    let k1 = board.ticket(&id).unwrap();
    assert!(matches!(k1.column, Column::Done { discarded: false, .. }), "the ticket still landed");
    assert!(path.exists(), "the uncommitted work is still on disk");
    assert!(path.join("half-finished.txt").exists());
    let note = k1.notes.last().unwrap();
    assert_eq!(note.author.as_deref(), Some("kanban"));
    assert!(note.text.contains("uncommitted changes"), "{note:?}");
    assert!(note.text.contains(&path.display().to_string()), "the note must name the path: {note:?}");
}

/// The close-out guard's engine: `kanban_move to=review` asks this, and refuses when it answers.
#[test]
fn a_dirty_worktree_is_reported_so_the_close_out_move_can_refuse() {
    let s = scratch();
    let id = TicketId("K-1".into());
    ops::apply(&s.store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    assert!(worktree::dirty_worktree(&s.store, &id).is_none(), "a fresh worktree is clean");

    fs::write(report.path.join("uncommitted.txt"), "work in progress\n").unwrap();
    assert_eq!(worktree::dirty_worktree(&s.store, &id).as_deref(), Some(report.path.as_path()));

    sh(&report.path, "git", &["add", "-A"]);
    sh(&report.path, "git", &["commit", "-qm", "feat: commit it"]);
    assert!(worktree::dirty_worktree(&s.store, &id).is_none(), "committing clears the way to review");
}
