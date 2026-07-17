//! The Create PR button's lifecycle: eligibility, then `git push` + `gh pr create`.
//!
//! This is the binary's one network egress, and it only ever runs on an explicit user click — the recorded amendment to
//! "nothing leaves the machine" (see design.md). The mechanics are deliberately dumb: no LLM, no schema change. The PR
//! body is templated verbatim from the ticket (its `body` is a refined spec, its `notes` are the progress log), and the
//! resulting URL is recorded as a progress note by the handler — the note is the record, exactly like branches.
//!
//! Everything here shells out through [`crate::git::run`] with prompts suppressed (`GIT_TERMINAL_PROMPT=0`,
//! `GH_PROMPT_DISABLED=1`), so a missing credential fails fast with stderr instead of hanging the handler.

use std::path::Path;

use anyhow::{Context, bail};

use crate::{
    git::{self, GitError, git},
    store::{
        Store,
        model::{Column, Ticket, TicketId},
    },
    worktree,
};

/// What [`create_pr`] did: the PR's URL, and whether this click created it (`false` = an open PR already existed).
#[derive(Debug)]
pub struct PrReport {
    pub url: String,
    pub created: bool,
}

/// The button-visibility predicate: a non-external `done` ticket whose branch still exists locally, in a repo with at
/// least one remote (any git error — e.g. no repo at all — counts as false). Checked live per detail-pane render, never
/// cached: two subprocesses per pane open is cheap, and a user who adds a remote mid-session in order to push must not
/// need a server restart.
#[must_use]
pub fn eligible(store: &Store, ticket: &Ticket) -> bool {
    let Column::Done { branch: Some(branch), .. } = &ticket.column else { return false };
    if ticket.external.is_some() {
        return false; // external tickets are worked elsewhere; their PRs are the daemon's business
    }
    let Ok(repo) = worktree::repo_root(store) else { return false };
    has_remote(&repo) && branch_exists(&repo, branch)
}

/// Push the done ticket's branch and open a GitHub PR, deduping against an already-open PR for the branch first (the
/// shared-branch case: subtasks resolve several tickets on one branch, and the second ticket's click finds the first's
/// PR). Re-validates everything [`eligible`] checks — the render is stale by click time.
pub fn create_pr(store: &Store, id: &TicketId) -> anyhow::Result<PrReport> {
    let repo = worktree::repo_root(store)?;
    let board = store.read_board()?;
    let ticket = board.ticket(id).with_context(|| format!("{id} not found on the board"))?;
    let Column::Done { branch: Some(branch), .. } = &ticket.column else {
        bail!("{id} is not a done ticket with a branch — nothing to open a PR from");
    };
    if ticket.external.is_some() {
        bail!("{id} is external — its PRs are worked elsewhere");
    }
    if !branch_exists(&repo, branch) {
        bail!("branch {branch} no longer exists locally — already merged and deleted?");
    }
    let remote = pick_remote(&repo)?;

    // Dedupe before pushing: never surprise-update an open PR.
    if let Some(url) = open_pr_for(&repo, branch)? {
        tracing::info!(ticket = %id, %branch, %url, "PR already open — not pushing");
        return Ok(PrReport { url, created: false });
    }

    git::run("git", &repo, &["push", "-u", &remote, branch], &[("GIT_TERMINAL_PROMPT", "0")])
        .with_context(|| format!("pushing {branch} to {remote}"))?;
    let title = format!("{id}: {}", ticket.title);
    let url = gh(&repo, &["pr", "create", "--head", branch, "--title", &title, "--body", &pr_body(ticket)])
        .with_context(|| format!("creating the PR for {branch}"))?;
    tracing::info!(ticket = %id, %branch, %url, "PR created");
    Ok(PrReport { url, created: true })
}

/// The remote to push to: `origin` when present, else the first listed, else bail.
fn pick_remote(repo: &Path) -> anyhow::Result<String> {
    let remotes = git(repo, &["remote"])?;
    remotes
        .lines()
        .find(|r| *r == "origin")
        .or_else(|| remotes.lines().next())
        .map(str::to_owned)
        .context("this repository has no git remote to push to")
}

fn has_remote(repo: &Path) -> bool {
    git(repo, &["remote"]).is_ok_and(|out| !out.is_empty())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--quiet", "--verify", &format!("refs/heads/{branch}")]).is_ok()
}

