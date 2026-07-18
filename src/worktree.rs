//! One ticket, one checkout: the worktree lifecycle (`start` / `finish` / `list`).
//!
//! Claude never works in the user's checkout. `start` gives a claimed ticket its own branch (`k-7/<slug>`) and its own
//! worktree under the worktree root, with a per-worktree sparse checkout that excludes `.kanban/` — so no worktree can
//! ever even *see* a board file, let alone conflict over one. `finish` removes the worktree and keeps the branch;
//! integrating it is the user's explicit next step (or `--merge` in one motion).
//!
//! Board writes from here still funnel through [`crate::ops`] ([`Op::StampWorktree`] / [`Op::ClearWorktreePath`]) under
//! the same lock as everything else. Git operations deliberately happen *outside* that lock — they're slow — and the
//! stamps are server-derived facts, not view-based edits, which is why these are the documented `expected_version`
//! exceptions.
//!
//! Worktrees live at `<root>/<repo-name>-<hash>/<ticket-id>`; the root defaults to `/tmp/claude-kanban` (volatile on
//! purpose — commits, branch, claim, and card all survive a wipe; `start` prunes and re-attaches to the ticket's branch,
//! so recovery is just running it again).

use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::Serialize;

use crate::{
    config::Config,
    git::{self, git},
    ops::{self, Op},
    store::{
        Store,
        model::{Column, Ticket, TicketId},
    },
};

/// Sparse checkout from a worktree is only safe from this git version up: on older gits it can flip
/// `core.sparseCheckout` in the *shared* repo config and blank files out of the main checkout.
const SPARSE_GIT_FLOOR: (u32, u32) = (2, 36);

/// Words that carry no weight in a branch slug.
const STOPWORDS: [&str; 18] =
    ["the", "a", "an", "of", "for", "to", "and", "or", "in", "on", "with", "from", "into", "at", "by", "is", "be", "based"];

/// Options for [`start`], mirroring the CLI flags and the MCP tool's arguments.
#[derive(Debug, Default)]
pub struct StartOpts {
    /// Base ref for a fresh branch. Defaults to the main checkout's current `HEAD`.
    pub base: Option<String>,
    /// Branch slug override, for when a human (or a fast model) can condense the title better.
    pub slug: Option<String>,
    /// Worktree root override (`--dir` / `KANBAN_WORKTREE_DIR`); beats config, which beats `/tmp/claude-kanban`.
    pub dir: Option<PathBuf>,
    /// Skip the sparse checkout and take a full worktree (the board stays inert there anyway, thanks to anchoring).
    pub no_sparse: bool,
}

/// What [`start`] did.
#[derive(Debug, Serialize)]
pub struct StartReport {
    pub ticket: String,
    pub branch: String,
    pub path: PathBuf,
    /// True when an existing `k-7/*` branch (or live worktree) was re-attached instead of creating a fresh one.
    pub reattached: bool,
    /// Whether the worktree excludes `.kanban/` via sparse checkout.
    pub sparse: bool,
    pub warnings: Vec<String>,
}

/// What [`finish`] did. The branch is the headline: it survives, and integrating it is the user's next step.
#[derive(Debug, Serialize)]
pub struct FinishReport {
    pub ticket: String,
    pub branch: Option<String>,
    pub merged: bool,
    /// The worktree that was removed, if one still existed.
    pub removed: Option<PathBuf>,
}

/// One row of [`list`]: a ticket worktree joined with its claim.
#[derive(Debug, Serialize)]
pub struct WorktreeRow {
    /// The ticket this branch belongs to, when the branch follows the `k-<n>/…` scheme.
    pub ticket: Option<String>,
    pub branch: String,
    pub path: PathBuf,
    /// Uncommitted changes in the worktree.
    pub dirty: bool,
    /// The registered path no longer exists on disk (e.g. a /tmp wipe) — restore with `worktree start`.
    pub missing: bool,
    /// Who claims the ticket, per the live-claims sidecar.
    pub agent: Option<String>,
}

