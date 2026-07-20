//! Landing detection: how a review ticket earns its move to `done`.
//!
//! Done means *landed in the local main branch* (or explicitly discarded), so somebody has to notice the landing. Two
//! passes do, both driven from here:
//!
//! - [`sweep`] — the **offline** pass, pure git: land every review ticket whose code has provably reached the local
//!   main branch. Run by the serve poller every tick and by `kanban_next` before it computes eligibility, so dependents
//!   unblock even in an MCP-only session, with no network.
//! - [`poll`] — the **gh** pass, serve-only: keep each review ticket's PR binding fresh (`gh pr view`/`gh pr list`),
//!   recording state transitions on the board. Discovery is by branch when no PR is recorded, which is how skill- and
//!   daemon-created PRs get bound with no extra step.
//!
//! Three principles, settled with the user, bound everything here:
//!
//! 1. **Auto-landing requires positive proof.** A branch tip (or PR merge commit) that is an ancestor of local main, or
//!    a deleted branch whose last-observed tip proves patch-equivalent (`git cherry` — rebase-then-fast-forward keeps
//!    patch-ids). No proof → the ticket stays in review, flagged for the human.
//! 2. **Discard is always a human action.** The sweep never marks work discarded; ambiguity never silently unblocks —
//!    or blocks — anything.
//! 3. **External tickets never land from local branch state.** Their `branch` is whatever the delegate created on the
//!    far side and was never a local ref, so its absence proves nothing; only the PR route (rule 4) applies to them.
//!
//! Detection runs *outside* the store lock — git and gh are slow — and every landing goes through
//! [`Op::LandTicket`], which re-checks its evidence under the lock and refuses harmlessly when the board moved.
//!
//! The **observations sidecar** (`.kanban/land-state.json`, gitignored, machine-local like claims) records each live
//! review branch's tip per sweep. It is what makes the `merge.sh` flow land automatically: rebase onto main,
//! fast-forward, delete the branch — afterwards nothing named the branch exists, but the observed pre-rebase tip still
//! proves the landing by patch-id. No observation (another machine, gc'd objects) degrades to the human's call.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    config::Config,
    git,
    ops::{self, Op, OpError},
    pr,
    store::{
        Store,
        model::{Column, PrRef, PrState, Ticket},
    },
    worktree,
};

/// The repo and effective main branch, or `None` when landing detection has nothing to stand on (no repo, no
/// configured or detectable main branch) — every entry point degrades to a silent no-op then.
fn context(store: &Store) -> Option<(PathBuf, String)> {
    let repo = worktree::repo_root(store).ok()?;
    let main = Config::load(store.dir()).ok()?.main_branch(&repo)?;
    Some((repo, main))
}

/// One offline pass: land every review ticket with proof, then refresh the branch-tip observations. Returns how many
/// tickets landed. Never touches the network; errors that mean "cannot answer" skip the ticket rather than failing the
/// pass.
pub fn sweep(store: &Store) -> anyhow::Result<usize> {
    let Some((repo, main)) = context(store) else {
        tracing::debug!("landing sweep skipped — no repo or no main branch to land into");
        return Ok(0);
    };
    let board = store.read_board()?;
    let Some(heads) = git::local_heads(&repo) else {
        return Ok(0); // git can't list branches: without ground truth, "branch gone" would be a guess
    };
    let observations = store.read_land_state()?;

    let landed = board
        .tickets
        .iter()
        .filter(|t| matches!(t.column, Column::Review { .. }))
        .filter_map(|t| verdict(t, &repo, &main, &heads, &observations).map(|reason| (t, reason)))
        .filter(|(t, reason)| land(store, t, reason))
        .count();

    refresh_observations(store, &repo, &heads, observations)?;
    Ok(landed)
}

