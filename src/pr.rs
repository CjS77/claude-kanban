//! The Create PR button's lifecycle: eligibility, then `git push` + `gh pr create`.
//!
//! This is the first of the binary's three sanctioned network egresses — the explicit user click, beside the
//! `poll_interval`-gated gh polls in `land`/`minesweeper` and the toggle-gated delegation issue-create in `minesweeper`
//! (see design.md; "local-only" always meant the board itself). The mechanics are deliberately dumb: no LLM, no schema
//! change. The PR body is templated verbatim from the ticket (its `body` is a refined spec, its `notes` are the
//! progress log), and the resulting URL is recorded as a progress note by the handler — the note is the record, exactly
//! like branches.
//!
//! Everything here shells out through [`crate::git::run`] with prompts suppressed (`GIT_TERMINAL_PROMPT=0`,
//! `GH_PROMPT_DISABLED=1`), so a missing credential fails fast with stderr instead of hanging the handler.
//!
//! A repo whose only credential is a GitHub PAT sets [`TOKEN_ENV`] in the environment `serve` runs in: `gh` receives it
//! as `GH_TOKEN`, and `git push` gets a one-shot credential helper that answers with it. The token never touches an
//! argument vector — the helper reads it from the inherited environment.

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

/// The environment variable holding a GitHub personal access token, for repos without `gh auth login` or a git
/// credential helper. Read on every call, so it applies to the Create PR click and the gh polls alike.
pub const TOKEN_ENV: &str = "KANBAN_GH_TOKEN";

/// A git credential helper answering `get` requests from [`TOKEN_ENV`]. GitHub accepts any username alongside a PAT;
/// `x-access-token` is the conventional one. `store` and `erase` fall through silently, so nothing is persisted.
const CREDENTIAL_HELPER: &str = r#"!f() { test "$1" = get && printf 'username=x-access-token\npassword=%s\n' "$KANBAN_GH_TOKEN"; }; f"#;

/// Lower-cased stderr fragments with which GitHub, `gh` or git report a credential that was presented but refused: too
/// few permissions, a repo outside the token's reach (GitHub answers "not found" rather than leak a private repo), or a
/// revoked token.
const REFUSALS: &[&str] = &[
    "resource not accessible",
    "bad credentials",
    "could not resolve to a repository",
    "permission to",
    "write access to repository not granted",
    "authentication failed",
    "invalid username or password",
    "returned error: 403",
    "http 401",
    "http 403",
];

/// What [`create_pr`] did: the PR's URL, and whether this click created it (`false` = an open PR already existed).
#[derive(Debug)]
pub struct PrReport {
    pub url: String,
    pub created: bool,
}

/// The button-visibility predicate: a non-external `review` ticket whose branch still exists locally, in a repo with at
/// least one remote (any git error — e.g. no repo at all — counts as false). Review is the PR moment — a done ticket
/// has already landed. Checked live per detail-pane render, never cached: two subprocesses per pane open is cheap, and
/// a user who adds a remote mid-session in order to push must not need a server restart.
#[must_use]
pub fn eligible(store: &Store, ticket: &Ticket) -> bool {
    let Column::Review { branch: Some(branch), .. } = &ticket.column else {
        return false;
    };
    if ticket.external.is_some() {
        return false; // external tickets are worked elsewhere; their PRs are the daemon's business
    }
    let Ok(repo) = worktree::repo_root(store) else { return false };
    has_remote(&repo) && git::branch_exists(&repo, branch)
}

/// Push the review ticket's branch and open a GitHub PR, deduping against an already-open PR for the branch first (the
/// shared-branch case: subtasks resolve several tickets on one branch, and the second ticket's click finds the first's
/// PR). Re-validates everything [`eligible`] checks — the render is stale by click time. Either way the PR is recorded
/// on the ticket ([`Op::SetPr`]) so the poller can track it to its merge.
pub fn create_pr(store: &Store, id: &TicketId) -> anyhow::Result<PrReport> {
    let repo = worktree::repo_root(store)?;
    let board = store.read_board()?;
    let ticket = board.ticket(id).with_context(|| format!("{id} not found on the board"))?;
    let Column::Review { branch: Some(branch), .. } = &ticket.column else {
        bail!("{id} is not a review ticket with a branch — nothing to open a PR from");
    };
    if ticket.external.is_some() {
        bail!("{id} is external — its PRs are worked elsewhere");
    }
    if !git::branch_exists(&repo, branch) {
        bail!("branch {branch} no longer exists locally — already merged and deleted?");
    }
    let remote = pick_remote(&repo)?;

    let explain = |e: anyhow::Error| explain_pat_refusal(&repo, &remote, e);

    // Dedupe before pushing: never surprise-update an open PR.
    if let Some((number, url)) = open_pr_for(&repo, branch).map_err(explain)? {
        tracing::info!(ticket = %id, %branch, %url, "PR already open — not pushing");
        record_pr(store, id, number, &url);
        return Ok(PrReport { url, created: false });
    }

    push(&repo, &remote, branch).map_err(explain)?;
    let title = format!("{id}: {}", ticket.title);
    let url = gh(
        &repo,
        &["pr", "create", "--head", branch, "--title", &title, "--body", &pr_body(ticket)],
    )
    .with_context(|| format!("creating the PR for {branch}"))
    .map_err(explain)?;
    tracing::info!(ticket = %id, %branch, %url, "PR created");
    if let Some(number) = trailing_pr_number(&url) {
        record_pr(store, id, number, &url);
    }
    Ok(PrReport { url, created: true })
}

