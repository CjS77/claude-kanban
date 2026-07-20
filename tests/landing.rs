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

/// Claim K-1, work it in its worktree (one real commit), finish, and close out to review. Returns the branch.
fn work_k1_to_review(s: &Scratch) -> String {
    let id = TicketId("K-1".into());
    ops::apply(&s.store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    let report = worktree::start(&s.store, &id, &opts(s)).unwrap();
    fs::write(report.path.join("foundation.txt"), "the foundation\n").unwrap();
    sh(&report.path, "git", &["add", "-A"]);
    sh(&report.path, "git", &["commit", "-qm", "feat: foundation"]);
    worktree::finish(&s.store, &id, false, false).unwrap();
    ops::apply(&s.store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
    report.branch
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

    // A sweep observed the live branch tip above; now the user lands it exactly the way merge.sh does: rebase onto
    // main (main has moved, so every sha is rewritten), fast-forward main, delete the branch.
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