/// The proof rules, in order. `Some(reason)` means "land, and say why on the card".
fn verdict(t: &Ticket, repo: &Path, main: &str, heads: &HashSet<String>, observations: &HashMap<String, String>) -> Option<String> {
    // Rules 2-3: local branch evidence — never for external tickets (their branch was never a local ref).
    if t.external.is_none()
        && let Some(branch) = t.column.branch()
    {
        if heads.contains(branch) {
            if git::is_ancestor(repo, branch, main) == Some(true) {
                return Some(format!("branch {branch} merged into {main}"));
            }
        } else if let Some(tip) = observations.get(branch) {
            if git::is_ancestor(repo, tip, main) == Some(true) {
                return Some(format!("branch {branch} deleted after landing in {main}"));
            }
            if git::cherry_equivalent(repo, main, tip) == Some(true) {
                return Some(format!("branch {branch} rebased into {main} and deleted"));
            }
            // Observed but unproven: stays in review wearing the "branch gone" flag — the human decides.
        }
    }
    // Rule 4: the PR route, external tickets included. The gh poll recorded the merge; landing still waits for the
    // merge commit to reach the *local* main branch — origin/main is not done.
    if let Some(PrRef { number, state: PrState::Merged, merged_commit: Some(oid), .. }) = &t.pr
        && git::is_ancestor(repo, oid, main) == Some(true)
    {
        return Some(format!("PR #{number} merged and pulled into {main}"));
    }
    None
}

/// Apply one landing, tolerating the benign race: the board may have moved since the evidence was gathered, and
/// [`Op::LandTicket`] refusing is the mechanism working, not a failure.
fn land(store: &Store, t: &Ticket, reason: &str) -> bool {
    let op = Op::LandTicket { id: t.id.clone(), expected_branch: t.column.branch().map(str::to_owned), reason: reason.to_owned() };
    match ops::apply(store, None, op) {
        Ok(_) => true,
        Err(OpError::Invalid(e)) => {
            tracing::debug!(ticket = %t.id, "landing refused (board moved underneath the sweep): {e}");
            false
        }
        Err(e) => {
            tracing::warn!(ticket = %t.id, error = %e, "landing failed");
            false
        }
    }
}

/// Record the current tip of every live, non-external review branch and drop observations for branches no ticket in
/// review references any more. Reads the board *after* the landings so a landed ticket's observation retires with it.
fn refresh_observations(store: &Store, repo: &Path, heads: &HashSet<String>, mut observations: HashMap<String, String>) -> anyhow::Result<()> {
    let board = store.read_board()?;
    let review_branches: HashSet<String> = board
        .tickets
        .iter()
        .filter(|t| matches!(t.column, Column::Review { .. }) && t.external.is_none())
        .filter_map(|t| t.column.branch().map(str::to_owned))
        .collect();

    let before = observations.clone();
    observations.retain(|branch, _| review_branches.contains(branch));
    review_branches
        .iter()
        .filter(|b| heads.contains(*b))
        .filter_map(|b| git::git(repo, &["rev-parse", &format!("refs/heads/{b}")]).ok().map(|tip| (b.clone(), tip)))
        .for_each(|(branch, tip)| {
            observations.insert(branch, tip);
        });

    if observations != before {
        warn_if_not_ignored(store, repo);
        store.write_land_state(&observations)?;
    }
    Ok(())
}

/// The sidecar must stay machine-local. `init` seeds the ignore line, but boards initialised before it existed have a
/// `.gitignore` that `seed_if_absent` will never touch — say so once instead of silently committing observations.
fn warn_if_not_ignored(store: &Store, repo: &Path) {
    static WARNED: OnceLock<()> = OnceLock::new();
    let path = store.land_state_path();
    let Some(rel) = path.strip_prefix(repo).ok().and_then(Path::to_str) else { return };
    if git::git(repo, &["check-ignore", "-q", rel]).is_err() && WARNED.set(()).is_ok() {
        tracing::warn!("{rel} is not gitignored — add it to .kanban/.gitignore (it records machine-local branch observations)");
    }
}