/// Best-effort recording of the PR onto the ticket. The PR exists on GitHub either way — a failure to write the board
/// (or an unparseable URL) must never fail the click; the poller's by-branch discovery is the safety net.
fn record_pr(store: &Store, id: &TicketId, number: u64, url: &str) {
    let pr = crate::store::model::PrRef {
        number,
        url: url.to_owned(),
        state: crate::store::model::PrState::Open,
        merged_commit: None,
    };
    if let Err(e) = crate::ops::apply(
        store,
        None,
        crate::ops::Op::SetPr {
            id: id.clone(),
            pr: Some(pr),
        },
    ) {
        tracing::warn!(ticket = %id, error = %e, "PR created but not recorded on the board — the poller will rediscover it by branch");
    }
}

/// `git push -u <remote> <branch>`, authenticated by [`TOKEN_ENV`] when it is set. The empty `credential.helper` resets
/// any configured helpers first, so a stale keychain entry can't shadow the token the user just supplied.
fn push(repo: &Path, remote: &str, branch: &str) -> anyhow::Result<()> {
    let helper = format!("credential.helper={CREDENTIAL_HELPER}");
    let auth = if token().is_some() { vec!["-c", "credential.helper=", "-c", &helper] } else { vec![] };
    let args: Vec<&str> = auth.into_iter().chain(["push", "-u", remote, branch]).collect();
    git::run("git", repo, &args, &[("GIT_TERMINAL_PROMPT", "0")])
        .map_err(|e| with_auth_hint(e, "could not read Username"))
        .with_context(|| format!("pushing {branch} to {remote}"))?;
    Ok(())
}

/// The PAT from [`TOKEN_ENV`], when set and non-blank.
fn token() -> Option<String> {
    std::env::var(TOKEN_ENV).ok().filter(|t| !t.trim().is_empty())
}

/// Point a credential failure (recognised by `marker` in stderr) at [`TOKEN_ENV`]; any other error passes through.
fn with_auth_hint(e: GitError, marker: &str) -> anyhow::Error {
    let missing_auth = matches!(&e, GitError::Failed { stderr, .. } if stderr.contains(marker));
    let hint = format!("no GitHub credentials — run `gh auth login`, or set {TOKEN_ENV} to a personal access token where `serve` runs");
    match (missing_auth, token()) {
        (true, None) => anyhow::Error::new(e).context(hint),
        _ => e.into(),
    }
}

/// Whether a personal access token is in play: [`TOKEN_ENV`], or a `GH_TOKEN`/`GITHUB_TOKEN` the user exported for `gh`.
fn pat_in_use() -> bool {
    token().is_some() || ["GH_TOKEN", "GITHUB_TOKEN"].iter().any(|v| std::env::var(v).is_ok_and(|t| !t.trim().is_empty()))
}

/// Whether `stderr` says GitHub refused a personal access token: GitHub names the token type outright, or a PAT is in
/// play and the failure is one of the [`REFUSALS`].
fn pat_refused(stderr: &str, pat_in_use: bool) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("personal access token") || (pat_in_use && REFUSALS.iter().any(|r| stderr.contains(r)))
}