/// The URL of an already-open PR whose head is `branch`, if any.
fn open_pr_for(repo: &Path, branch: &str) -> anyhow::Result<Option<String>> {
    let out = gh(repo, &["pr", "list", "--head", branch, "--state", "open", "--json", "url"])?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&out).with_context(|| format!("unexpected `gh pr list` output: {out}"))?;
    Ok(prs.first().and_then(|pr| pr["url"].as_str()).map(str::to_owned))
}

/// Run `gh` in the repo with every prompt suppressed (title and body are always supplied, so it never opens an editor).
/// A missing binary becomes install advice rather than a bare spawn error.
fn gh(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    match git::run("gh", repo, args, &[("GH_PROMPT_DISABLED", "1"), ("GH_NO_UPDATE_NOTIFIER", "1")]) {
        Err(GitError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("GitHub CLI (gh) not found — install it, or push the branch manually")
        }
        other => Ok(other?),
    }
}

/// The deterministic PR body: the ticket's spec verbatim, then its progress log. The prose already exists on the card.
fn pr_body(ticket: &Ticket) -> String {
    let spec = format!("## {}: {}\n\n{}", ticket.id, ticket.title, ticket.body);
    if ticket.notes.is_empty() {
        return spec;
    }
    let notes: Vec<String> = ticket
        .notes
        .iter()
        .map(|n| {
            let at = n.at.format("%Y-%m-%d %H:%M UTC");
            n.author.as_deref().map_or_else(|| format!("- {at}: {}", n.text), |author| format!("- {at} — {author}: {}", n.text))
        })
        .collect();
    format!("{spec}\n\n## Progress\n\n{}", notes.join("\n"))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::store::model::{Note, Status};

    fn scratch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]).unwrap();
        dir
    }

    fn ticket(body: &str, notes: Vec<Note>) -> Ticket {
        Ticket {
            id: TicketId("K-7".into()),
            title: "Rate-limit the login route".into(),
            epic: None,
            status: Status::Ready,
            body: body.into(),
            labels: vec![],
            depends_on: vec![],
            notes,
            external: None,
            column: Column::Done { branch: Some("k-7/rate-limit-login".into()), completed_at: Utc::now() },
        }
    }

    #[test]
    fn pr_body_is_the_spec_verbatim_without_notes() {
        let body = pr_body(&ticket("# Spec\n\nDo the thing.", vec![]));
        assert_eq!(body, "## K-7: Rate-limit the login route\n\n# Spec\n\nDo the thing.");
    }

    #[test]
    fn pr_body_appends_the_progress_log_when_notes_exist() {
        let at = Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
        let notes = vec![
            Note { at, author: Some("claude".into()), text: "started".into() },
            Note { at, author: None, text: "hand-added".into() },
        ];
        let body = pr_body(&ticket("spec", notes));
        assert_eq!(
            body,
            "## K-7: Rate-limit the login route\n\nspec\n\n## Progress\n\n\
             - 2026-07-17 12:00 UTC — claude: started\n- 2026-07-17 12:00 UTC: hand-added"
        );
    }

    #[test]
    fn pick_remote_prefers_origin_then_first_then_errors() {
        let repo = scratch_repo();
        assert!(pick_remote(repo.path()).is_err(), "no remotes at all must bail");

        git(repo.path(), &["remote", "add", "alpha", "https://example.invalid/alpha.git"]).unwrap();
        git(repo.path(), &["remote", "add", "zeta", "https://example.invalid/zeta.git"]).unwrap();
        assert_eq!(pick_remote(repo.path()).unwrap(), "alpha", "no origin: the first listed remote wins");

        git(repo.path(), &["remote", "add", "origin", "https://example.invalid/origin.git"]).unwrap();
        assert_eq!(pick_remote(repo.path()).unwrap(), "origin");
    }

    #[test]
    fn branch_and_remote_probes_answer_false_cleanly() {
        let repo = scratch_repo();
        assert!(!has_remote(repo.path()));
        assert!(!branch_exists(repo.path(), "k-7/nope"));
        let outside = tempfile::tempdir().unwrap();
        assert!(!has_remote(outside.path()), "not a repo counts as no remote, not an error");
    }
}
