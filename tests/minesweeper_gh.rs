//! The minesweeper delegation lifecycle end to end without anything leaving the machine: the remote is a local bare
//! repository and `gh` is a shim answering canned JSON while logging its argv. As in `land_gh.rs`, the crate forbids
//! `unsafe`, so PATH is never mutated in-process — the driver re-execs this test binary against the ignored
//! `minesweeper_gh_inner` helper with a doctored PATH.
#![cfg(feature = "minesweeper")]

use std::{os::unix::fs::PermissionsExt, path::Path};

use claude_kanban::{
    config::Config,
    git::git,
    land, minesweeper,
    ops::{self, Op},
    store::{
        Store,
        model::{Column, ColumnId, External, PrState, Status, TicketId},
    },
};

/// Run the inner helper in a child whose PATH is `path`: `delegate=<id>` fires the entering-doing hook for one ticket,
/// anything else runs one minesweeper poll followed by one offline sweep, reporting both counts.
fn run_with_path(path: &str, store_dir: &Path, out: &Path, mode: &str) -> String {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "minesweeper_gh_inner", "--ignored"])
        .env("PATH", path)
        .env("MS_GH_STORE", store_dir)
        .env("MS_GH_OUT", out)
        .env("MS_GH_MODE", mode)
        .status()
        .unwrap();
    assert!(status.success(), "the inner helper must run to completion");
    std::fs::read_to_string(out).unwrap()
}

/// The driver's other half. A no-op under a plain `--ignored` run.
#[test]
#[ignore = "helper — run by the delegation lifecycle test in a child process"]
fn minesweeper_gh_inner() {
    let Ok(store_dir) = std::env::var("MS_GH_STORE") else { return };
    let store = Store::at(store_dir);
    let mode = std::env::var("MS_GH_MODE").unwrap();
    let line = if let Some(id) = mode.strip_prefix("delegate=") {
        minesweeper::delegate_entering_doing(&store, &TicketId(id.into()));
        "delegated".to_owned()
    } else if let Some(id) = mode.strip_prefix("handover=") {
        minesweeper::hand_over(&store, &TicketId(id.into()));
        "handed".to_owned()
    } else {
        match (minesweeper::poll(&store), land::sweep(&store)) {
            (Ok(p), Ok(s)) => format!("polled={p} swept={s}"),
            (p, s) => format!("err poll={p:?} sweep={s:?}"),
        }
    };
    std::fs::write(std::env::var("MS_GH_OUT").unwrap(), line).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
    let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
    git(repo, &args).unwrap();
}

