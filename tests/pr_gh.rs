//! `pr::create_pr` end to end without anything leaving the machine: the remote is a local bare repository, and `gh` is a
//! shim answering canned JSON/URLs while logging its argv. The crate forbids `unsafe`, so PATH is never mutated in-process:
//! the driver re-execs this very test binary against the ignored `pr_gh_inner` helper with a doctored PATH on the child.

use std::{os::unix::fs::PermissionsExt, path::Path};

use claude_kanban::{
    git::git,
    ops::{self, Op},
    pr,
    store::{
        Store,
        model::{ColumnId, Status, TicketId},
    },
};

const PR_URL: &str = "https://github.com/example/repo/pull/7";

/// Run [`pr::create_pr`] in a child process whose PATH is `path`, reporting the outcome as one line of text.
fn create_pr_with_path(path: &str, store_dir: &Path, out: &Path) -> String {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pr_gh_inner", "--ignored"])
        .env("PATH", path)
        .env("PR_GH_STORE", store_dir)
        .env("PR_GH_OUT", out)
        .status()
        .unwrap();
    assert!(status.success(), "the inner helper must run to completion");
    std::fs::read_to_string(out).unwrap()
}

/// The driver's other half: reads the store from the environment, runs `create_pr` for K-1, writes the outcome. A no-op
/// under a plain `cargo test -- --ignored` run (no environment, nothing to do).
#[test]
#[ignore = "helper — run by create_pr_pushes_once_dedupes_and_names_a_missing_gh in a child process"]
fn pr_gh_inner() {
    let Ok(store_dir) = std::env::var("PR_GH_STORE") else { return };
    let store = Store::at(store_dir);
    let line = match pr::create_pr(&store, &TicketId("K-1".into())) {
        Ok(r) => format!("ok created={} url={}", r.created, r.url),
        Err(e) => format!("err {e:#}"),
    };
    std::fs::write(std::env::var("PR_GH_OUT").unwrap(), line).unwrap();
}

#[test]
fn create_pr_pushes_once_dedupes_and_names_a_missing_gh() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    let bare = scratch.path().join("origin.git");

    // A repo with a done ticket's branch, its "remote" a bare sibling directory — pushes never leave the disk.
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]).unwrap();
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "seed"])
        .unwrap();
    git(&repo, &["branch", "k-1/work"]).unwrap();
    git(scratch.path(), &["init", "-q", "--bare", "origin.git"]).unwrap();
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]).unwrap();

    let store = Store::at(repo.join(".kanban"));
    store.init().unwrap();
    ops::apply(
        &store,
        None,
        Op::CreateTicket {
            title: "Work the thing".into(),
            body: "# Spec".into(),
            epic: None,
            labels: vec![],
            depends_on: vec![],
            status: Status::Ready,
            model: None,
            effort: None,
            auto_merge: false,
        },
    )
    .unwrap();
    let id = TicketId("K-1".into());
    ops::apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
    ops::apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "k-1/work".into(), path: "/tmp/unused".into() }).unwrap();
    ops::apply(&store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();

    // The gh shim: logs argv, answers `pr list` from a swappable file and `pr create` with the canned URL.
    let shims = scratch.path().join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    let log = scratch.path().join("gh.log");
    let list_file = scratch.path().join("pr-list.json");
    std::fs::write(&list_file, "[]").unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
         \"pr list\") cat {list} ;;\n\
         \"pr create\") echo {PR_URL} ;;\n\
         *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n",
        log = log.display(),
        list = list_file.display()
    );
    std::fs::write(shims.join("gh"), script).unwrap();
    std::fs::set_permissions(shims.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let store_dir = repo.join(".kanban");
    let out = scratch.path().join("outcome");
    let real_path = std::env::var("PATH").unwrap();
    let shim_path = format!("{}:{real_path}", shims.display());

    // Fresh branch: dedupe finds nothing, the push lands on the bare remote, the report carries gh's URL.
    let outcome = create_pr_with_path(&shim_path, &store_dir, &out);
    assert_eq!(outcome, format!("ok created=true url={PR_URL}"));
    git(&bare, &["rev-parse", "--quiet", "--verify", "refs/heads/k-1/work"]).expect("the branch must have reached the remote");

    // The click also bound the PR to the ticket — number parsed off the URL — so the poller can track it to its merge.
    let board = store.read_board().unwrap();
    let pr = board.tickets[0].pr.as_ref().expect("create_pr must record the PR on the ticket");
    assert_eq!((pr.number, pr.url.as_str()), (7, PR_URL));

    // An open PR for the branch: the second click reports it, and neither pushes nor creates again.
    std::fs::write(&list_file, format!(r#"[{{"number":7,"url":"{PR_URL}"}}]"#)).unwrap();
    let outcome = create_pr_with_path(&shim_path, &store_dir, &out);
    assert_eq!(outcome, format!("ok created=false url={PR_URL}"));
    let gh_log = std::fs::read_to_string(&log).unwrap();
    assert_eq!(gh_log.matches("pr create").count(), 1, "dedupe must prevent a second create: {gh_log}");

    // gh absent (PATH holds git alone): install advice, not a bare spawn error.
    let real_git = String::from_utf8(std::process::Command::new("which").arg("git").output().unwrap().stdout).unwrap().trim().to_owned();
    let gitonly = scratch.path().join("gitonly");
    std::fs::create_dir_all(&gitonly).unwrap();
    std::os::unix::fs::symlink(&real_git, gitonly.join("git")).unwrap();
    let outcome = create_pr_with_path(gitonly.to_str().unwrap(), &store_dir, &out);
    assert!(outcome.starts_with("err") && outcome.contains("GitHub CLI (gh) not found"), "{outcome}");
}
