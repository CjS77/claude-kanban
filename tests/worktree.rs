//! Worktree lifecycle tests against real scratch git repos: sparse checkout excluding `.kanban/`, idempotent re-attach,
//! /tmp-wipe recovery, dirty-finish refusal, `--merge`, external refusal, and the gitignore gate on copied files.

use std::{fs, path::Path, process::Command};

use claude_kanban::{
    ops::{self, Op},
    store::{Store, model::{Column, Status, TicketId}},
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

/// A committed scratch repo with an initialised board and one claimed ready ticket (K-1).
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

    ops::apply(
        &store,
        None,
        Op::CreateTicket {
            title: "Rate-limit the login route".into(),
            body: String::new(),
            epic: None,
            labels: vec![],
            depends_on: vec![],
            status: Status::Ready,
            model: None,
            effort: None,
        },
    )
    .unwrap();
    ops::apply(&store, None, Op::Claim { id: TicketId("K-1".into()), agent: "claude".into() }).unwrap();

    let wt_root = dir.path().join("worktrees");
    Scratch { _dir: dir, repo, wt_root, store }
}

fn opts(s: &Scratch) -> StartOpts {
    StartOpts { dir: Some(s.wt_root.clone()), ..StartOpts::default() }
}

#[test]
fn start_creates_a_sparse_worktree_without_the_board() {
    let s = scratch();
    let report = worktree::start(&s.store, &TicketId("K-1".into()), &opts(&s)).unwrap();

    assert_eq!(report.branch, "k-1/rate-limit-login");
    assert!(!report.reattached);
    assert!(report.sparse, "git on this machine is ≥ 2.36");
    assert!(report.path.join("README.md").exists(), "tracked files are checked out");
    assert!(!report.path.join(".kanban").exists(), "the sparse checkout excludes .kanban/ entirely");

    // The stamps: branch on the doing column, path on the claim.
    let board = s.store.read_board().unwrap();
    assert_eq!(board.tickets[0].column.branch(), Some("k-1/rate-limit-login"));
    let claims = s.store.read_claims().unwrap();
    assert_eq!(claims[0].path.as_deref(), Some(report.path.as_path()));
}

#[test]
fn start_is_idempotent_and_recovers_from_a_wipe() {
    let s = scratch();
    let id = TicketId("K-1".into());
    let first = worktree::start(&s.store, &id, &opts(&s)).unwrap();

    // Running start again on a live worktree: same branch, same path, no error.
    let again = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    assert!(again.reattached);
    assert_eq!(again.path, first.path);
    assert_eq!(again.branch, first.branch);

    // A /tmp wipe: the directory vanishes, the branch survives. start prunes and re-attaches to the SAME branch.
    fs::remove_dir_all(&s.wt_root).unwrap();
    let recovered = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    assert!(recovered.reattached);
    assert_eq!(recovered.branch, first.branch);
    assert!(recovered.path.join("README.md").exists());
}

#[test]
fn finish_refuses_dirty_then_discards_on_request_and_keeps_the_branch() {
    let s = scratch();
    let id = TicketId("K-1".into());
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    fs::write(report.path.join("uncommitted.txt"), "precious?").unwrap();

    let err = worktree::finish(&s.store, &id, false, false).unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");

    let done = worktree::finish(&s.store, &id, true, false).unwrap();
    assert_eq!(done.removed.as_deref(), Some(report.path.as_path()));
    assert!(!report.path.exists());
    assert_eq!(done.branch.as_deref(), Some("k-1/rate-limit-login"));

    // The branch survives; the claim survives but loses its path (the ticket is in flight until moved to done).
    let branches = Command::new("git").current_dir(&s.repo).args(["branch", "--list", "k-1/*"]).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains("k-1/rate-limit-login"));
    let claims = s.store.read_claims().unwrap();
    assert_eq!(claims.len(), 1);
    assert!(claims[0].path.is_none());
}

#[test]
fn finish_with_merge_lands_the_commits_on_the_current_branch() {
    let s = scratch();
    let id = TicketId("K-1".into());
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();

    fs::write(report.path.join("feature.txt"), "the work\n").unwrap();
    sh(&report.path, "git", &["add", "-A"]);
    sh(&report.path, "git", &["commit", "-qm", "feat: the work"]);

    let done = worktree::finish(&s.store, &id, false, true).unwrap();
    assert!(done.merged);
    assert!(s.repo.join("feature.txt").exists(), "the merge landed on the main checkout's branch");
}

