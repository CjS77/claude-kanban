//! Minesweeper delegation — the binary's third sanctioned network egress (after the explicit Create PR click and the
//! `poll_interval`-gated landing poll), and the only one that *creates* anything on GitHub. Gated twice: the
//! `minesweeper` compile-time feature (on by default; `--no-default-features` compiles it out for people who want the
//! network surface minimal) and the per-project `minesweeper` config toggle (off by default).
//!
//! With both on, kanban writes no code for ready tickets. Entering `doing` mirrors the ticket to a GitHub issue wearing
//! the configured eligibility label ([`delegate_entering_doing`], called from the same post-apply seams as the
//! review-tip observation), and the serve poller's [`poll`] pass tracks every delegated issue back: a PR referencing
//! the issue moves the card to review with the PR recorded; a flag label or a closure without a PR is written onto the
//! binding and noted, with the card staying in `doing` for the human; a refine split is mirrored as child tickets, the
//! parent parking in review until the refined-parent landing rule can finish it. The merged PR's journey to done is the
//! ordinary landing machinery's business, not this module's.

#[cfg(feature = "minesweeper")]
pub use active::{delegate_entering_doing, hand_over, poll};

/// Compiled out: delegation quietly does nothing — but a board *configured* for it deserves one loud line.
#[cfg(not(feature = "minesweeper"))]
pub fn delegate_entering_doing(store: &crate::store::Store, _id: &crate::store::model::TicketId) {
    warn_if_configured(store);
}

/// Compiled out. Unreachable from the UI — the create modal hides its checkbox in feature-off builds.
#[cfg(not(feature = "minesweeper"))]
pub fn hand_over(store: &crate::store::Store, _id: &crate::store::model::TicketId) {
    warn_if_configured(store);
}

/// Compiled out: the poll pass reports nothing to do.
#[cfg(not(feature = "minesweeper"))]
pub fn poll(store: &crate::store::Store) -> anyhow::Result<usize> {
    warn_if_configured(store);
    Ok(0)
}

#[cfg(not(feature = "minesweeper"))]
fn warn_if_configured(store: &crate::store::Store) {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if crate::config::Config::load(store.dir()).is_ok_and(|c| c.minesweeper()) && WARNED.set(()).is_ok() {
        tracing::warn!("this project enables minesweeper delegation, but this binary was built without the `minesweeper` feature — nothing will be delegated");
    }
}

#[cfg(feature = "minesweeper")]
mod active {
    use std::{
        collections::{HashMap, HashSet},
        path::Path,
        sync::OnceLock,
    };

    use anyhow::Context;

    use crate::{
        config::Config,
        ops::{self, Op, OpError, SubIssueSpec},
        pr,
        store::{
            Store,
            model::{Board, Column, ColumnId, External, PrRef, PrState, TicketId},
        },
        worktree,
    };

    /// The agent name delegated tickets are owned and claimed by — also what `/kanban:delegate` uses by hand.
    const DELEGATE_AGENT: &str = "minesweeper";

    /// The first line of the daemon's refine comment, verbatim (minesweeper `src/child/modes/refine.ts`).
    const REFINE_PREFIX: &str = "Refined into the following sub-tasks:";