/// When a Create PR step failed because GitHub refused a PAT, log what the token needs and put the short version on the
/// error, which the browser shows as a toast. Any other error passes through untouched.
fn explain_pat_refusal(repo: &Path, remote: &str, e: anyhow::Error) -> anyhow::Error {
    let refused = e.chain().filter_map(|c| c.downcast_ref::<GitError>()).any(|g| {
        matches!(g, GitError::Failed { stderr, .. } if pat_refused(stderr, pat_in_use()))
    });
    if !refused {
        return e;
    }
    let slug = github_slug(repo, remote).unwrap_or_else(|| "<owner>/<repo>".into());
    tracing::warn!(error = format!("{e:#}"), "{}", pat_checklist(&slug));
    e.context(format!(
        "GitHub refused the personal access token — check it has Contents and Pull requests read & write (classic: `repo` \
         scope), that its repository access covers {slug}, and, if {slug} is a fork, that it is gh's default repo \
         (`gh repo set-default {slug}`); the server log has the full checklist"
    ))
}

/// The full checklist logged on a PAT refusal, for the repo `slug` the push went to.
fn pat_checklist(slug: &str) -> String {
    format!(
        "GitHub refused the personal access token used for Create PR. Check:\n\
         \x20 1. Permissions. A fine-grained PAT needs Contents: read & write (push), Pull requests: read & write (open \
         and list PRs) and Metadata: read-only. A classic PAT needs the `repo` scope (`public_repo` is enough for a public \
         repo). If the branch changes .github/workflows, add Workflows: read & write (classic: the `workflow` scope).\n\
         \x20 2. Repository access. A fine-grained PAT must list {slug} under \"Repository access\" (or use \"All \
         repositories\"), and be owned by the account or org that owns {slug}. An org may also need to approve the \
         token, or a classic token may need SSO authorisation.\n\
         \x20 3. Forks. If {slug} is your fork, gh may be opening the PR against the upstream repo, which the token \
         cannot reach. Make the fork the default repo for PRs: `gh repo set-default {slug}`"
    )
}

/// The `owner/repo` of a GitHub remote's URL (`https://github.com/o/r.git`, `git@github.com:o/r.git`, …), or `None` when
/// the remote is not on GitHub.
fn github_slug(repo: &Path, remote: &str) -> Option<String> {
    let url = git(repo, &["remote", "get-url", remote]).ok()?;
    slug_of(&url)
}