/// One gh pass: refresh (or discover, by branch) the PR binding of every review ticket whose PR is not already merged,
/// note closed-without-merge transitions once, and land immediately when a recorded merge has already been pulled.
/// Returns how many tickets had their binding updated. Network trouble — gh missing, offline, rate-limited — warns once
/// per process and ends the pass with the board untouched; the next tick tries again.
pub fn poll(store: &Store) -> anyhow::Result<usize> {
    let Some((repo, main)) = context(store) else { return Ok(0) };
    if !pr::has_remote(&repo) {
        return Ok(0); // nowhere a PR could live
    }
    let board = store.read_board()?;
    let mut updated = 0;
    for t in board.tickets.iter().filter(|t| matches!(t.column, Column::Review { .. })) {
        // Merged is terminal for the poll — the sweep owns the rest of that ticket's journey.
        if matches!(&t.pr, Some(PrRef { state: PrState::Merged, .. })) {
            continue;
        }
        let fresh = match (&t.pr, t.column.branch()) {
            (Some(pr), _) => lookup(&repo, &["pr", "view", &pr.number.to_string(), "--json", GH_FIELDS]),
            (None, Some(branch)) => lookup(&repo, &["pr", "list", "--head", branch, "--state", "all", "--limit", "1", "--json", GH_FIELDS]),
            (None, None) => continue,
        };
        let fresh = match fresh {
            Ok(Some(pr)) => pr,
            Ok(None) => continue,
            Err(e) => {
                warn_once(&e);
                return Ok(updated);
            }
        };
        if t.pr.as_ref() == Some(&fresh) {
            continue;
        }

        let newly_closed = fresh.state == PrState::Closed && t.pr.as_ref().is_none_or(|old| old.state != PrState::Closed);
        let landable = fresh.merged_commit.clone().filter(|oid| fresh.state == PrState::Merged && git::is_ancestor(&repo, oid, &main) == Some(true));
        let (id, number) = (t.id.clone(), fresh.number);
        ops::apply(store, None, Op::SetPr { id: id.clone(), pr: Some(fresh) })?;
        updated += 1;
        if newly_closed {
            let text = format!("PR #{number} was closed without merging — rework the ticket, or discard it");
            ops::apply(store, None, Op::AddNote { id: id.clone(), text, author: Some("kanban".into()) })?;
        }
        // Merge recorded and already pulled: land in this tick, not the next.
        if landable.is_some() {
            let op = Op::LandTicket {
                id,
                expected_branch: t.column.branch().map(str::to_owned),
                reason: format!("PR #{number} merged and pulled into {main}"),
            };
            if let Err(e) = ops::apply(store, None, op) {
                tracing::debug!(error = %e, "landing after poll refused — the sweep will retry from fresh state");
            }
        }
    }
    Ok(updated)
}

/// The fields both gh lookups request.
const GH_FIELDS: &str = "number,url,state,mergeCommit";

/// Run one gh lookup and shape its answer. `pr view` answers an object, `pr list` an array — accept either. `Ok(None)`
/// means gh answered and there is no such PR.
fn lookup(repo: &Path, args: &[&str]) -> anyhow::Result<Option<PrRef>> {
    let out = pr::gh(repo, args)?;
    let value: serde_json::Value = serde_json::from_str(&out).map_err(|e| anyhow::anyhow!("unexpected gh output: {e}: {out}"))?;
    let obj = match &value {
        serde_json::Value::Array(items) => match items.first() {
            Some(first) => first,
            None => return Ok(None),
        },
        other => other,
    };
    let state = match obj["state"].as_str() {
        Some(s) if s.eq_ignore_ascii_case("open") => PrState::Open,
        Some(s) if s.eq_ignore_ascii_case("merged") => PrState::Merged,
        Some(s) if s.eq_ignore_ascii_case("closed") => PrState::Closed,
        _ => return Ok(None),
    };
    let Some(number) = obj["number"].as_u64() else { return Ok(None) };
    Ok(Some(PrRef {
        number,
        url: obj["url"].as_str().unwrap_or_default().to_owned(),
        state,
        merged_commit: obj["mergeCommit"]["oid"].as_str().map(str::to_owned),
    }))
}