/// Create (or re-attach) the ticket's branch and worktree, then stamp branch + path onto the board and claim.
pub fn start(store: &Store, id: &TicketId, opts: &StartOpts) -> anyhow::Result<StartReport> {
    let repo = repo_root(store)?;
    let ticket = load_ticket(store, id)?;
    if ticket.external.is_some() {
        bail!("{id} is external — it is worked elsewhere and never gets a worktree here");
    }
    if !matches!(ticket.column, Column::Doing { .. }) {
        bail!("{id} is not in doing — claim it first (kanban_claim, or drag it to doing on the board)");
    }

    // Always prune first: stale registrations (a wiped /tmp) would otherwise block re-adding the same path.
    git(&repo, &["worktree", "prune"])?;

    let mut warnings = Vec::new();
    let sparse = sparse_available(opts.no_sparse, &mut warnings);
    let (branch, path, reattached) = if let Some(branch) = existing_branch(&repo, id)? {
        let path = if let Some(live) = live_worktree_for(&repo, &branch)? {
            live // fully alive: nothing to create
        } else {
            let path = worktree_path(store, &repo, id, opts)?;
            add_worktree(&repo, &path, &branch, None, sparse)?;
            path
        };
        (branch, path, true)
    } else {
        let slug = opts.slug.clone().unwrap_or_else(|| derive_slug(&ticket.title));
        let branch = format!("{}/{}", id.0.to_lowercase(), slug);
        let base = opts.base.clone().unwrap_or_else(|| "HEAD".to_owned());
        let path = worktree_path(store, &repo, id, opts)?;
        add_worktree(&repo, &path, &branch, Some(&base), sparse)?;
        (branch, path, false)
    };

    if repo.join(".gitmodules").exists() {
        git(&path, &["submodule", "update", "--init", "--recursive"]).context("initialising submodules in the worktree")?;
    }
    copy_authorised_files(store, &repo, &path, &mut warnings)?;

    ops::apply(store, None, Op::StampWorktree { id: id.clone(), branch: branch.clone(), path: path.clone() })?;
    tracing::info!(ticket = %id, %branch, path = %path.display(), reattached, sparse, "worktree started");
    Ok(StartReport { ticket: id.to_string(), branch, path, reattached, sparse, warnings })
}

/// Remove the ticket's worktree (refusing if dirty unless `force_discard`), optionally merging the branch into the main
/// checkout's current branch first. The branch always survives.
pub fn finish(store: &Store, id: &TicketId, force_discard: bool, merge: bool) -> anyhow::Result<FinishReport> {
    let repo = repo_root(store)?;
    let ticket = load_ticket(store, id)?;
    let branch = ticket.column.branch().map(str::to_owned).or(existing_branch(&repo, id)?);

    let worktree = match &branch {
        Some(b) => live_worktree_for(&repo, b)?.filter(|p| p.exists()),
        None => None,
    };

    if let Some(path) = &worktree {
        let dirty = !git(path, &["status", "--porcelain"])?.is_empty();
        if dirty && !force_discard {
            bail!(
                "the worktree at {} has uncommitted changes — commit them there, or pass --force-discard to throw them away",
                path.display()
            );
        }
    }

    let mut merged = false;
    if merge {
        let Some(branch) = &branch else { bail!("{id} has no branch to merge") };
        // --merge targets THE integration branch, not wherever the checkout happens to sit: done means landed in main.
        // With no configured or detectable main branch there is no target to enforce, and the v1 behaviour (merge into
        // the current branch) is all that's left.
        if let Some(main) = Config::load(store.dir())?.main_branch(&repo) {
            match git::current_branch(&repo) {
                Some(current) if current == main => {}
                Some(current) => bail!("the main checkout is on '{current}', not '{main}' — finish --merge only targets the main branch"),
                None => bail!("the main checkout is on a detached HEAD — check out '{main}' before finish --merge"),
            }
        }
        // The board itself is tracked and mutates constantly — that's the tracker doing its job, not user work at risk,
        // so changes under .kanban/ don't count as dirt here.
        if !git(&repo, &["status", "--porcelain", "--", ".", ":(exclude).kanban"])?.is_empty() {
            bail!("the main checkout has uncommitted changes — merge refused; commit or stash them first");
        }
        git(&repo, &["merge", "--no-edit", branch]).with_context(|| format!("merging {branch} into the main branch"))?;
        merged = true;
    }

    if let Some(path) = &worktree {
        let mut args = vec!["worktree", "remove"];
        if force_discard {
            args.push("--force");
        }
        let path_str = path.to_string_lossy().into_owned();
        args.push(&path_str);
        git(&repo, &args)?;
    }
    git(&repo, &["worktree", "prune"])?;

    ops::apply(store, None, Op::ClearWorktreePath { id: id.clone() })?;
    tracing::info!(ticket = %id, branch = branch.as_deref().unwrap_or("-"), merged, "worktree finished");
    Ok(FinishReport { ticket: id.to_string(), branch, merged, removed: worktree })
}