    /// The hook both faces call after a mutation that may have put a ready ticket into `doing`. Best-effort by design —
    /// delegating is never a reason to fail a claim or a drag the board has already accepted; trouble lands as a note on
    /// the card instead, and the ticket stays in `doing`, unbound and visibly stalled.
    pub fn delegate_entering_doing(store: &Store, id: &TicketId) {
        match delegate(store, id) {
            Ok(Some(url)) => tracing::info!(ticket = %id, %url, "delegated to minesweeper"),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(ticket = %id, error = format!("{e:#}"), "delegation failed — the ticket stays in doing, unbound");
                let text = format!("delegating to minesweeper failed: {e:#} — fix the cause, then re-enter doing (or run /kanban:delegate)");
                if let Err(e) = ops::apply(store, None, Op::AddNote { id: id.clone(), text, author: Some("kanban".into()) }) {
                    tracing::warn!(ticket = %id, error = %e, "and the failure could not be noted on the card either");
                }
            }
        }
    }

    /// The Create-modal checkbox: hand a freshly created ticket straight to the daemon. Ticking the box is the
    /// per-ticket opt-in, so this runs with or without the project's `minesweeper` toggle. The claim does the
    /// eligibility work (ready, unblocked, unclaimed). Failures differ from the hook's: the hook leaves a failed
    /// ticket in doing because its claimant was real, but here the only claimant is the daemon — nothing was
    /// delegated, so a failed handoff releases the claim and the card returns to todo wearing the explanation.
    pub fn hand_over(store: &Store, id: &TicketId) {
        if let Err(e) = ops::apply(store, None, Op::Claim { id: id.clone(), agent: DELEGATE_AGENT.into() }) {
            note_failure(store, id, &format!("handing to minesweeper failed: {e} — clear that, then delegate again"));
            return;
        }
        let outcome = Config::load(store.dir()).map_err(anyhow::Error::from).and_then(|config| delegate_ticket(store, id, &config));
        match outcome {
            Ok(Some(url)) => tracing::info!(ticket = %id, %url, "handed to minesweeper"),
            Ok(None) => release_noting(store, id, "handing to minesweeper failed: no git repository or remote to mirror the issue to"),
            Err(e) => {
                release_noting(store, id, &format!("handing to minesweeper failed: {e:#} — fix the cause and delegate again, or run /kanban:delegate"));
            }
        }
    }

    /// Record a handoff failure on the card — the note is the record; a card that silently did nothing would be a bug
    /// report waiting to happen.
    fn note_failure(store: &Store, id: &TicketId, text: &str) {
        tracing::warn!(ticket = %id, "{text}");
        if let Err(e) = ops::apply(store, None, Op::AddNote { id: id.clone(), text: text.into(), author: Some("kanban".into()) }) {
            tracing::warn!(ticket = %id, error = %e, "and the failure could not be noted on the card either");
        }
    }

    /// Undo the daemon's claim, then note why. Release before noting so the card is back in todo by the time the SSE
    /// refresh paints the note.
    fn release_noting(store: &Store, id: &TicketId, text: &str) {
        if let Err(e) = ops::apply(store, None, Op::Release { id: id.clone() }) {
            tracing::warn!(ticket = %id, error = %e, "could not release the undelegated ticket — it stays in doing");
        }
        note_failure(store, id, text);
    }

    /// `Ok(None)` means a guard decided this entry into doing is no delegation: toggle off, ticket moved on, rework or
    /// local work (a branch is recorded), a stub claimed for refinement, already bound, or nowhere to mirror to. The
    /// guards run against fresh state — the hook fires after the op released the lock — and [`Op::Delegate`] re-checks
    /// the same evidence *under* the lock after the slow network part is over.
    fn delegate(store: &Store, id: &TicketId) -> anyhow::Result<Option<String>> {
        let config = Config::load(store.dir())?;
        if !config.minesweeper() {
            return Ok(None);
        }
        delegate_ticket(store, id, &config)
    }

    /// The toggle-blind half of [`delegate`]: guards, mirror, bind. [`hand_over`] calls it directly — the checkbox is
    /// its own authorisation.
    fn delegate_ticket(store: &Store, id: &TicketId, config: &Config) -> anyhow::Result<Option<String>> {
        let board = store.read_board()?;
        let Some(ticket) = board.ticket(id) else { return Ok(None) };
        let Column::Doing { branch, .. } = &ticket.column else { return Ok(None) };
        if branch.is_some() || ticket.status != crate::store::model::Status::Ready || ticket.external.is_some() {
            return Ok(None);
        }
        let Ok(repo) = worktree::repo_root(store) else { return Ok(None) };
        if !pr::has_remote(&repo) {
            return Ok(None);
        }

        let footer = mirror_footer(id);
        let (number, url) = match find_mirror(&repo, &footer)? {
            Some(adopted) => adopted, // an earlier attempt created the issue but crashed before binding
            None => create_issue(&repo, ticket, &config.minesweeper_label(), &footer)?,
        };
        let note = format!("delegated to minesweeper: issue #{number} {url}");
        ops::apply(store, None, Op::Delegate { id: id.clone(), external: External::github_issue(number), agent: DELEGATE_AGENT.into(), note })?;
        Ok(Some(url))
    }

    /// The provenance line the issue body ends with — also the dedup key, and exactly what `/kanban:delegate` writes.
    fn mirror_footer(id: &TicketId) -> String {
        format!("Mirrored from kanban ticket {id}.")
    }

    /// An issue an earlier attempt already created for this ticket, found by its exact footer. GitHub's search index can
    /// lag a fresh crash by minutes — the residual risk is a rare duplicate issue, visible and human-closable.
    fn find_mirror(repo: &Path, footer: &str) -> anyhow::Result<Option<(u64, String)>> {
        let search = format!("\"{footer}\" in:body");
        let out = pr::gh(repo, &["issue", "list", "--state", "all", "--search", &search, "--json", "number,url", "--limit", "5"])?;
        let issues: Vec<serde_json::Value> = serde_json::from_str(&out).with_context(|| format!("unexpected `gh issue list` output: {out}"))?;
        Ok(issues.first().and_then(|i| Some((i["number"].as_u64()?, i["url"].as_str()?.to_owned()))))
    }

    /// `gh issue create` with the configured label, the ticket's spec as the body, and the footer as provenance.
    fn create_issue(repo: &Path, ticket: &crate::store::model::Ticket, label: &str, footer: &str) -> anyhow::Result<(u64, String)> {
        ensure_label(repo, label);
        let title = format!("{}: {}", ticket.id, ticket.title);
        let body = format!("{}\n\n---\n{footer}", ticket.body);
        let url = pr::gh(repo, &["issue", "create", "--title", &title, "--body", &body, "--label", label])
            .with_context(|| format!("creating the mirror issue for {}", ticket.id))?;
        let url = url.trim().to_owned();
        let number = trailing_issue_number(&url).with_context(|| format!("no issue number in gh's answer: {url}"))?;
        Ok((number, url))
    }

    /// Create the label if it is missing. "Already exists" is success; any other trouble is only logged, because a truly
    /// missing label makes the `issue create` fail loudly right after — that is the error that matters.
    fn ensure_label(repo: &Path, label: &str) {
        if let Err(e) = pr::gh(repo, &["label", "create", label, "--description", "claude-kanban delegation"]) {
            tracing::debug!(%label, "gh label create declined (fine if it already exists): {e:#}");
        }
    }

    /// The `<n>` of an `…/issues/<n>` URL, as `gh issue create` prints it.
    fn trailing_issue_number(url: &str) -> Option<u64> {
        url.rsplit_once("/issues/").and_then(|(_, n)| n.trim_end_matches('/').parse().ok())
    }

    // ---- the poll pass ------------------------------------------------------------------------------------------------

    /// One pass over the delegated tickets: discover PRs (doing → review, PR recorded), record flag labels and issue
    /// closure onto the binding, and mirror refine splits as child tickets. Returns how many tickets changed. Gated on
    /// the runtime toggle; network trouble warns once per process and ends the pass with the board untouched.
    pub fn poll(store: &Store) -> anyhow::Result<usize> {
        let config = Config::load(store.dir())?;
        if !config.minesweeper() {
            return Ok(0);
        }
        let Ok(repo) = worktree::repo_root(store) else { return Ok(0) };
        if !pr::has_remote(&repo) {
            return Ok(0);
        }
        let board = store.read_board()?;
        let tracked: Vec<&crate::store::model::Ticket> = board.tickets.iter().filter(|t| tracked(t)).collect();
        if tracked.is_empty() {
            return Ok(0);
        }

        let numbers: Vec<u64> = tracked.iter().filter_map(|t| t.external.as_ref().map(|e| e.number)).collect();
        let facts = match batch_issue_facts(&repo, &numbers) {
            Ok(facts) => facts,
            Err(e) => {
                warn_once(&e);
                return Ok(0);
            }
        };

        let flags = config.minesweeper_flag_labels();
        let mut updated = 0;
        for t in tracked {
            match track_ticket(store, &board, &repo, t, &facts, &flags) {
                Ok(true) => updated += 1,
                Ok(false) => {}
                Err(e) => {
                    warn_once(&e);
                    return Ok(updated);
                }
            }
        }
        Ok(updated)
    }

    /// The poll's scope: a GitHub-issue ticket in `doing` (waiting for its PR, flag, or refine), or in `review` with no
    /// PR recorded yet (a refined parent awaiting closure, or a manual delegation someone moved early).
    fn tracked(t: &crate::store::model::Ticket) -> bool {
        t.external.as_ref().is_some_and(External::is_github_issue)
            && match t.column {
                Column::Doing { .. } => true,
                Column::Review { .. } => t.pr.is_none(),
                _ => false,
            }
    }

    /// One tracked ticket against its issue's polled facts. Returns whether anything was written.
    fn track_ticket(
        store: &Store,
        board: &Board,
        repo: &Path,
        t: &crate::store::model::Ticket,
        facts: &HashMap<u64, IssueFacts>,
        flags: &[String],
    ) -> anyhow::Result<bool> {
        let ext = t.external.as_ref().expect("tracked tickets are bound");
        let Some(facts) = facts.get(&ext.number) else { return Ok(false) };
        let in_doing = matches!(t.column, Column::Doing { .. });

        // 1. A PR references the issue: record it, and put the card where PRs are tracked. The existing landing
        //    machinery (rule 5) owns the journey from here.
        if let Some((pr, head)) = &facts.pr {
            let fresh = t.pr.as_ref() != Some(pr);
            if fresh {
                ops::apply(store, None, Op::SetPr { id: t.id.clone(), pr: Some(pr.clone()) })?;
            }
            if in_doing {
                let op = Op::MoveTicket { id: t.id.clone(), to: ColumnId::Review, position: None, owner: None, branch: Some(head.clone()) };
                match ops::apply(store, None, op) {
                    Ok(_) => {}
                    Err(OpError::Invalid(e)) => tracing::debug!(ticket = %t.id, "review move refused (board moved underneath the poll): {e}"),
                    Err(e) => return Err(e.into()),
                }
                return Ok(true);
            }
            return Ok(fresh);
        }

        // 2. A refine split not yet (fully) mirrored: fetch the unmirrored sub-issues and mirror them. The op parks the
        //    parent in review; its binding facts refresh on the next tick.
        if in_doing && facts.refine.iter().any(|n| !ext.sub_issues.contains(n)) {
            let bound: HashSet<u64> =
                board.tickets.iter().filter_map(|t| t.external.as_ref().filter(|e| e.is_github_issue()).map(|e| e.number)).collect();
            let children: Vec<SubIssueSpec> =
                facts.refine.iter().filter(|n| !bound.contains(n)).map(|&n| fetch_issue(repo, n)).collect::<anyhow::Result<_>>()?;
            ops::apply(store, None, Op::MirrorSubIssues { parent: t.id.clone(), agent: DELEGATE_AGENT.into(), children })?;
            return Ok(true);
        }

        // 3. Durable observations: flag labels and closure, written onto the binding only when they changed. The alarm
        //    notes fire once, on the transition — the fresh binding is what makes the next tick a no-op.
        let fresh = External {
            closed: facts.closed,
            flag: flags.iter().find(|f| facts.labels.contains(f)).cloned(),
            ..ext.clone()
        };
        if &fresh == ext {
            return Ok(false);
        }
        let newly_flagged = fresh.flag.is_some() && fresh.flag != ext.flag;
        let newly_abandoned = fresh.closed && !ext.closed && in_doing && fresh.sub_issues.is_empty();
        let (id, number) = (t.id.clone(), ext.number);
        ops::apply(store, None, Op::BindExternal { id: id.clone(), external: Some(fresh.clone()) })?;
        if newly_flagged {
            let label = fresh.flag.expect("just checked");
            let text = format!("minesweeper flagged: issue #{number} carries `{label}` — investigate on GitHub; the ticket stays in doing until you decide");
            ops::apply(store, None, Op::AddNote { id: id.clone(), text, author: Some("kanban".into()) })?;
        }
        if newly_abandoned {
            let text = format!("issue #{number} was closed without a PR — rework the ticket, release it, or close it out by hand");
            ops::apply(store, None, Op::AddNote { id, text, author: Some("kanban".into()) })?;
        }
        Ok(true)
    }

    /// Everything the poll wants to know about one issue, out of the batched GraphQL answer.
    struct IssueFacts {
        closed: bool,
        labels: Vec<String>,
        /// Sub-issue numbers from a refine comment, when one exists.
        refine: Vec<u64>,
        /// The first PR GitHub resolves from the issue's closing references (`Fixes #n`), with its head branch.
        pr: Option<(PrRef, String)>,
    }

    /// One `gh api graphql` call for every delegated issue at once: state, labels, recent comments (for the refine
    /// marker), and `closedByPullRequestsReferences` — GitHub's own resolution of the `Fixes #n` line every minesweeper
    /// PR body is normalised to end with. O(1) network calls per tick regardless of ticket count.
    fn batch_issue_facts(repo: &Path, numbers: &[u64]) -> anyhow::Result<HashMap<u64, IssueFacts>> {
        let (owner, name) = repo_slug(repo)?;
        let query = batch_query(numbers);
        let out = pr::gh(repo, &["api", "graphql", "-f", &format!("query={query}"), "-f", &format!("owner={owner}"), "-f", &format!("name={name}")])?;
        let v: serde_json::Value = serde_json::from_str(&out).with_context(|| format!("unexpected graphql output: {out}"))?;
        let repository = &v["data"]["repository"];
        Ok(numbers
            .iter()
            .filter_map(|&n| {
                let issue = &repository[format!("i{n}")];
                issue.is_object().then(|| (n, issue_facts(issue)))
            })
            .collect())
    }

    /// The batched query. Aliases (`i42: issue(number: 42)`) let one round trip answer for every issue; numbers are
    /// interpolated (they are `u64`s, not strings — nothing to escape), the repo goes in as proper variables.
    fn batch_query(numbers: &[u64]) -> String {
        use std::fmt::Write as _;
        let issues = numbers.iter().fold(String::new(), |mut q, n| {
            let _ = write!(
                q,
                "i{n}: issue(number: {n}) {{ number state labels(first: 20) {{ nodes {{ name }} }} \
                 comments(last: 30) {{ nodes {{ body }} }} \
                 closedByPullRequestsReferences(first: 5, includeClosedPrs: true) \
                 {{ nodes {{ number url state merged mergeCommit {{ oid }} headRefName }} }} }} "
            );
            q
        });
        format!("query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {issues} }} }}")
    }

    fn issue_facts(issue: &serde_json::Value) -> IssueFacts {
        let nodes = |v: &serde_json::Value| v["nodes"].as_array().cloned().unwrap_or_default();
        let labels = nodes(&issue["labels"]).iter().filter_map(|l| l["name"].as_str().map(str::to_owned)).collect();
        let refine = nodes(&issue["comments"])
            .iter()
            .filter_map(|c| c["body"].as_str())
            .map(parse_refine_comment)
            .find(|numbers| !numbers.is_empty())
            .unwrap_or_default();
        let pr = nodes(&issue["closedByPullRequestsReferences"]).first().and_then(|node| {
            let state = match node["state"].as_str()? {
                s if s.eq_ignore_ascii_case("merged") => PrState::Merged,
                s if s.eq_ignore_ascii_case("open") => PrState::Open,
                _ => PrState::Closed,
            };
            let pr = PrRef {
                number: node["number"].as_u64()?,
                url: node["url"].as_str().unwrap_or_default().to_owned(),
                state,
                merged_commit: node["mergeCommit"]["oid"].as_str().map(str::to_owned),
            };
            Some((pr, node["headRefName"].as_str().unwrap_or_default().to_owned()))
        });
        IssueFacts { closed: issue["state"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("closed")), labels, refine, pr }
    }

    /// Sub-issue numbers out of a refine comment: a body starting with the daemon's verbatim marker, one
    /// `- [ ] #<n> — <title>` line per sub-task (ticked boxes included — humans check them off as sub-issues close).
    fn parse_refine_comment(body: &str) -> Vec<u64> {
        if !body.trim_start().starts_with(REFINE_PREFIX) {
            return vec![];
        }
        body.lines()
            .filter_map(|line| {
                let rest = ["- [ ] ", "- [x] ", "- [X] "].iter().find_map(|p| line.trim().strip_prefix(p))?;
                let digits: String = rest.strip_prefix('#')?.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect()
    }

    /// One sub-issue's title and spec, fetched only when a refine actually needs mirroring — a rare event, so the extra
    /// call per sub-issue stays off the steady-state tick.
    fn fetch_issue(repo: &Path, n: u64) -> anyhow::Result<SubIssueSpec> {
        let out = pr::gh(repo, &["issue", "view", &n.to_string(), "--json", "title,body"])?;
        let v: serde_json::Value = serde_json::from_str(&out).with_context(|| format!("unexpected `gh issue view` output: {out}"))?;
        Ok(SubIssueSpec {
            number: n,
            title: v["title"].as_str().unwrap_or("(untitled)").to_owned(),
            body: v["body"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// `owner`/`name` of the repo gh is talking to — one call per poll tick.
    fn repo_slug(repo: &Path) -> anyhow::Result<(String, String)> {
        let out = pr::gh(repo, &["repo", "view", "--json", "owner,name"])?;
        let v: serde_json::Value = serde_json::from_str(&out).with_context(|| format!("unexpected `gh repo view` output: {out}"))?;
        let owner = v["owner"]["login"].as_str().context("gh repo view answered without an owner")?.to_owned();
        let name = v["name"].as_str().context("gh repo view answered without a name")?.to_owned();
        Ok((owner, name))
    }

    /// Log network/gh trouble once per process, exactly like the landing poll's version of the same courtesy.
    fn warn_once(e: &anyhow::Error) {
        static WARNED: OnceLock<()> = OnceLock::new();
        if WARNED.set(()).is_ok() {
            tracing::warn!("minesweeper poll unavailable ({e:#}) — polling continues quietly until it recovers");
        } else {
            tracing::debug!("minesweeper poll still unavailable: {e:#}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn refine_comments_parse_the_daemons_exact_format_and_nothing_else() {
            // The literal shape minesweeper posts (src/child/modes/refine.ts), plus a human-ticked box.
            let comment = "Refined into the following sub-tasks:\n\n- [ ] #12 — split the parser\n- [x] #13 — wire the flags\n- [ ] #14 — docs";
            assert_eq!(parse_refine_comment(comment), vec![12, 13, 14]);

            assert!(parse_refine_comment("Fixed in #12").is_empty(), "an ordinary comment mentioning an issue is not a refine");
            assert!(parse_refine_comment("- [ ] #12 — no marker line").is_empty(), "the marker prefix is required");
            let junk = "Refined into the following sub-tasks:\n\n- [ ] no number here\n- [ ] #notanumber — x\nprose in between\n- [ ] #15 — real";
            assert_eq!(parse_refine_comment(junk), vec![15], "junk lines are skipped, not fatal");
        }

        #[test]
        fn issue_urls_yield_their_trailing_number() {
            assert_eq!(trailing_issue_number("https://github.com/o/r/issues/42"), Some(42));
            assert_eq!(trailing_issue_number("https://github.com/o/r/issues/42/"), Some(42));
            assert_eq!(trailing_issue_number("https://github.com/o/r/pull/42"), None, "a PR URL is not an issue");
        }

        #[test]
        fn the_batch_query_aliases_every_number_once() {
            let q = batch_query(&[7, 42]);
            assert!(q.contains("i7: issue(number: 7)") && q.contains("i42: issue(number: 42)"), "{q}");
            assert!(q.contains("closedByPullRequestsReferences"), "{q}");
            assert!(q.starts_with("query($owner: String!, $name: String!)"), "{q}");
        }

        #[test]
        fn the_mirror_footer_matches_the_delegate_skills_wording() {
            // The dedup search and /kanban:delegate must agree on this line, or each will duplicate the other's issues.
            assert_eq!(mirror_footer(&TicketId("K-7".into())), "Mirrored from kanban ticket K-7.");
        }

        fn scratch() -> (tempfile::TempDir, Store) {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::at(dir.path().join(".kanban"));
            store.init().unwrap();
            (dir, store)
        }

        fn create(store: &Store, title: &str, depends_on: Vec<TicketId>) -> TicketId {
            let op = Op::CreateTicket {
                title: title.into(),
                body: String::new(),
                epic: None,
                labels: vec![],
                depends_on,
                status: crate::store::model::Status::Ready,
                model: None,
                effort: None,
                auto_merge: false,
            };
            TicketId(ops::apply(store, None, op).unwrap().created_ids[0].clone())
        }

        /// Both handoff failure shapes stop before any `gh` subprocess could spawn (the scratch store has no repo at
        /// all), and both leave the card in todo, unclaimed, wearing the explanation.
        #[test]
        fn a_failed_handoff_notes_the_card_and_leaves_it_unclaimed_in_todo() {
            let (_dir, store) = scratch();

            // Blocked: the claim itself refuses — the eligibility contract (the daemon is dependency-blind).
            let dep = create(&store, "unfinished dependency", vec![]);
            let blocked = create(&store, "wants delegation too early", vec![dep.clone()]);
            hand_over(&store, &blocked);
            let board = store.read_board().unwrap();
            let t = board.ticket(&blocked).unwrap();
            assert!(matches!(t.column, Column::Todo), "{:?}", t.column);
            assert!(t.notes.last().unwrap().text.contains("handing to minesweeper failed"), "{:?}", t.notes);
            assert!(store.read_claims().unwrap().is_empty());

            // No repo/remote: the claim succeeds, the guards decline, and the daemon's claim is released again —
            // a card saying "minesweeper has this" with no issue behind it would be a lie.
            hand_over(&store, &dep);
            let board = store.read_board().unwrap();
            let t = board.ticket(&dep).unwrap();
            assert!(matches!(t.column, Column::Todo), "released back: {:?}", t.column);
            assert!(t.external.is_none());
            assert!(t.notes.last().unwrap().text.contains("no git repository or remote"), "{:?}", t.notes);
            assert!(store.read_claims().unwrap().is_empty(), "the released claim is gone");
        }

        #[test]
        fn graphql_answers_shape_into_issue_facts() {
            let issue = serde_json::json!({
                "number": 42,
                "state": "CLOSED",
                "labels": { "nodes": [{ "name": "autofix" }, { "name": "minesweeperFailed" }] },
                "comments": { "nodes": [
                    { "body": "Hi — I am Minesweeper, an automated bot." },
                    { "body": "Refined into the following sub-tasks:\n\n- [ ] #43 — half" },
                ] },
                "closedByPullRequestsReferences": { "nodes": [{
                    "number": 57,
                    "url": "https://github.com/o/r/pull/57",
                    "state": "MERGED",
                    "merged": true,
                    "mergeCommit": { "oid": "abc123" },
                    "headRefName": "myrepo-issue0042",
                }] },
            });
            let facts = issue_facts(&issue);
            assert!(facts.closed);
            assert_eq!(facts.labels, vec!["autofix", "minesweeperFailed"]);
            assert_eq!(facts.refine, vec![43]);
            let (pr, head) = facts.pr.expect("the closing reference is the PR");
            assert_eq!((pr.number, pr.state, pr.merged_commit.as_deref()), (57, PrState::Merged, Some("abc123")));
            assert_eq!(head, "myrepo-issue0042");
        }
    }
}