fn create_ticket(store: &Store, title: &str) -> TicketId {
    let applied = ops::apply(
        store,
        None,
        Op::CreateTicket { title: title.into(), body: "the spec".into(), epic: None, labels: vec![], depends_on: vec![], status: Status::Ready, model: None, effort: None, auto_merge: false },
    )
    .unwrap();
    TicketId(applied.created_ids[0].clone())
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_delegation_lifecycle_mirrors_tracks_flags_refines_and_lands() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]).unwrap();
    commit(&repo, "seed");

    // A "staging" branch models what GitHub's squash-merge of the daemon's PR will deliver: its oid is what the poll
    // reports as the merge commit, and fast-forwarding main onto it later is the local "git pull".
    git(&repo, &["checkout", "-q", "-b", "staging"]).unwrap();
    std::fs::write(repo.join("work.txt"), "the daemon's work\n").unwrap();
    git(&repo, &["add", "work.txt"]).unwrap();
    commit(&repo, "K-1: mirror me (#57)");
    let squash_oid = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(&repo, &["checkout", "-q", "main"]).unwrap();

    git(scratch.path(), &["init", "-q", "--bare", "origin.git"]).unwrap();
    git(&repo, &["remote", "add", "origin", scratch.path().join("origin.git").to_str().unwrap()]).unwrap();

    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    let mut config = Config::load(store.dir()).unwrap();
    config.minesweeper = Some(true);
    store.write_config(&config).unwrap();

    // The gh shim: canned JSON per subcommand, every call logged, issue creations logged separately.
    let shims = scratch.path().join("shims");
    let answers = scratch.path().join("answers");
    std::fs::create_dir_all(&shims).unwrap();
    std::fs::create_dir_all(&answers).unwrap();
    let log = scratch.path().join("gh.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
         \"issue list\") cat {answers}/search.json 2>/dev/null || echo '[]' ;;\n\
         \"label create\") : ;;\n\
         \"issue create\") echo \"$@\" >> {answers}/created.log; cat {answers}/issue-url.txt 2>/dev/null || echo 'https://github.com/example/repo/issues/42' ;;\n\
         \"issue view\") cat {answers}/issue-$3.json ;;\n\
         \"repo view\") echo '{{\"owner\":{{\"login\":\"example\"}},\"name\":\"repo\"}}' ;;\n\
         \"api graphql\") cat {answers}/graphql.json ;;\n\
         *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n",
        log = log.display(),
        answers = answers.display()
    );
    std::fs::write(shims.join("gh"), script).unwrap();
    std::fs::set_permissions(shims.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let shim_path = format!("{}:{}", shims.display(), std::env::var("PATH").unwrap());
    let out = scratch.path().join("outcome");
    let gh_calls = |pattern: &str| std::fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains(pattern)).count();

    // (a) Claiming a ready ticket delegates it: dedup search misses, the label is ensured, the issue is created with
    // the spec and the footer, and the board records the binding with minesweeper as the owner.
    let k1 = create_ticket(&store, "mirror me");
    ops::apply(&store, None, Op::Claim { id: k1.clone(), agent: "claude".into() }).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "delegate=K-1"), "delegated");
    let created = std::fs::read_to_string(answers.join("created.log")).unwrap();
    assert!(created.contains("K-1: mirror me") && created.contains("Mirrored from kanban ticket K-1.") && created.contains("--label autofix"), "{created}");
    let board = store.read_board().unwrap();
    let t1 = board.ticket(&k1).unwrap();
    assert_eq!(t1.external.as_ref().unwrap().number, 42);
    assert!(matches!(&t1.column, Column::Doing { owner, .. } if owner == "minesweeper"));
    assert!(t1.notes.last().unwrap().text.contains("issue #42"), "{:?}", t1.notes);
    assert_eq!(store.read_claims().unwrap().iter().find(|c| c.ticket == k1).unwrap().agent, "minesweeper");

    // (b) The crash-recovery path: an issue whose footer matches already exists (created before a crash could bind it).
    // The dedup search adopts it — no second issue.
    let k2 = create_ticket(&store, "crashed mid-delegation");
    ops::apply(&store, None, Op::Claim { id: k2.clone(), agent: "claude".into() }).unwrap();
    std::fs::write(answers.join("search.json"), r#"[{"number":43,"url":"https://github.com/example/repo/issues/43"}]"#).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "delegate=K-2"), "delegated");
    assert_eq!(gh_calls("issue create"), 1, "the existing mirror is adopted, never duplicated");
    assert_eq!(store.read_board().unwrap().ticket(&k2).unwrap().external.as_ref().unwrap().number, 43);

    // (c) The poll finds the daemon's PR for issue #42 (merged on GitHub, not yet pulled): K-1 moves to review carrying
    // the PR's head branch, in one batched graphql call. K-2's issue reports nothing — no change.
    let pr57 = format!(
        r#"{{"nodes":[{{"number":57,"url":"https://github.com/example/repo/pull/57","state":"MERGED","merged":true,"mergeCommit":{{"oid":"{squash_oid}"}},"headRefName":"repo-issue0042"}}]}}"#
    );
    let issue = |n: u64, state: &str, labels: &str, comments: &str, prs: &str| {
        format!(
            r#""i{n}": {{"number":{n},"state":"{state}","labels":{{"nodes":[{labels}]}},"comments":{{"nodes":[{comments}]}},"closedByPullRequestsReferences":{prs}}}"#
        )
    };
    let graphql = |issues: &[String]| format!(r#"{{"data":{{"repository":{{{}}}}}}}"#, issues.join(","));
    let empty_prs = r#"{"nodes":[]}"#;
    std::fs::write(
        answers.join("graphql.json"),
        graphql(&[issue(42, "OPEN", "", "", &pr57), issue(43, "OPEN", "", "", empty_prs)]),
    )
    .unwrap();
    let calls_before = gh_calls("api graphql");
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=1 swept=0", "merged-but-unpulled must not land");
    assert_eq!(gh_calls("api graphql"), calls_before + 1, "one batched call covers every delegated issue");
    let board = store.read_board().unwrap();
    let t1 = board.ticket(&k1).unwrap();
    assert!(matches!(&t1.column, Column::Review { branch: Some(b) } if b == "repo-issue0042"), "{:?}", t1.column);
    let pr = t1.pr.as_ref().unwrap();
    assert_eq!((pr.number, pr.state), (57, PrState::Merged));

    // The local "git pull": the offline sweep lands K-1 by the ordinary PR route — the full doing → done journey.
    git(&repo, &["merge", "-q", "--ff-only", "staging"]).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=0 swept=1");
    assert!(matches!(store.read_board().unwrap().ticket(&k1).unwrap().column, Column::Done { discarded: false, .. }));

    // (d) A flag label appears on K-2's issue: recorded once, noted once, and the card stays in doing.
    std::fs::write(answers.join("graphql.json"), graphql(&[issue(43, "OPEN", r#"{"name":"minesweeperFailed"}"#, "", empty_prs)])).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=1 swept=0");
    let flagged_notes = |t: &claude_kanban::store::model::Ticket| t.notes.iter().filter(|n| n.text.contains("minesweeperFailed")).count();
    let board = store.read_board().unwrap();
    let t2 = board.ticket(&k2).unwrap();
    assert!(matches!(t2.column, Column::Doing { .. }), "flagged tickets stay in doing — the human decides");
    assert_eq!(t2.external.as_ref().unwrap().flag.as_deref(), Some("minesweeperFailed"));
    assert_eq!(flagged_notes(t2), 1, "{:?}", t2.notes);
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=0 swept=0", "an unchanged flag writes nothing");
    assert_eq!(flagged_notes(store.read_board().unwrap().ticket(&k2).unwrap()), 1, "the note fires on the transition, not per tick");

    // Its issue then closes with no PR ever appearing: the abandonment is noted, and the card still stays put.
    std::fs::write(answers.join("graphql.json"), graphql(&[issue(43, "CLOSED", r#"{"name":"minesweeperFailed"}"#, "", empty_prs)])).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=1 swept=0");
    let t2 = store.read_board().unwrap();
    let t2 = t2.ticket(&k2).unwrap();
    assert!(matches!(t2.column, Column::Doing { .. }));
    assert!(t2.notes.last().unwrap().text.contains("closed without a PR"), "{:?}", t2.notes);

    // (e) A refine split: the daemon commented the checklist on K-3's issue #44 and opened sub-issues #45/#46. The poll
    // mirrors them as claimed doing tickets and parks the parent in review; closing the issue then lands it by rule 6.
    let k3 = create_ticket(&store, "too big — got refined");
    ops::apply(&store, None, Op::BindExternal { id: k3.clone(), external: Some(External::github_issue(44)) }).unwrap();
    ops::apply(&store, None, Op::Claim { id: k3.clone(), agent: "minesweeper".into() }).unwrap();
    let refine = r#"{"body":"Refined into the following sub-tasks:\n\n- [ ] #45 — first half\n- [ ] #46 — second half"}"#;
    std::fs::write(answers.join("graphql.json"), graphql(&[issue(44, "OPEN", "", refine, empty_prs)])).unwrap();
    std::fs::write(answers.join("issue-45.json"), r#"{"title":"first half","body":"spec 45"}"#).unwrap();
    std::fs::write(answers.join("issue-46.json"), r#"{"title":"second half","body":"spec 46"}"#).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=1 swept=0");
    let board = store.read_board().unwrap();
    let t3 = board.ticket(&k3).unwrap();
    assert!(matches!(t3.column, Column::Review { branch: None }), "{:?}", t3.column);
    assert_eq!(t3.external.as_ref().unwrap().sub_issues, vec![45, 46]);
    assert_eq!(t3.depends_on.len(), 2);
    let children: Vec<&claude_kanban::store::model::Ticket> = t3.depends_on.iter().map(|id| board.ticket(id).unwrap()).collect();
    assert!(children.iter().all(|c| matches!(&c.column, Column::Doing { owner, .. } if owner == "minesweeper")));

    // The children land (their PRs merged and pulled, compressed here to direct moves), the human closes issue #44 —
    // and the sweep finishes the parent without any PR of its own.
    for child in t3.depends_on.clone() {
        ops::apply(&store, None, Op::MoveTicket { id: child, to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();
    }
    std::fs::write(answers.join("graphql.json"), graphql(&[issue(44, "CLOSED", "", refine, empty_prs)])).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=1 swept=1");
    let board = store.read_board().unwrap();
    let t3 = board.ticket(&k3).unwrap();
    assert!(matches!(t3.column, Column::Done { discarded: false, .. }), "{:?}", t3.column);
    assert!(t3.notes.last().unwrap().text.contains("issue #44 closed and all 2"), "{:?}", t3.notes);

    // (f) Toggle off: neither the hook nor the poll touches gh, and a fresh claim stays undelegated.
    let mut config = Config::load(store.dir()).unwrap();
    config.minesweeper = Some(false);
    store.write_config(&config).unwrap();
    std::fs::write(&log, "").unwrap();
    let k6 = create_ticket(&store, "worked locally again");
    ops::apply(&store, None, Op::Claim { id: k6.clone(), agent: "claude".into() }).unwrap();
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "delegate=K-6"), "delegated");
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "poll"), "polled=0 swept=0");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "", "toggle off means zero gh invocations");
    let board = store.read_board().unwrap();
    assert!(board.ticket(&k6).unwrap().external.is_none());
    assert!(matches!(&board.ticket(&k6).unwrap().column, Column::Doing { owner, .. } if owner == "claude"));

    // (g) The create-modal handoff checkbox works with the toggle STILL OFF — ticking the box is its own opt-in:
    // claim for the daemon, mirror, bind, all in one go.
    std::fs::remove_file(answers.join("search.json")).unwrap();
    std::fs::write(answers.join("issue-url.txt"), "https://github.com/example/repo/issues/47").unwrap();
    let k7 = create_ticket(&store, "hand me over");
    assert_eq!(run_with_path(&shim_path, store.dir(), &out, "handover=K-7"), "handed");
    let created = std::fs::read_to_string(answers.join("created.log")).unwrap();
    assert!(created.contains("K-7: hand me over"), "{created}");
    let board = store.read_board().unwrap();
    let t7 = board.ticket(&k7).unwrap();
    assert_eq!(t7.external.as_ref().unwrap().number, 47);
    assert!(matches!(&t7.column, Column::Doing { owner, .. } if owner == "minesweeper"));
    assert!(t7.notes.last().unwrap().text.contains("issue #47"), "{:?}", t7.notes);
    assert_eq!(store.read_claims().unwrap().iter().find(|c| c.ticket == k7).unwrap().agent, "minesweeper");
}