/// Every registered ticket worktree joined with the live claims — plus claims whose worktree has vanished entirely.
pub fn list(store: &Store) -> anyhow::Result<Vec<WorktreeRow>> {
    let repo = repo_root(store)?;
    let claims = store.read_claims()?;
    let mut rows: Vec<WorktreeRow> = worktree_entries(&repo)?
        .into_iter()
        .skip(1) // the first entry is the main checkout itself
        .filter_map(|entry| {
            let branch = entry.branch?;
            let ticket = ticket_for_branch(&branch);
            let missing = !entry.path.exists();
            let dirty = !missing && git(&entry.path, &["status", "--porcelain"]).is_ok_and(|out| !out.is_empty());
            let agent = ticket
                .as_ref()
                .and_then(|t| crate::store::find_claim(&claims, &TicketId(t.clone())))
                .map(|c| c.agent.clone());
            Some(WorktreeRow { ticket, branch, path: entry.path, dirty, missing, agent })
        })
        .collect();

    // Claims pointing at paths git no longer knows (e.g. pruned after a wipe): surface them as missing rather than
    // letting "Claude is working on this" ghost around with no row at all.
    let orphaned: Vec<WorktreeRow> = claims
        .iter()
        .filter(|c| c.path.is_some() && !rows.iter().any(|r| r.ticket.as_deref() == Some(c.ticket.0.as_str())))
        .map(|c| WorktreeRow {
            ticket: Some(c.ticket.to_string()),
            branch: String::new(),
            path: c.path.clone().unwrap_or_default(),
            dirty: false,
            missing: true,
            agent: Some(c.agent.clone()),
        })
        .collect();
    rows.extend(orphaned);
    Ok(rows)
}

// ---- the git plumbing --------------------------------------------------------------------------------------------------

/// The repository the store belongs to: the store's parent directory, which by anchoring *is* the main working tree.
/// Shared with `pr.rs`, whose git questions are about the same repository.
pub(crate) fn repo_root(store: &Store) -> anyhow::Result<PathBuf> {
    let parent = store.dir().parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
    git::main_worktree(&parent).with_context(|| format!("{} is not inside a git repository — worktrees need one", parent.display()))
}

fn load_ticket(store: &Store, id: &TicketId) -> anyhow::Result<Ticket> {
    store.read_board()?.ticket(id).cloned().with_context(|| format!("{id} not found on the board"))
}

/// The ticket's existing branch, by the `<id>/*` naming scheme that makes branch → ticket unambiguous.
fn existing_branch(repo: &Path, id: &TicketId) -> anyhow::Result<Option<String>> {
    let pattern = format!("refs/heads/{}/*", id.0.to_lowercase());
    let out = git(repo, &["for-each-ref", "--format=%(refname:short)", &pattern])?;
    Ok(out.lines().next().map(str::to_owned))
}

/// A parsed stanza of `git worktree list --porcelain`.
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
}

fn worktree_entries(repo: &Path) -> anyhow::Result<Vec<WorktreeEntry>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = current.take() {
                entries.push(done);
            }
            current = Some(WorktreeEntry { path: PathBuf::from(path), branch: None });
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(entry) = current.as_mut()
        {
            entry.branch = Some(branch.to_owned());
        }
    }
    entries.extend(current);
    Ok(entries)
}

/// The registered worktree currently on `branch`, if any.
fn live_worktree_for(repo: &Path, branch: &str) -> anyhow::Result<Option<PathBuf>> {
    Ok(worktree_entries(repo)?.into_iter().skip(1).find(|e| e.branch.as_deref() == Some(branch)).map(|e| e.path))
}

/// `k-7/rate-limit-login` → `K-7`.
fn ticket_for_branch(branch: &str) -> Option<String> {
    let (prefix, _) = branch.split_once('/')?;
    let n = prefix.strip_prefix("k-")?;
    n.chars().all(|c| c.is_ascii_digit()).then(|| format!("K-{n}"))
}

/// Create the worktree, sparse if asked: `--no-checkout` first so `.kanban/` is excluded before a single file lands.
fn add_worktree(repo: &Path, path: &Path, branch: &str, base: Option<&str>, sparse: bool) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let path_str = path.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if sparse {
        args.push("--no-checkout");
    }
    args.push(&path_str);
    match base {
        Some(base) => args.extend(["-b", branch, base]),
        None => args.push(branch), // re-attach to the existing branch
    }
    git(repo, &args).with_context(|| format!("creating the worktree at {}", path.display()))?;

    if sparse {
        git(path, &["sparse-checkout", "set", "--no-cone", "/*", "!/.kanban/"])?;
        git(path, &["checkout"])?;
    }
    Ok(())
}

