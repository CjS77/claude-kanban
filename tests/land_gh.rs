//! `land::poll` end to end without anything leaving the machine: the remote is a local bare repository and `gh` is a
//! shim answering canned JSON while logging its argv. The crate forbids `unsafe`, so PATH is never mutated in-process:
//! the driver re-execs this very test binary against the ignored `land_gh_inner` helper with a doctored PATH.

use std::{os::unix::fs::PermissionsExt, path::Path};

use claude_kanban::{
    git::git,
    land,
    ops::{self, Op},
    store::{
        Store,
        model::{Column, ColumnId, PrState, Status, TicketId},
    },
};

/// Run [`land::poll`] then [`land::sweep`] in a child whose PATH is `path`, reporting both counts as one line.
fn poll_with_path(path: &str, store_dir: &Path, out: &Path) -> String {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "land_gh_inner", "--ignored"])
        .env("PATH", path)
        .env("LAND_GH_STORE", store_dir)
        .env("LAND_GH_OUT", out)
        .status()
        .unwrap();
    assert!(status.success(), "the inner helper must run to completion");
    std::fs::read_to_string(out).unwrap()
}

/// The driver's other half: one poll, one sweep, counts written out. A no-op under a plain `--ignored` run.
#[test]
#[ignore = "helper — run by the_poll_discovers_records_notes_and_lands in a child process"]
fn land_gh_inner() {
    let Ok(store_dir) = std::env::var("LAND_GH_STORE") else { return };
    let store = Store::at(store_dir);
    let line = match (land::poll(&store), land::sweep(&store)) {
        (Ok(p), Ok(s)) => format!("polled={p} swept={s}"),
        (p, s) => format!("err poll={p:?} sweep={s:?}"),
    };
    std::fs::write(std::env::var("LAND_GH_OUT").unwrap(), line).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
    let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
    git(repo, &args).unwrap();
}