#[test]
fn merge_refuses_when_the_main_checkout_is_not_on_the_main_branch() {
    let s = scratch();
    let id = TicketId("K-1".into());
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();
    fs::write(report.path.join("feature.txt"), "the work\n").unwrap();
    sh(&report.path, "git", &["add", "-A"]);
    sh(&report.path, "git", &["commit", "-qm", "feat: the work"]);

    // Park the main checkout on a side branch: --merge must refuse rather than land the ticket somewhere random.
    sh(&s.repo, "git", &["checkout", "-qb", "elsewhere"]);
    let err = worktree::finish(&s.store, &id, false, true).unwrap_err();
    assert!(err.to_string().contains("'elsewhere', not 'main'"), "{err}");

    sh(&s.repo, "git", &["checkout", "-q", "main"]);
    assert!(worktree::finish(&s.store, &id, false, true).unwrap().merged, "back on main, the merge proceeds");
}

#[test]
fn merge_refuses_when_the_main_checkout_is_dirty() {
    let s = scratch();
    let id = TicketId("K-1".into());
    worktree::start(&s.store, &id, &opts(&s)).unwrap();
    fs::write(s.repo.join("README.md"), "# dirtied\n").unwrap();
    let err = worktree::finish(&s.store, &id, false, true).unwrap_err();
    assert!(err.to_string().contains("main checkout has uncommitted changes"), "{err}");
}

#[test]
fn unclaimed_and_external_tickets_are_refused() {
    let s = scratch();
    ops::apply(
        &s.store,
        None,
        Op::CreateTicket { title: "not claimed".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Ready, model: None, effort: None },
    )
    .unwrap();
    let err = worktree::start(&s.store, &TicketId("K-2".into()), &opts(&s)).unwrap_err();
    assert!(err.to_string().contains("claim it first"), "{err}");

    // An external ticket is claimable but never gets a worktree here.
    s.store
        .mutate(None, |board, _| {
            let t = board.ticket_mut(&TicketId("K-2".into())).unwrap();
            t.external = Some(claude_kanban::store::model::External { provider: "github".into(), kind: "issue".into(), number: 42 });
            t.column = Column::Doing { owner: "minesweeper".into(), branch: None };
            Ok::<_, claude_kanban::store::StoreError>(())
        })
        .unwrap();
    let err = worktree::start(&s.store, &TicketId("K-2".into()), &opts(&s)).unwrap_err();
    assert!(err.to_string().contains("external"), "{err}");
}

#[test]
fn copy_to_worktrees_honours_the_gitignore_gate() {
    let s = scratch();
    fs::write(s.repo.join(".gitignore"), ".env\n").unwrap();
    fs::write(s.repo.join(".env"), "SECRET=1\n").unwrap();
    fs::write(s.repo.join("tracked.txt"), "not ignored\n").unwrap();
    sh(&s.repo, "git", &["add", "-A"]);
    sh(&s.repo, "git", &["commit", "-qm", "add gitignore"]);
    fs::write(s.store.dir().join("config.json"), r#"{ "copy_to_worktrees": [".env", "tracked.txt", "absent.txt"] }"#).unwrap();

    let report = worktree::start(&s.store, &TicketId("K-1".into()), &opts(&s)).unwrap();
    assert!(report.path.join(".env").exists(), "authorised gitignored files are copied");
    assert_eq!(fs::read_to_string(report.path.join(".env")).unwrap(), "SECRET=1\n");
    assert_eq!(
        fs::read_to_string(report.path.join("tracked.txt")).unwrap(),
        "not ignored\n",
        "tracked files come from the checkout itself, not the copy step"
    );
    assert_eq!(report.warnings.len(), 2, "the non-ignored and the absent entries warn: {:?}", report.warnings);
}

#[test]
fn list_joins_worktrees_with_claims_and_flags_missing_paths() {
    let s = scratch();
    let id = TicketId("K-1".into());
    let report = worktree::start(&s.store, &id, &opts(&s)).unwrap();

    let rows = worktree::list(&s.store).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ticket.as_deref(), Some("K-1"));
    assert_eq!(rows[0].agent.as_deref(), Some("claude"));
    assert!(!rows[0].dirty && !rows[0].missing);

    fs::write(report.path.join("junk.txt"), "x").unwrap();
    assert!(worktree::list(&s.store).unwrap()[0].dirty);

    fs::remove_dir_all(&s.wt_root).unwrap();
    let rows = worktree::list(&s.store).unwrap();
    assert!(rows[0].missing, "a wiped path reads as missing, not as live work: {rows:?}");
}