fn sparse_available(no_sparse: bool, warnings: &mut Vec<String>) -> bool {
    if no_sparse {
        return false;
    }
    match git::version() {
        Some(v) if v >= SPARSE_GIT_FLOOR => true,
        v => {
            warnings.push(format!(
                "git {} is below {}.{} — falling back to a full worktree (safe: the board is anchored to the main checkout anyway)",
                v.map_or_else(|| "??".into(), |(maj, min)| format!("{maj}.{min}")),
                SPARSE_GIT_FLOOR.0,
                SPARSE_GIT_FLOOR.1
            ));
            false
        }
    }
}

/// `<root>/<repo-name>-<hash8>/<ticket-id>` — the hash keeps two repos both named `api` apart.
fn worktree_path(store: &Store, repo: &Path, id: &TicketId, opts: &StartOpts) -> anyhow::Result<PathBuf> {
    let config = Config::load(store.dir())?;
    let root = config.worktree_root(opts.dir.clone());
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash8 = format!("{:08x}", hasher.finish() & 0xffff_ffff);
    let name = repo.file_name().map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned());
    Ok(root.join(format!("{name}-{hash8}")).join(&id.0))
}

/// Lowercase, split on non-alphanumerics, drop stopwords, keep the first three words (≤24 chars):
/// "Rate-limit the login route" → `rate-limit-login`.
fn derive_slug(title: &str) -> String {
    let words: Vec<&str> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 1)
        .filter(|w| !STOPWORDS.contains(&w.to_lowercase().as_str()))
        .take(3)
        .collect();
    let slug: String = words.join("-").to_lowercase();
    let trimmed: String = slug.chars().take(24).collect();
    let trimmed = trimmed.trim_end_matches('-').to_owned();
    if trimmed.is_empty() { "work".to_owned() } else { trimmed }
}

/// Copy config-authorised, *gitignored* files (`.env`, local certs) into a fresh worktree. Only files git actually
/// ignores are copied — the config cannot smuggle tracked or unknown files.
fn copy_authorised_files(store: &Store, repo: &Path, worktree: &Path, warnings: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load(store.dir())?;
    for entry in &config.copy_to_worktrees {
        let source = repo.join(entry);
        if !source.is_file() {
            warnings.push(format!("copy_to_worktrees: {entry} is not a file — skipped"));
            continue;
        }
        if git(repo, &["check-ignore", "-q", entry]).is_err() {
            warnings.push(format!("copy_to_worktrees: {entry} is not gitignored — skipped (only ignored files may be copied)"));
            continue;
        }
        let target = worktree.join(entry);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &target).with_context(|| format!("copying {entry} into the worktree"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_condense_like_design_md_says() {
        assert_eq!(derive_slug("Rate-limit the login route"), "rate-limit-login");
        assert_eq!(derive_slug("Add session refresh"), "add-session-refresh");
        assert_eq!(derive_slug("Add authorization based on OAuth from Google"), "add-authorization-oauth");
        assert_eq!(derive_slug("The Of For"), "work", "all-stopword titles still get a slug");
        assert_eq!(derive_slug("Ünïcode Tïtle Here Now"), "code-tle-here", "non-ascii splits rather than panics");
    }

    #[test]
    fn same_named_repos_get_distinct_worktree_paths() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (repo_a, repo_b) = (a.path().join("api"), b.path().join("api"));
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        let id = TicketId("K-1".into());
        let path_a = worktree_path(&Store::at(repo_a.join(".kanban")), &repo_a, &id, &StartOpts::default()).unwrap();
        let path_b = worktree_path(&Store::at(repo_b.join(".kanban")), &repo_b, &id, &StartOpts::default()).unwrap();
        assert_ne!(path_a, path_b, "the path hash keeps two repos both named `api` apart");
        for p in [&path_a, &path_b] {
            assert!(p.ends_with("K-1"), "{}", p.display());
            assert!(p.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("api-"), "{}", p.display());
        }
    }

    #[test]
    fn branch_to_ticket_mapping_is_strict() {
        assert_eq!(ticket_for_branch("k-7/rate-limit-login").as_deref(), Some("K-7"));
        assert_eq!(ticket_for_branch("k-12/x").as_deref(), Some("K-12"));
        assert_eq!(ticket_for_branch("feature/nope"), None);
        assert_eq!(ticket_for_branch("k-x/nope"), None);
        assert_eq!(ticket_for_branch("myrepo-issue0042"), None, "external branches are data, not a format");
    }
}