fn slug_of(url: &str) -> Option<String> {
    if !url.contains("github") {
        return None;
    }
    let mut parts = url.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']);
    let name = parts.next().filter(|n| !n.is_empty())?;
    let owner = parts.next().filter(|o| !o.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// The `<n>` of a `…/pull/<n>` URL, as `gh pr create` prints it.
fn trailing_pr_number(url: &str) -> Option<u64> {
    url.rsplit_once("/pull/").and_then(|(_, n)| n.trim_end_matches('/').parse().ok())
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

pub(crate) fn has_remote(repo: &Path) -> bool {
    git(repo, &["remote"]).is_ok_and(|out| !out.is_empty())
}

/// The number and URL of an already-open PR whose head is `branch`, if any.
fn open_pr_for(repo: &Path, branch: &str) -> anyhow::Result<Option<(u64, String)>> {
    let out = gh(repo, &["pr", "list", "--head", branch, "--state", "open", "--json", "number,url"])?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&out).with_context(|| format!("unexpected `gh pr list` output: {out}"))?;
    Ok(prs
        .first()
        .and_then(|pr| Some((pr["number"].as_u64()?, pr["url"].as_str()?.to_owned()))))
}

/// Run `gh` in the repo with every prompt suppressed (title and body are always supplied, so it never opens an editor),
/// passing [`TOKEN_ENV`] through as `GH_TOKEN` when set. A missing binary becomes install advice rather than a bare
/// spawn error, and a missing login names the token variable.
pub(crate) fn gh(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let token = token();
    let envs: Vec<(&str, &str)> = [("GH_PROMPT_DISABLED", "1"), ("GH_NO_UPDATE_NOTIFIER", "1")]
        .into_iter()
        .chain(token.as_deref().map(|t| ("GH_TOKEN", t)))
        .collect();
    match git::run("gh", repo, args, &envs) {
        Err(GitError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("GitHub CLI (gh) not found — install it, or push the branch manually")
        }
        Err(e) => Err(with_auth_hint(e, "gh auth login")),
        Ok(out) => Ok(out),
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
            n.author
                .as_deref()
                .map_or_else(|| format!("- {at}: {}", n.text), |author| format!("- {at} — {author}: {}", n.text))
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
            model: None,
            effort: None,
            auto_merge: false,
            changes_requested: false,
            accepted: false,
            landing_blocked: false,
            depends_on: vec![],
            notes,
            review: vec![],
            external: None,
            pr: None,
            column: Column::Done {
                branch: Some("k-7/rate-limit-login".into()),
                completed_at: Utc::now(),
                discarded: false,
            },
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
            Note {
                at,
                author: Some("claude".into()),
                text: "started".into(),
            },
            Note {
                at,
                author: None,
                text: "hand-added".into(),
            },
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
        assert_eq!(
            pick_remote(repo.path()).unwrap(),
            "alpha",
            "no origin: the first listed remote wins"
        );

        git(repo.path(), &["remote", "add", "origin", "https://example.invalid/origin.git"]).unwrap();
        assert_eq!(pick_remote(repo.path()).unwrap(), "origin");
    }

    /// Drive `git credential <action>` through the same helper config [`push`] installs, with `token` in the environment.
    fn credential(action: &str, token: &str) -> String {
        use std::io::Write;
        let helper = format!("credential.helper={CREDENTIAL_HELPER}");
        let mut child = std::process::Command::new("git")
            .args(["-c", "credential.helper=", "-c", &helper, "credential", action])
            .env(TOKEN_ENV, token)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"protocol=https\nhost=github.com\n\n").unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    }

    #[test]
    fn credential_helper_answers_get_with_the_token_from_the_environment() {
        let out = credential("fill", "ghp_example");
        assert!(out.contains("username=x-access-token\n"), "{out}");
        assert!(out.contains("password=ghp_example\n"), "{out}");
        assert_eq!(credential("approve", "ghp_example"), "", "store must persist nothing");
    }

    #[test]
    fn auth_hint_names_the_token_variable_only_for_credential_failures() {
        let failed = |stderr: &str| GitError::Failed {
            args: "gh pr list".into(),
            stderr: stderr.into(),
        };
        let hinted = format!("{:#}", with_auth_hint(failed("To get started with GitHub CLI, please run:  gh auth login"), "gh auth login"));
        assert!(hinted.contains(TOKEN_ENV), "{hinted}");
        let plain = format!("{:#}", with_auth_hint(failed("no pull requests match"), "gh auth login"));
        assert!(!plain.contains(TOKEN_ENV), "{plain}");
    }

    #[test]
    fn pat_refusals_are_recognised_only_when_a_pat_is_in_play_or_named() {
        let named = "GraphQL: Resource not accessible by personal access token (createPullRequest)";
        assert!(pat_refused(named, false), "GitHub naming the token type is enough");
        let push = "remote: Permission to me/repo.git denied to me.\nfatal: unable to access '…': The requested URL returned error: 403";
        assert!(pat_refused(push, true));
        assert!(!pat_refused(push, false), "without a PAT, a 403 is someone else's credential");
        assert!(!pat_refused("no pull requests match your search", true));
    }

    #[test]
    fn pat_refusal_on_the_error_chain_gains_the_hint_and_others_pass_through() {
        let repo = scratch_repo();
        git(repo.path(), &["remote", "add", "origin", "git@github.com:me/fork.git"]).unwrap();
        let failed = |stderr: &str| -> anyhow::Error {
            anyhow::Error::new(GitError::Failed {
                args: "gh pr create".into(),
                stderr: stderr.into(),
            })
            .context("creating the PR for k-7/x")
        };
        let refused = format!(
            "{:#}",
            explain_pat_refusal(repo.path(), "origin", failed("HTTP 403: Resource not accessible by personal access token"))
        );
        assert!(refused.contains("gh repo set-default me/fork"), "{refused}");
        assert!(refused.contains("creating the PR for k-7/x"), "the original error must survive: {refused}");
        let other = format!("{:#}", explain_pat_refusal(repo.path(), "origin", failed("a pull request already exists")));
        assert!(!other.contains("set-default"), "{other}");
    }

    #[test]
    fn slug_of_reads_github_urls_and_ignores_others() {
        assert_eq!(slug_of("https://github.com/me/repo.git").as_deref(), Some("me/repo"));
        assert_eq!(slug_of("https://x-access-token@github.com/me/repo/").as_deref(), Some("me/repo"));
        assert_eq!(slug_of("git@github.com:me/repo.git").as_deref(), Some("me/repo"));
        assert_eq!(slug_of("ssh://git@github.com/me/repo").as_deref(), Some("me/repo"));
        assert_eq!(slug_of("/srv/git/origin.git"), None);
    }

    #[test]
    fn branch_and_remote_probes_answer_false_cleanly() {
        let repo = scratch_repo();
        assert!(!has_remote(repo.path()));
        assert!(!git::branch_exists(repo.path(), "k-7/nope"));
        let outside = tempfile::tempdir().unwrap();
        assert!(!has_remote(outside.path()), "not a repo counts as no remote, not an error");
    }
}