/// Log network/gh trouble once per process: a laptop without gh (or offline for the afternoon) should not warn every
/// tick, and the pass simply resumes when the answers come back.
fn warn_once(e: &anyhow::Error) {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        tracing::warn!("gh PR poll unavailable ({e:#}) — polling continues quietly until it recovers");
    } else {
        tracing::debug!("gh PR poll still unavailable: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git::git,
        ops::Op,
        store::model::{ColumnId, External, Status, TicketId},
    };

    struct Scratch {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        store: Store,
    }

    fn commit(repo: &Path, msg: &str) {
        let sign = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"];
        let args: Vec<&str> = sign.iter().chain(&["commit", "--allow-empty", "-q", "-m", msg]).copied().collect();
        git(repo, &args).unwrap();
    }

    /// A committed repo on `main` with an initialised board holding K-1 in review on branch `k-1/work` (one extra
    /// commit on the branch).
    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        commit(&repo, "seed");
        let store = Store::at(repo.join(".kanban"));
        store.init().unwrap();

        ops::apply(
            &store,
            None,
            Op::CreateTicket { title: "the work".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Ready, model: None, effort: None },
        )
        .unwrap();
        let id = TicketId("K-1".into());
        ops::apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();

        git(&repo, &["checkout", "-q", "-b", "k-1/work"]).unwrap();
        commit(&repo, "feat: the work");
        git(&repo, &["checkout", "-q", "main"]).unwrap();

        ops::apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "k-1/work".into(), path: "/tmp/unused".into() }).unwrap();
        ops::apply(&store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
        Scratch { _dir: dir, repo, store }
    }

    fn column_of(store: &Store, id: &str) -> Column {
        store.read_board().unwrap().ticket(&TicketId(id.into())).unwrap().column.clone()
    }

    #[test]
    fn an_unmerged_branch_stays_in_review() {
        let s = scratch();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        assert!(matches!(column_of(&s.store, "K-1"), Column::Review { .. }));
    }

    #[test]
    fn a_branch_merged_into_main_lands_and_unblocks_its_dependents() {
        let s = scratch();
        ops::apply(
            &s.store,
            None,
            Op::CreateTicket { title: "dependent".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![TicketId("K-1".into())], status: Status::Ready, model: None, effort: None },
        )
        .unwrap();
        git(&s.repo, &["merge", "-q", "--ff-only", "k-1/work"]).unwrap();

        assert_eq!(sweep(&s.store).unwrap(), 1);
        let board = s.store.read_board().unwrap();
        let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
        assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
        assert!(k1.notes.last().unwrap().text.contains("merged into main"), "{:?}", k1.notes);
        assert_eq!(crate::store::derive::next_ticket(&board, &[]).unwrap().id.0, "K-2", "the dependent unblocked");
        assert_eq!(sweep(&s.store).unwrap(), 0, "a second sweep finds nothing to do");
    }

    #[test]
    fn the_merge_sh_flow_lands_via_the_observed_tip() {
        // The user's own script: rebase the branch onto main, fast-forward main, delete the branch. Post-hoc the tip is
        // no ancestor (rebase rewrote it) and the ref is gone — only the sweep's earlier observation + git cherry prove it.
        let s = scratch();
        assert_eq!(sweep(&s.store).unwrap(), 0, "first sweep observes the live branch tip");
        assert!(s.store.read_land_state().unwrap().contains_key("k-1/work"));

        std::fs::write(s.repo.join("drift"), "x").unwrap();
        git(&s.repo, &["add", "drift"]).unwrap();
        commit(&s.repo, "mainline moves"); // forces the rebase to rewrite shas
        git(&s.repo, &["checkout", "-q", "k-1/work"]).unwrap();
        git(&s.repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "rebase", "-q", "main"]).unwrap();
        git(&s.repo, &["checkout", "-q", "main"]).unwrap();
        git(&s.repo, &["merge", "-q", "--ff-only", "k-1/work"]).unwrap();
        git(&s.repo, &["branch", "-q", "-d", "k-1/work"]).unwrap();

        assert_eq!(sweep(&s.store).unwrap(), 1);
        let board = s.store.read_board().unwrap();
        let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
        assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
        assert!(k1.notes.last().unwrap().text.contains("rebased into main"), "{:?}", k1.notes);
        assert!(s.store.read_land_state().unwrap().is_empty(), "the landed ticket's observation retires with it");
    }

    #[test]
    fn a_vanished_branch_without_proof_stays_in_review() {
        // Deleted before any sweep observed it: no proof of landing, no auto-discard — the human decides.
        let s = scratch();
        git(&s.repo, &["branch", "-q", "-D", "k-1/work"]).unwrap();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        assert!(matches!(column_of(&s.store, "K-1"), Column::Review { .. }));
    }

    #[test]
    fn an_observed_but_discarded_branch_still_stays_in_review() {
        // Observed, then force-deleted with commits that never reached main: ancestry and patch-ids both refute the
        // landing, so nothing moves — and nothing is ever auto-discarded.
        let s = scratch();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        git(&s.repo, &["branch", "-q", "-D", "k-1/work"]).unwrap();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        assert!(matches!(column_of(&s.store, "K-1"), Column::Review { .. }));
    }

    #[test]
    fn external_tickets_never_land_from_local_branch_state() {
        // The delegate's branch name is not a local ref — exactly the shape rule 2 would false-positive on if external
        // tickets weren't exempt.
        let s = scratch();
        let id = TicketId("K-1".into());
        ops::apply(&s.store, None, Op::BindExternal { id: id.clone(), external: Some(External { provider: "github".into(), kind: "issue".into(), number: 42 }) }).unwrap();
        ops::apply(&s.store, None, Op::MoveTicket { id, to: ColumnId::Review, position: None, owner: None, branch: Some("myrepo-issue0042".into()) }).unwrap();

        assert_eq!(sweep(&s.store).unwrap(), 0);
        assert!(matches!(column_of(&s.store, "K-1"), Column::Review { .. }));
    }

    #[test]
    fn a_merged_pr_lands_only_once_its_commit_reaches_local_main() {
        let s = scratch();
        // The PR merged on GitHub as a squash commit that does not exist locally yet.
        let pr = PrRef { number: 12, url: "https://example.invalid/pull/12".into(), state: PrState::Merged, merged_commit: Some("0".repeat(40)) };
        ops::apply(&s.store, None, Op::SetPr { id: TicketId("K-1".into()), pr: Some(pr) }).unwrap();
        assert_eq!(sweep(&s.store).unwrap(), 0, "merged on GitHub is not done — local main hasn't got it");
        assert!(matches!(column_of(&s.store, "K-1"), Column::Review { .. }));

        // "git pull": simulate the squash landing locally by making a commit and recording ITS oid as the merge commit.
        git(&s.repo, &["merge", "-q", "--squash", "k-1/work"]).unwrap();
        commit(&s.repo, "K-1: the work (#12)");
        let oid = git(&s.repo, &["rev-parse", "HEAD"]).unwrap();
        let pr = PrRef { number: 12, url: "https://example.invalid/pull/12".into(), state: PrState::Merged, merged_commit: Some(oid) };
        ops::apply(&s.store, None, Op::SetPr { id: TicketId("K-1".into()), pr: Some(pr) }).unwrap();

        assert_eq!(sweep(&s.store).unwrap(), 1);
        let board = s.store.read_board().unwrap();
        let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
        assert!(matches!(k1.column, Column::Done { discarded: false, .. }));
        assert!(k1.notes.last().unwrap().text.contains("PR #12 merged and pulled"), "{:?}", k1.notes);
    }

    #[test]
    fn no_main_branch_and_no_repo_are_silent_no_ops() {
        // A repo whose only branch is neither main nor master, with nothing configured: no anchor, no sweeping.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("odd");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]).unwrap();
        commit(&repo, "seed");
        let store = Store::at(repo.join(".kanban"));
        store.init().unwrap();
        // init seeded main_branch: "main" (the fallback)… which doesn't exist as a ref, so every proof degrades to None.
        assert_eq!(sweep(&store).unwrap(), 0);

        // No repo at all.
        let bare = tempfile::tempdir().unwrap();
        let store = Store::at(bare.path().join(".kanban"));
        store.init().unwrap();
        assert_eq!(sweep(&store).unwrap(), 0);
        assert_eq!(poll(&store).unwrap(), 0);
    }

    #[test]
    fn a_rework_claim_retargets_the_evidence_and_the_sweep_respects_it() {
        // Claim the review ticket back (rework), then merge the branch: the ticket is in doing, so the sweep must not
        // touch it — landing only ever moves review tickets.
        let s = scratch();
        ops::apply(&s.store, None, Op::Claim { id: TicketId("K-1".into()), agent: "claude".into() }).unwrap();
        git(&s.repo, &["merge", "-q", "--ff-only", "k-1/work"]).unwrap();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        assert!(matches!(column_of(&s.store, "K-1"), Column::Doing { .. }));
    }

    #[test]
    fn dependents_of_a_discarded_ticket_stay_blocked_through_sweeps() {
        let s = scratch();
        ops::apply(
            &s.store,
            None,
            Op::CreateTicket { title: "dependent".into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![TicketId("K-1".into())], status: Status::Ready, model: None, effort: None },
        )
        .unwrap();
        ops::apply(&s.store, None, Op::DiscardTicket { id: TicketId("K-1".into()), reason: "abandoned".into() }).unwrap();
        // Even though the branch still exists and could later merge, the ticket is done+discarded: no sweep revives it,
        // and the dependent stays blocked.
        git(&s.repo, &["merge", "-q", "--ff-only", "k-1/work"]).unwrap();
        assert_eq!(sweep(&s.store).unwrap(), 0);
        let board = s.store.read_board().unwrap();
        assert!(crate::store::derive::blocked(board.ticket(&TicketId("K-2".into())).unwrap(), &board));
    }
}