fn seed_review_ticket(store: &Store, title: &str, branch: &str) -> TicketId {
    let applied = ops::apply(
        store,
        None,
        Op::CreateTicket { title: title.into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Ready, model: None, effort: None, auto_merge: false },
    )
    .unwrap();
    let id = TicketId(applied.created_ids[0].clone());
    ops::apply(store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    ops::apply(store, None, Op::MoveTicket { id: id.clone(), to: ColumnId::Review, position: None, owner: None, branch: Some(branch.into()) })
        .unwrap();
    id
}

#[test]
fn the_poll_discovers_records_notes_and_lands() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]).unwrap();
    commit(&repo, "seed");

    // K-1's real branch, plus a "staging" copy of what GitHub's squash-merge will eventually deliver: its oid is what
    // gh reports as the merge commit, and fast-forwarding main onto staging later is the local "git pull".
    git(&repo, &["checkout", "-q", "-b", "k-1/work"]).unwrap();
    std::fs::write(repo.join("work.txt"), "the work\n").unwrap();
    git(&repo, &["add", "work.txt"]).unwrap();
    commit(&repo, "feat: the work");
    git(&repo, &["checkout", "-q", "main"]).unwrap();
    git(&repo, &["checkout", "-q", "-b", "staging"]).unwrap();
    git(&repo, &["merge", "-q", "--squash", "k-1/work"]).unwrap();
    commit(&repo, "K-1: the work (#7)");
    let squash_oid = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(&repo, &["checkout", "-q", "main"]).unwrap();
    let main_oid = git(&repo, &["rev-parse", "HEAD"]).unwrap();

    git(scratch.path(), &["init", "-q", "--bare", "origin.git"]).unwrap();
    git(&repo, &["remote", "add", "origin", scratch.path().join("origin.git").to_str().unwrap()]).unwrap();

    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let k1 = seed_review_ticket(&store, "squash-merged via PR", "k-1/work");
    let k2 = seed_review_ticket(&store, "merged and already pulled", "k-2/side");
    let k3 = seed_review_ticket(&store, "PR closed without merge", "k-3/note");

    // The gh shim answers `pr list --head <branch>` from list-<branch>.json ('/' → '-') and `pr view <n>` from
    // view-<n>.json, logging every call.
    let shims = scratch.path().join("shims");
    let answers = scratch.path().join("answers");
    std::fs::create_dir_all(&shims).unwrap();
    std::fs::create_dir_all(&answers).unwrap();
    let log = scratch.path().join("gh.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
         \"pr list\") b=$(printf %s \"$4\" | tr / -); cat {answers}/list-$b.json 2>/dev/null || echo '[]' ;;\n\
         \"pr view\") cat {answers}/view-$3.json ;;\n\
         *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n",
        log = log.display(),
        answers = answers.display()
    );
    std::fs::write(shims.join("gh"), script).unwrap();
    std::fs::set_permissions(shims.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let real_path = std::env::var("PATH").unwrap();
    let shim_path = format!("{}:{real_path}", shims.display());
    let out = scratch.path().join("outcome");

    // Round 1 — discovery by branch, three shapes at once: K-1's PR is open (recorded, nothing more); K-2's PR merged
    // as a commit main already has (records AND lands in the same tick); K-3's PR closed without merging (records + one
    // note, stays in review).
    let url = |n: u64| format!("https://github.com/example/repo/pull/{n}");
    std::fs::write(answers.join("list-k-1-work.json"), format!(r#"[{{"number":7,"url":"{}","state":"OPEN","mergeCommit":null}}]"#, url(7))).unwrap();
    std::fs::write(
        answers.join("list-k-2-side.json"),
        format!(r#"[{{"number":8,"url":"{}","state":"MERGED","mergeCommit":{{"oid":"{main_oid}"}}}}]"#, url(8)),
    )
    .unwrap();
    std::fs::write(answers.join("list-k-3-note.json"), format!(r#"[{{"number":9,"url":"{}","state":"CLOSED","mergeCommit":null}}]"#, url(9))).unwrap();

    let outcome = poll_with_path(&shim_path, store.dir(), &out);
    assert_eq!(outcome, "polled=3 swept=0");
    let board = store.read_board().unwrap();
    let pr1 = board.ticket(&k1).unwrap().pr.as_ref().unwrap();
    assert_eq!((pr1.number, pr1.state), (7, PrState::Open));
    let t2 = board.ticket(&k2).unwrap();
    assert!(matches!(t2.column, Column::Done { discarded: false, .. }), "merged-and-pulled lands within the tick");
    assert!(t2.notes.last().unwrap().text.contains("PR #8 merged and pulled"), "{:?}", t2.notes);
    let t3 = board.ticket(&k3).unwrap();
    assert!(matches!(t3.column, Column::Review { .. }));
    let closed_notes = |t: &claude_kanban::store::model::Ticket| t.notes.iter().filter(|n| n.text.contains("closed without merging")).count();
    assert_eq!(closed_notes(t3), 1, "{:?}", t3.notes);

    // Round 2 — recorded PRs re-polled by number: K-1's PR merges on GitHub as the squash commit main does NOT yet
    // have (records the state, stays in review); K-3 unchanged (no second note).
    std::fs::write(
        answers.join("view-7.json"),
        format!(r#"{{"number":7,"url":"{}","state":"MERGED","mergeCommit":{{"oid":"{squash_oid}"}}}}"#, url(7)),
    )
    .unwrap();
    std::fs::write(answers.join("view-9.json"), format!(r#"{{"number":9,"url":"{}","state":"CLOSED","mergeCommit":null}}"#, url(9))).unwrap();

    let outcome = poll_with_path(&shim_path, store.dir(), &out);
    assert_eq!(outcome, "polled=1 swept=0", "merged-but-unpulled must not land");
    let board = store.read_board().unwrap();
    let t1 = board.ticket(&k1).unwrap();
    assert!(matches!(t1.column, Column::Review { .. }));
    assert_eq!(t1.pr.as_ref().unwrap().state, PrState::Merged);
    assert_eq!(closed_notes(board.ticket(&k3).unwrap()), 1, "the closed note fires once, not per poll");

    // Round 3 — the local "git pull": main fast-forwards onto the squash commit; the offline sweep lands K-1. The poll
    // itself skips K-1 now (merged is terminal for polling).
    git(&repo, &["merge", "-q", "--ff-only", "staging"]).unwrap();
    let outcome = poll_with_path(&shim_path, store.dir(), &out);
    assert_eq!(outcome, "polled=0 swept=1");
    let board = store.read_board().unwrap();
    assert!(matches!(board.ticket(&k1).unwrap().column, Column::Done { discarded: false, .. }));

    // Round 4 — gh vanishes (PATH holds git alone): the poll goes quiet, and the board is byte-identical afterwards.
    let real_git = String::from_utf8(std::process::Command::new("which").arg("git").output().unwrap().stdout).unwrap().trim().to_owned();
    let gitonly = scratch.path().join("gitonly");
    std::fs::create_dir_all(&gitonly).unwrap();
    std::os::unix::fs::symlink(&real_git, gitonly.join("git")).unwrap();
    let before = std::fs::read(store.board_path()).unwrap();
    let outcome = poll_with_path(gitonly.to_str().unwrap(), store.dir(), &out);
    assert_eq!(outcome, "polled=0 swept=0");
    assert_eq!(std::fs::read(store.board_path()).unwrap(), before, "no gh, no writes");
}
