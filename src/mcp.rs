//! `claude-kanban mcp` — Claude's face of the board: a stdio MCP server (rmcp) giving Claude typed tools instead of letting
//! it hand-edit JSON. Claude Code launches this via the plugin's `.mcp.json`; it is not meant to be run by hand.
//!
//! Every mutating tool funnels into [`crate::ops::apply`] exactly like the HTTP handlers do, and takes an
//! `expected_version` — the optimistic-concurrency token from the last `kanban_board` read. On a conflict the tool returns
//! a caller-visible error naming the current version, so Claude re-reads and retries instead of clobbering the human.
//!
//! The worktree tools (`kanban_worktree_start` / `kanban_worktree_finish`) are the one documented exception: no
//! `expected_version`, because their board writes are server-derived stamps, not view-based edits — see `worktree.rs`.
//!
//! stdout belongs to the protocol; logs go to stderr (see `cli::init_tracing`).

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;

use crate::{
    config::Config,
    ops::{self, Applied, NewEpicSpec, NewTicketSpec, Op, OpError, RefineTarget, TicketPatch, VerdictReview},
    store::{
        Store, derive,
        derive::BoardView,
        model::{Board, Column, ColumnId, Effort, EpicId, ReviewKind, Side, Status, TicketId, Verdict},
    },
};

/// The workflow contract, shipped to the client as server instructions. Public because opencode never surfaces MCP
/// server instructions, so `opencode/kanban-rules.md` must carry the same text — tests/manifests.rs pins the two
/// together.
pub const INSTRUCTIONS: &str = "A local Kanban board shared with a human (who sees it live in a browser). \
The lifecycle of a ready ticket: kanban_claim → kanban_worktree_start → work in the worktree, committing as you go → \
kanban_note progress → kanban_move to review. Do NOT call kanban_worktree_finish at close-out: the worktree is kept \
through review so the human can read the code and rework re-attaches instantly, and the board removes it once the \
ticket lands. kanban_move to review refuses while that worktree has uncommitted changes — commit them first. \
Done is not yours to declare: the board lands review tickets in done automatically once their branch or PR is merged \
into the local main branch — done means landed, and dependencies unblock only then (a discarded done ticket never \
unblocks anything). A review ticket can be claimed again for rework (PR feedback); its branch and worktree are kept \
and kanban_worktree_start re-attaches. A human can also send a review ticket back with feedback: it returns to doing \
unclaimed, kanban_next surfaces it as action \"rework\" ahead of any todo work, and the newest \"changes requested:\" \
note is the spec for that round — address it in the existing worktree and move it to review again. Accepting, \
requesting changes and discarding are the human's verdicts, made in the browser; never press one for them \
outside a /kanban:review loop the board's auto_review allows. Reviewing is not judging: a review loop writes its \
findings with kanban_review_comment onto the ticket's review log, whatever auto_review says — never as kanban_note \
progress, and never replacing what an earlier reviewer wrote. \
Accepting does not close a ticket — it clears the branch to land, and kanban_next then surfaces it as action \
\"land\" ahead of everything else. Take it with kanban_start_landing, never kanban_claim (claiming a review ticket is \
the rework path), rebase the branch onto the main branch and fast-forward main into it, then call kanban_next and let \
its sweep move the card. Resolve a rebase conflict only where the intent is unambiguous; anything you would have to \
guess at goes back to the human with kanban_block_landing, after git rebase --abort. \
Stubs are specs to write, not code to build: kanban_claim (the card sits pink in doing) → research → kanban_refine, \
which lands it back in todo at status=review for the human — no worktree. \
Only claim tickets kanban_next surfaces — ready (implement) or stub (refine), in todo, unblocked; never claim \
spontaneously outside an explicit work loop. Never touch draft tickets. Tickets you create default to status=review so \
the human vets them. kanban_update_ticket edits an existing ticket's fields — including depends_on, so a dependency \
discovered mid-flight can be added, dropped or rewired instead of living only in a note; it replaces the whole list, so \
read the ticket first and send the set you want. Mutating tools need expected_version from your latest kanban_board \
read (kanban_next also returns one — use it, its landing sweep may have advanced the board); on a version conflict, \
re-read and retry. \
`auto_merge` makes `/kanban:work` rebase and land the ticket's branch into main when it reaches review, without a human \
seeing the merge. Never set it on tickets you create unless the user explicitly asked for it. \
kanban_claim and kanban_start_landing stamp the card's owner, and kanban_note its author, with your own agent name by \
default — that is who the board shows did the work, so leave `agent` unset for your own actions and pass it only to act \
as someone else, the way /kanban:delegate names the external worker it hands a ticket to. \
kanban_board omits done tickets by default and returns a `done` summary of their ids instead — their specs and progress \
logs are the bulk of the board and finished work is not input to your next decision; pass include_done=true, or \
column=\"done\", when you actually need to read them.";

/// The MCP server: a thin adapter from tools onto the shared ops layer and read model.
#[derive(Debug, Clone)]
pub struct KanbanServer {
    store: Store,
    /// The name stamped as owner/author when a tool leaves `agent` unset — this harness's identity, set at launch.
    agent: String,
}

/// Run the stdio MCP server until the client disconnects. `agent` is the name stamped when a tool omits its own.
pub fn run(store: Store, agent: String) -> anyhow::Result<()> {
    tracing::info!(store = %store.dir().display(), agent = %agent, "MCP server on stdio");
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let service = KanbanServer { store, agent }.serve(stdio()).await?;
        service.waiting().await?;
        tracing::info!("MCP client disconnected");
        Ok(())
    })
}

// ---- tool parameter shapes ---------------------------------------------------------------------------------------------
// Doc comments become the schema descriptions Claude reads.

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct BoardParams {
    /// Restrict to one column: "todo", "doing", "review", or "done". Omit for the whole board.
    #[serde(default)]
    pub column: Option<String>,
    /// Include the full done tickets. Off by default: done work is finished, and its specs and
    /// progress logs are the bulk of the board. Only ask for them when you actually need the text.
    #[serde(default)]
    pub include_done: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TicketParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EpicParams {
    /// The epic id, e.g. "EP-1".
    pub epic: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ClaimParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// Who is claiming it. Defaults to this session's own agent name — set it only to claim for someone else.
    #[serde(default)]
    pub agent: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReleaseParams {
    /// The ticket id to give back, e.g. "K-7".
    pub ticket: String,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct StartLandingParams {
    /// The accepted review ticket you are about to land, e.g. "K-7".
    pub ticket: String,
    /// Who is landing it. Defaults to this session's own agent name — set it only to land for someone else.
    pub agent: Option<String>,
    /// The board version from your latest `kanban_board` or `kanban_next` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct BlockLandingParams {
    /// The ticket whose landing you could not finish, e.g. "K-7".
    pub ticket: String,
    /// What stopped it, in words the human can act on — name the conflicting paths, or the exact refusal git gave.
    /// This becomes the newest note on the card and is the only thing they have to go on.
    pub reason: String,
    /// The board version from your latest `kanban_board` or `kanban_next` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// Destination column: "todo", "doing", "review", or "done".
    pub to: String,
    /// Position within the destination column, 0 = top. Omit for the bottom. Position IS priority.
    #[serde(default)]
    pub position: Option<usize>,
    /// Record this git branch on the destination state, overriding whatever the ticket carried. Use when closing out a
    /// companion subtask worked on its parent ticket's branch — without it the ticket reaches review branchless and the
    /// auto-lander can never resolve it.
    #[serde(default)]
    pub branch: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateTicketParams {
    pub title: String,
    /// Markdown body — the spec of the work.
    #[serde(default)]
    pub body: Option<String>,
    /// Epic id this ticket belongs to, e.g. "EP-1".
    #[serde(default)]
    pub epic: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// The model this ticket's work should run on — one of the board's models (`kanban_board` returns the effective
    /// list as `models`), or any free-text model id. Omit to inherit whatever the worker session is already running.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for this ticket's work: "low", "medium", "high", "xhigh", or "max". Omit to inherit.
    #[serde(default)]
    pub effort: Option<String>,
    /// Let this ticket's branch land in main unattended once it reaches review. Defaults to false, and you never set it
    /// unless the user explicitly asked for it.
    #[serde(default)]
    pub auto_merge: Option<bool>,
    /// Ticket ids this one depends on; it stays blocked until they are all done.
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// "draft", "stub", "review", or "ready". Defaults to "review": the human promotes to ready — you never grow your own
    /// work queue unless explicitly told to create ready tickets.
    #[serde(default)]
    pub status: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

/// Distinguish "field absent" from "field present and null". `#[serde(default)]` yields `None` when the key is missing;
/// this yields `Some(None)` when it is there as `null` — the difference between "leave it alone" and "clear it".
#[allow(clippy::option_option)] // the three states are the point, and they mirror `TicketPatch`'s own nested options
fn present_or_null<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::deserialize(de).map(Some)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UpdateTicketParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// A new title. Omit to leave it alone.
    #[serde(default)]
    pub title: Option<String>,
    /// A new markdown body, replacing the spec wholesale. Omit to leave it alone.
    #[serde(default)]
    pub body: Option<String>,
    /// The whole label set, replacing the current one. Omit to leave it alone; `[]` clears every label.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// The whole dependency list, replacing the current one — read the ticket first and send the set you want, not just the
    /// edge you are adding. Omit to leave it alone; `[]` clears every dependency. Ids must exist, and the graph must stay
    /// acyclic: a dangling id or a cycle is refused and nothing is written.
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// Epic id, e.g. "EP-2". `null` detaches the ticket from its epic; omit to leave it alone.
    #[serde(default, deserialize_with = "present_or_null")]
    pub epic: Option<Option<String>>,
    /// The model this ticket's work should run on — one of the board's models (`kanban_board` returns the effective
    /// list as `models`), or any free-text model id. `null` clears the preference back to "inherit"; omit to leave it
    /// alone.
    #[serde(default, deserialize_with = "present_or_null")]
    pub model: Option<Option<String>>,
    /// Reasoning effort: "low", "medium", "high", "xhigh", or "max". `null` clears it back to "inherit"; omit to leave it
    /// alone.
    #[serde(default, deserialize_with = "present_or_null")]
    pub effort: Option<Option<String>>,
    /// Whether this ticket's branch may land in main unattended. `false` is the off state, so there is no `null` here —
    /// omit to leave it alone. Only ever set it to true at the user's explicit request.
    #[serde(default)]
    pub auto_merge: Option<bool>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateEpicParams {
    pub title: String,
    /// CSS hex colour for the epic's cards, e.g. "#7c9cf5". Omit to auto-pick from a palette.
    #[serde(default)]
    pub color: Option<String>,
    /// Markdown body.
    #[serde(default)]
    pub body: Option<String>,
    /// "draft", "stub", "review", or "ready". Defaults to "review".
    #[serde(default)]
    pub status: Option<String>,
    /// Let every ticket in this epic land in main unattended — one dial for the whole work stream. Defaults to false,
    /// and you never set it unless the user explicitly asked for it.
    #[serde(default)]
    pub auto_merge: Option<bool>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// The progress note to append (plain text or markdown).
    pub text: String,
    /// Who is writing. Defaults to this session's own agent name — set it only to write as someone else.
    #[serde(default)]
    pub agent: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct InlineParam {
    /// The file, relative to the repository root, as the diff names it.
    pub path: String,
    /// The line number, counted in the branch's version of the file — or in the pre-change version when `side` is "old".
    pub line: u32,
    /// "new" (default) or "old". Old is for a deleted line, which has no number in the new file.
    #[serde(default)]
    pub side: Option<String>,
    /// The comment about that line.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReviewCommentParams {
    /// The review ticket, e.g. "K-7".
    pub ticket: String,
    /// The review prose (markdown). Optional when `inline` carries everything.
    #[serde(default)]
    pub text: Option<String>,
    /// Line-anchored findings, each becoming its own entry on the review log. Prefer these over `path:line` bullets in
    /// `text`: the board renders and re-reads them structurally.
    #[serde(default)]
    pub inline: Option<Vec<InlineParam>>,
    /// Who is reviewing. Defaults to "auto-review".
    #[serde(default)]
    pub agent: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReviewParams {
    /// The review ticket to judge, e.g. "K-7".
    pub ticket: String,
    /// The verdict: `accept` (clear the branch to land, card stays in review), `request_changes` (send it back to the
    /// top of doing carrying the comment as the rework spec), or `discard` (retire it unlanded).
    pub verdict: String,
    /// The reviewer's message. **Required** for `request_changes` (it becomes the `changes requested:` note that is the
    /// spec for the next round) and for `discard` (the reason). Optional for `accept` — record detailed findings as
    /// `kanban_note`s first; this is only the one-line verdict.
    #[serde(default)]
    pub comment: Option<String>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NewTicketParam {
    pub title: String,
    /// Markdown body — a full spec, since these land in review.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Model for this subtask's work — one of the board's models (`kanban_board` returns the effective list as
    /// `models`), or any free-text model id. Omit to inherit; set it on the hard split.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for this subtask: "low", "medium", "high", "xhigh", or "max". Omit to inherit.
    #[serde(default)]
    pub effort: Option<String>,
    /// Whether this subtask may land unattended. Defaults to false; only propose true if the user asked for it.
    #[serde(default)]
    pub auto_merge: Option<bool>,
    /// Existing ticket ids, or "new:<i>" naming the i-th entry of `split_tickets` (0-based).
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// Epic id; omit to inherit the refine target's epic.
    #[serde(default)]
    pub epic: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NewEpicParam {
    pub title: String,
    /// CSS hex colour; omit to auto-pick.
    #[serde(default)]
    pub color: Option<String>,
    /// Markdown body.
    #[serde(default)]
    pub body: Option<String>,
    /// Tickets belonging to this new epic. These cannot be referenced by `new:<i>` placeholders.
    #[serde(default)]
    pub tickets: Option<Vec<NewTicketParam>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RefineParams {
    /// The stub (or review, for another pass) item being refined: a ticket id ("K-7") or an epic id ("EP-2").
    pub target: String,
    /// A better title, if the refinement produced one.
    #[serde(default)]
    pub title: Option<String>,
    /// The refined spec (markdown) — replaces the target's body.
    pub body: String,
    /// Split-off tickets, when the target is too big for one unit of work.
    #[serde(default)]
    pub split_tickets: Option<Vec<NewTicketParam>>,
    /// Split-off epics (each with its own tickets), when the target is really several work streams.
    #[serde(default)]
    pub split_epics: Option<Vec<NewEpicParam>>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct BindExternalParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// The external system, e.g. "github".
    #[serde(default)]
    pub provider: Option<String>,
    /// The kind of item there, e.g. "issue".
    #[serde(default)]
    pub kind: Option<String>,
    /// The item's number, e.g. 42.
    #[serde(default)]
    pub number: Option<u64>,
    /// The board version from your latest `kanban_board` read.
    pub expected_version: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WorktreeStartParams {
    /// The claimed ticket's id, e.g. "K-7".
    pub ticket: String,
    /// Branch slug override (branch is `k-7/<slug>`) — supply one when you can condense the title well.
    #[serde(default)]
    pub slug: Option<String>,
    /// Base ref for a fresh branch. Defaults to the main checkout's current HEAD.
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WorktreeFinishParams {
    /// The ticket's id, e.g. "K-7".
    pub ticket: String,
    /// Throw away uncommitted changes instead of refusing. Only with explicit human approval.
    #[serde(default)]
    pub force_discard: Option<bool>,
    /// Merge the branch into the main checkout's current branch too. Only with explicit human approval.
    #[serde(default)]
    pub merge: Option<bool>,
}

// ---- the tools -----------------------------------------------------------------------------------------------------------

#[tool_router]
impl KanbanServer {
    /// Read the board: columns, tickets (with derived blocked/claim facts), epics (with derived columns), the current
    /// version — pass that version as `expected_version` to mutating tools — plus `max_workers`, how many tickets a work
    /// loop may drive concurrently, `idle_time`, how many seconds it sleeps between polls when nothing is eligible, and
    /// `models`, the model vocabulary a ticket's `model` field should draw from.
    ///
    /// `implement_model` and `refine_model` are the board's **role defaults**: the model to work a ticket that names
    /// none of its own — the first for implementing and reworking, the second for refining a stub. Each is `null` when
    /// unset, which means inherit whatever the worker session runs. A ticket's own `model` always wins over these.
    ///
    /// Done tickets are **omitted by default**: finished work is not input to the next decision, and its specs and
    /// progress logs are the bulk of the board. In their place comes a `done` summary — `count` plus the `landed` and
    /// `discarded` id lists, kept apart because a discarded ticket never unblocks a dependent. Two ways to the full
    /// text: `include_done=true` for the whole board, or `column="done"` for that column alone. `version` always means
    /// the board's version whatever the shape, so it is a valid `expected_version` either way.
    ///
    /// `minesweeper: true` means delegation mode is live: claiming a ready ticket mirrors it to a labelled GitHub issue
    /// for an external daemon — never start a worktree or implement it yourself; the board tracks the daemon's PR back
    /// through review to done.
    ///
    /// `auto_review: true` means a `/kanban:review` loop may press a verdict on a review ticket with `kanban_review`
    /// (accept / request changes / discard); `false` (the default) keeps the verdict the human's and makes that tool
    /// refuse.
    #[tool]
    async fn kanban_board(&self, Parameters(p): Parameters<BoardParams>) -> Result<CallToolResult, ErrorData> {
        let column = match p.column.as_deref() {
            None => None,
            Some(c) => Some(c.parse::<ColumnId>().map_err(|e| ErrorData::invalid_params(e, None))?),
        };
        let include_done = p.include_done.unwrap_or(false);
        self.read(move |store| {
            let config = Config::load(store.dir())?;
            let view = derive::board_view(&store.read_board()?, &store.read_claims()?);
            let mut value = shape(view, column, include_done);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("max_workers".into(), config.max_workers().into());
                obj.insert("idle_time".into(), config.idle_time().into());
                obj.insert("minesweeper".into(), (cfg!(feature = "minesweeper") && config.minesweeper()).into());
                obj.insert("models".into(), config.models().into());
                obj.insert("implement_model".into(), config.implement_model().into());
                obj.insert("refine_model".into(), config.refine_model().into());
                obj.insert("auto_review".into(), config.auto_review().into());
            }
            Ok(value)
        })
        .await
    }

    /// Read a single ticket by id (e.g. "K-7") — a slice instead of the whole board, for a harness that only needs one
    /// card. The answer is the same shape as one entry of `kanban_board`'s `tickets` array: the ticket's own fields plus
    /// `blocked`, `auto_merge_effective`, and `claim` when someone is working it. Errors when the id is not on the board.
    #[tool]
    async fn kanban_ticket(&self, Parameters(p): Parameters<TicketParams>) -> Result<CallToolResult, ErrorData> {
        let id = TicketId(p.ticket);
        self.read(move |store| {
            let view = derive::board_view(&store.read_board()?, &store.read_claims()?);
            ticket_value(&view, &id).ok_or_else(|| OpError::NotFound(id.0.clone()))
        })
        .await
    }

    /// Read one epic by id (e.g. "EP-1") plus every ticket that belongs to it — a slice instead of the whole board.
    /// `epic` is the same shape as one entry of `kanban_board`'s `epics` array (epic fields, derived `column`, and its
    /// checklist); `tickets` carries each of those tickets in full, the same shape `kanban_ticket` returns. An epic that
    /// owns no tickets answers with `tickets: []` — that is a real, empty epic. Errors when the id is not on the board.
    #[tool]
    async fn kanban_epic(&self, Parameters(p): Parameters<EpicParams>) -> Result<CallToolResult, ErrorData> {
        let id = EpicId(p.epic);
        self.read(move |store| {
            let view = derive::board_view(&store.read_board()?, &store.read_claims()?);
            epic_value(&view, &id).ok_or_else(|| OpError::NotFound(id.0.clone()))
        })
        .await
    }

    /// Report this MCP server's identity: the crate `name`, its semantic `version`, and the source `repository` — the
    /// same build the board's web UI shows in its footer. A plain read that touches neither the board nor the store, so
    /// it needs no `expected_version`; use it to learn which build a harness is talking to (a bug report, a
    /// compatibility check). The version also rides the `initialize` handshake in `serverInfo`; this exposes it as a
    /// tool a session can call at any point.
    #[tool]
    async fn kanban_version(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::structured(serde_json::json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "repository": env!("CARGO_PKG_REPOSITORY"),
        })))
    }

    /// The next thing to work on, and what it needs. Returns the full ticket plus the action, or explains that nothing
    /// is eligible:
    ///
    /// - `"land"` — a human accepted this review ticket and cleared its branch to land. **Do not claim it** (claiming a review ticket is
    ///   the rework path and would drag it into doing): call `kanban_start_landing` to take the in-flight marker, then follow *Landing a
    ///   branch* in the work command — rebase the branch onto the main branch, fast-forward main into it, and let the sweep move the card.
    ///   Ranked **first**: it is minutes of work, it is what unblocks dependents, and a fresh worktree is branched off the main checkout's
    ///   HEAD, so landing first keeps the next ticket from starting on a stale main. The answer carries `branch`.
    /// - `"rework"` — a human reviewed this ticket and sent it back. It is sitting in doing, unclaimed, with its branch and worktree
    ///   intact: claim it, `kanban_worktree_start` (which re-attaches), treat the newest `changes requested:` note as the spec for this
    ///   round, then move it to review again. Ranked above todo work. That note may carry `- path:line — comment` bullets: they were
    ///   written against the reviewed diff, so each one names the exact line it is about. A bullet marked `(old line)` numbers the file
    ///   *before* the branch's changes.
    /// - `"implement"` — a ready ticket in todo: build it.
    /// - `"refine"` — a stub: write its spec, don't build it.
    ///
    /// Eligibility is otherwise the same throughout: unblocked, unclaimed, non-external. A ticket whose landing is
    /// already in flight, or whose last landing attempt was blocked and is waiting on a human, is never surfaced. First
    /// auto-lands any review tickets whose branches have provably reached the local main branch (offline git, no
    /// network), so use the `version` this tool returns for your next mutation — the sweep may have advanced it.
    ///
    /// `auto_merge` is the *effective* answer — the ticket's own flag or its epic's — and is the field `/kanban:work`
    /// reads to decide whether to land the branch itself. Read it here, not off the ticket: the ticket carries only its
    /// own say, so an epic-level grant is invisible there.
    ///
    /// When nothing is eligible the answer carries `waiting`: why each todo ticket does not qualify, and why each review
    /// ticket could not be proven landed. An idle board and a stuck one are otherwise the same silence.
    #[tool]
    async fn kanban_next(&self) -> Result<CallToolResult, ErrorData> {
        self.read(|store| {
            // Land what has already reached local main before computing eligibility — dependents unblock even in an
            // MCP-only session with no serve process. A failing sweep only delays landings; never fail the read.
            let stalled = match crate::land::sweep_reporting(store) {
                Ok(swept) => swept.stalled,
                Err(e) => {
                    tracing::warn!(error = %e, "landing sweep before kanban_next failed — answering from the board as-is");
                    vec![]
                }
            };
            let board = store.read_board()?;
            let claims = store.read_claims()?;
            Ok(if let Some(t) = derive::next_ticket(&board, &claims) {
                let action = action_for(t);
                serde_json::json!({ "version": board.version, "ticket": t, "action": action,
                    "branch": t.column.branch(), "auto_merge": derive::auto_merge(t, &board) })
            } else {
                let todo = derive::ineligible(&board, &claims);
                serde_json::json!({ "version": board.version, "ticket": null, "reason": nothing_eligible(&todo, &stalled),
                    "waiting": { "todo": waiting_list(&todo), "review": waiting_list(&stalled) } })
            })
        })
        .await
    }

    /// Claim a ticket: moves it to doing owned by you and records the live claim. Requires unblocked + unclaimed, and
    /// status ready (to implement) or stub (to refine — the card shows pink while you write the spec). A pure board
    /// mutation — create the worktree with `kanban_worktree_start` afterwards (implementation only; refining needs none).
    /// Review tickets are claimable too: that is the rework path (PR feedback) — the branch is kept and
    /// `kanban_worktree_start` re-attaches to it. So is a ticket a human sent back for changes: it sits in doing owned
    /// by the reviewer but unclaimed, and any agent may take it — that is what `kanban_next`'s `"rework"` action hands
    /// you.
    #[tool]
    async fn kanban_claim(&self, Parameters(p): Parameters<ClaimParams>) -> Result<CallToolResult, ErrorData> {
        let id = TicketId(p.ticket);
        let op = Op::Claim {
            id: id.clone(),
            agent: p.agent.unwrap_or_else(|| self.agent.clone()),
        };
        // Claiming is the MCP door into `doing` (kanban_move passes no owner, so it can't enter) — with the minesweeper
        // toggle on, a ready claim is the delegation moment.
        self.apply_then(Some(p.expected_version), op, move |store| {
            crate::minesweeper::delegate_entering_doing(store, &id)
        })
        .await
    }

    /// Give a claimed ticket back: drops the claim and returns the card to the top of todo.
    #[tool]
    async fn kanban_release(&self, Parameters(p): Parameters<ReleaseParams>) -> Result<CallToolResult, ErrorData> {
        self.apply(Some(p.expected_version), Op::Release { id: TicketId(p.ticket) }).await
    }

    /// Take the in-flight landing marker on an accepted review ticket, then land it: rebase its branch onto the main
    /// branch and fast-forward main into it, per *Landing a branch* in the work command. Call this **instead of**
    /// `kanban_claim` — claiming a review ticket is the rework path and would drag the card into doing, while this
    /// leaves it in review with no owner and simply shows "landing this" on the card.
    ///
    /// It is also the interlock between concurrent work loops: it refuses when another loop already holds the landing,
    /// when a human has not accepted the ticket, and when the last attempt was blocked and is still waiting on them. A
    /// refusal means the ticket is not yours — take the next one rather than retrying.
    ///
    /// The marker retires with the landing itself, so a successful land needs no cleanup: merge, then call
    /// `kanban_next`, whose sweep proves the landing and moves the card. If you cannot finish, say so with
    /// `kanban_block_landing` — never leave it hanging. (A marker whose agent died is ignored after 15 minutes, so a
    /// crash costs one stalled card, not a stuck one.)
    #[tool]
    async fn kanban_start_landing(&self, Parameters(p): Parameters<StartLandingParams>) -> Result<CallToolResult, ErrorData> {
        let op = Op::StartLanding {
            id: TicketId(p.ticket),
            agent: p.agent.unwrap_or_else(|| self.agent.clone()),
        };
        self.apply(Some(p.expected_version), op).await
    }

    /// Give a landing back to the human: flags the card with a danger badge, shades it red on the board, records
    /// `reason` as the newest note, and drops the in-flight marker. The ticket stays in review and is **not** offered
    /// for landing again until a human accepts it once more (having fixed whatever it was) or sends it back for
    /// changes — so this ends your involvement with that ticket; move on to the next one.
    ///
    /// Use it for anything you cannot resolve with confidence: a rebase conflict whose intent is ambiguous, a worktree
    /// with uncommitted changes, a main checkout on the wrong branch or holding the user's own work, a fast-forward
    /// that main moved out from under. **Abort the rebase first** — `git rebase --abort` — and never leave a repository
    /// mid-rebase behind you; then name the conflicting paths here so the human knows where to look.
    #[tool]
    async fn kanban_block_landing(&self, Parameters(p): Parameters<BlockLandingParams>) -> Result<CallToolResult, ErrorData> {
        self.apply(
            Some(p.expected_version),
            Op::BlockLanding {
                id: TicketId(p.ticket),
                reason: p.reason,
            },
        )
        .await
    }

    /// Move a ticket to a column at a position (0 = top; position is priority). Moving to review is the close-out for
    /// finished work: it keeps the branch (pass `branch` for a companion subtask worked on its parent's branch) and
    /// drops the live claim, and it **refuses while the ticket's worktree has uncommitted changes** — commit them
    /// first, because the worktree may live on volatile /tmp and only commits survive. The worktree itself is kept
    /// through review; the board removes it when the ticket lands. Done means *landed in local main* and normally
    /// happens automatically — the board lands review tickets itself once their branch or PR merges.
    #[tool]
    async fn kanban_move(&self, Parameters(p): Parameters<MoveParams>) -> Result<CallToolResult, ErrorData> {
        let to = p.to.parse().map_err(|e: String| ErrorData::invalid_params(e, None))?;
        let id = TicketId(p.ticket);
        let op = Op::MoveTicket {
            id: id.clone(),
            to,
            position: p.position,
            owner: None,
            branch: p.branch,
        };
        let (guard_id, expected_version) = (id.clone(), p.expected_version);
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            // The close-out's dirty check used to belong to `kanban_worktree_finish`, which no longer runs here — the
            // worktree outlives review now. So the guarantee moves onto the move itself. Agents only: a human dragging
            // the card in the browser is the override, exactly as discard is.
            if to == ColumnId::Review
                && let Some(path) = crate::worktree::dirty_worktree(&store, &guard_id)
            {
                return Err(OpError::Invalid(format!(
                    "the worktree at {} has uncommitted changes — commit them there before closing {guard_id} out",
                    path.display()
                )));
            }
            let applied = ops::apply(&store, Some(expected_version), op);
            if applied.is_ok() {
                // Entering review is what arms the landing proof, and a later auto-merge may delete the branch minutes
                // later — so the tip is observed here, not whenever the next sweep happens to run.
                crate::land::observe_entering_review(&store, to);
            }
            applied
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(shape_applied(result))
    }

    /// Create a ticket at the bottom of todo. Defaults to `status=review` — the human promotes it to ready.
    #[tool]
    async fn kanban_create_ticket(&self, Parameters(p): Parameters<CreateTicketParams>) -> Result<CallToolResult, ErrorData> {
        let status = parse_status_or(p.status.as_deref(), Status::Review)?;
        let op = Op::CreateTicket {
            title: p.title,
            body: p.body.unwrap_or_default(),
            epic: p.epic.map(EpicId),
            labels: p.labels.unwrap_or_default(),
            model: p.model,
            effort: parse_effort(p.effort.as_deref())?,
            auto_merge: p.auto_merge.unwrap_or(false),
            depends_on: p.depends_on.unwrap_or_default().into_iter().map(TicketId).collect(),
            status,
        };
        self.apply(Some(p.expected_version), op).await
    }

    /// Edit an existing ticket's descriptive fields — including `depends_on`, which is how you rewire the dependency graph
    /// after the fact (add an edge you missed, drop one that turned out not to hold, or clear the list). Every field is
    /// optional: omit one and it is left untouched. `depends_on` and `labels` replace the whole list rather than merging,
    /// so read the ticket first and send the set you want; `[]` clears. `epic`, `model` and `effort` additionally accept
    /// `null`, which clears them. Dangling dependency ids and cycles are refused, and nothing is written when they are.
    /// Drafts are refused — they are the human's to shape. `status` is deliberately absent: promoting work to ready stays
    /// the human's call.
    #[tool]
    async fn kanban_update_ticket(&self, Parameters(p): Parameters<UpdateTicketParams>) -> Result<CallToolResult, ErrorData> {
        let patch = TicketPatch {
            title: p.title,
            body: p.body,
            labels: p.labels,
            depends_on: p.depends_on.map(|ids| ids.into_iter().map(TicketId).collect()),
            epic: p.epic.map(|e| e.map(EpicId)),
            model: p.model,
            effort: p.effort.map(|e| parse_effort(e.as_deref())).transpose()?,
            auto_merge: p.auto_merge,
        };
        let id = TicketId(p.ticket);
        self.apply_unless_draft(p.expected_version, id.clone(), Op::UpdateTicket { id, patch })
            .await
    }

    /// Create an epic. Defaults to `status=review` — the human promotes it to ready.
    #[tool]
    async fn kanban_create_epic(&self, Parameters(p): Parameters<CreateEpicParams>) -> Result<CallToolResult, ErrorData> {
        let status = parse_status_or(p.status.as_deref(), Status::Review)?;
        let op = Op::CreateEpic {
            title: p.title,
            color: p.color,
            body: p.body.unwrap_or_default(),
            status,
            auto_merge: p.auto_merge.unwrap_or(false),
        };
        self.apply(Some(p.expected_version), op).await
    }

    /// Append a timestamped entry to a ticket's progress log. Note progress after each meaningful chunk of work.
    #[tool]
    async fn kanban_note(&self, Parameters(p): Parameters<NoteParams>) -> Result<CallToolResult, ErrorData> {
        let op = Op::AddNote {
            id: TicketId(p.ticket),
            text: p.text,
            author: Some(p.agent.unwrap_or_else(|| self.agent.clone())),
        };
        self.apply(Some(p.expected_version), op).await
    }

    /// Write a review onto a review ticket's `review` log — the reviewer's findings, as prose and/or line-anchored
    /// `inline` entries. **Always allowed**, whatever the board's `auto_review` says: writing a review is not pressing a
    /// verdict. This is where a `/kanban:review` loop's findings go; `kanban_note` is the worker's progress log and is
    /// not for reviews. Entries append: a second review, or a human's own comments, sit beside earlier ones rather than
    /// replacing them, and the human sees new entries live under their own comment box. Refuses outside `review` and on
    /// external tickets.
    #[tool]
    async fn kanban_review_comment(&self, Parameters(p): Parameters<ReviewCommentParams>) -> Result<CallToolResult, ErrorData> {
        let op = review_comment_op(p).map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        self.apply(Some(op.1), op.0).await
    }

    /// Press a review verdict on a ticket — accept, request changes, or discard — the way a human would in the browser.
    /// This is a **guarded** tool for a `/kanban:review` loop, not part of the work loop: it applies **only** when the
    /// board's `auto_review` is on. With `auto_review` off (the default) it refuses, because the verdict is the human's;
    /// your `kanban_review_comment` review is the deliverable then, and the card stays in review for them.
    ///
    /// - `accept` — clears the branch to land (the card stays in `review`; the sweep lands it, exactly as pressing Accept does). Not
    ///   "done": there is still no proof the code reached main.
    /// - `request_changes` — needs a `comment`; sends the card to the top of `doing` carrying it as the `changes requested:` note that a
    ///   worker reads as the rework spec.
    /// - `discard` — retires it in `done` unlanded (`discarded: true`), `comment` folded in as the reason.
    ///
    /// Write your findings with `kanban_review_comment` **before** calling this; `comment` is the one-line verdict, not
    /// the review, and is recorded on the review log with the verdict.
    #[tool]
    async fn kanban_review(&self, Parameters(p): Parameters<ReviewParams>) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            let config = Config::load(store.dir())?;
            let op = review_verdict_op(config.auto_review(), TicketId(p.ticket), &p.verdict, p.comment.as_deref())?;
            ops::apply(&store, Some(p.expected_version), op)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(shape_applied(result))
    }

    /// Bind a ticket to a work item in another system (e.g. the GitHub issue it was mirrored to) — used by the delegate
    /// flow. Bound tickets are worked elsewhere: they never get a worktree here and `kanban_next` skips them. Omit provider
    /// to unbind.
    #[tool]
    async fn kanban_bind_external(&self, Parameters(p): Parameters<BindExternalParams>) -> Result<CallToolResult, ErrorData> {
        let external = match (p.provider, p.kind, p.number) {
            (Some(provider), Some(kind), Some(number)) => Some(crate::store::model::External {
                provider,
                kind,
                number,
                closed: false,
                flag: None,
                sub_issues: vec![],
            }),
            (None, None, None) => None,
            _ => {
                return Err(ErrorData::invalid_params(
                    "provider, kind, and number must be given together (or all omitted to unbind)",
                    None,
                ));
            }
        };
        self.apply(
            Some(p.expected_version),
            Op::BindExternal {
                id: TicketId(p.ticket),
                external,
            },
        )
        .await
    }

    /// Create (or re-attach) the claimed ticket's branch and worktree — `k-7/<slug>`, sparse, excluding .kanban/ — and
    /// stamp them onto the board. Work in the reported path for the ticket's lifetime; subtasks of a ticket in progress
    /// stay in their parent's worktree (never create a worktree from inside a worktree). Commit after logical chunks.
    #[tool]
    async fn kanban_worktree_start(&self, Parameters(p): Parameters<WorktreeStartParams>) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let opts = crate::worktree::StartOpts {
            base: p.base,
            slug: p.slug,
            dir: None,
            no_sparse: false,
        };
        let result = tokio::task::spawn_blocking(move || crate::worktree::start(&store, &TicketId(p.ticket), &opts))
            .await
            .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(match result {
            Ok(report) => CallToolResult::structured(serde_json::to_value(&report).unwrap_or_default()),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }

    /// Remove the ticket's worktree deliberately (refuses if dirty), keeping the branch.
    ///
    /// **This is not the close-out.** A worktree is kept through review — so the human can read the code on disk and a
    /// rework round re-attaches to it — and the board removes it by itself once the ticket lands. Close out with
    /// `kanban_move` to **review** and nothing else. Reach for this tool only to remove a worktree on purpose before
    /// then: the pre-merge step of auto-merge, or abandoning a checkout early. Merging or pushing the branch is the
    /// human's call — never do it unasked.
    #[tool]
    async fn kanban_worktree_finish(&self, Parameters(p): Parameters<WorktreeFinishParams>) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::worktree::finish(
                &store,
                &TicketId(p.ticket),
                p.force_discard.unwrap_or(false),
                p.merge.unwrap_or(false),
            )
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(match result {
            Ok(report) => CallToolResult::structured(serde_json::to_value(&report).unwrap_or_default()),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }

    /// Record a refinement of a stub: replace its spec (you did the thinking — this records it), optionally splitting off
    /// new tickets/epics. Everything touched or created lands in `status=review` for the human's verdict. A ticket claimed
    /// for refinement returns to the top of todo and its claim drops — no worktree is ever involved. Atomic.
    #[tool]
    async fn kanban_refine(&self, Parameters(p): Parameters<RefineParams>) -> Result<CallToolResult, ErrorData> {
        let target = if p.target.starts_with("EP-") {
            RefineTarget::Epic(EpicId(p.target))
        } else {
            RefineTarget::Ticket(TicketId(p.target))
        };
        let op = Op::Refine {
            target,
            title: p.title,
            body: p.body,
            split_tickets: p
                .split_tickets
                .unwrap_or_default()
                .into_iter()
                .map(new_ticket_spec)
                .collect::<Result<_, _>>()?,
            split_epics: p
                .split_epics
                .unwrap_or_default()
                .into_iter()
                .map(new_epic_spec)
                .collect::<Result<_, _>>()?,
        };
        self.apply(Some(p.expected_version), op).await
    }
}

// ---- plumbing --------------------------------------------------------------------------------------------------------------

impl KanbanServer {
    /// Read-only tools: run against the store off the async threads, answer with structured JSON.
    async fn read(
        &self,
        f: impl FnOnce(&Store) -> Result<serde_json::Value, OpError> + Send + 'static,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(match result {
            Ok(value) => CallToolResult::structured(value),
            Err(e) => tool_error(&e),
        })
    }

    /// Mutating tools: one op through the shared funnel, with the caller's expected version.
    async fn apply(&self, expected_version: Option<u64>, op: Op) -> Result<CallToolResult, ErrorData> {
        self.apply_then(expected_version, op, |_| {}).await
    }

    /// [`apply`](Self::apply), plus a hook run against the store once the op has succeeded — the seam
    /// [`crate::land::observe_entering_review`] needs. It runs in the same blocking task, after `ops::apply` has
    /// released the store lock, so the hook is free to take it again.
    async fn apply_then(
        &self,
        expected_version: Option<u64>,
        op: Op,
        after: impl FnOnce(&Store) + Send + 'static,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            let applied = ops::apply(&store, expected_version, op);
            if applied.is_ok() {
                after(&store);
            }
            applied
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(shape_applied(result))
    }

    /// Mutating tools that drafts are off-limits to: check the status, then apply. The check and the apply take the store
    /// lock separately, but `expected_version` closes the gap — anything landing in between bumps the board version, and
    /// the apply is then rejected as a conflict rather than acting on the status this read saw.
    async fn apply_unless_draft(&self, expected_version: u64, id: TicketId, op: Op) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            refuse_draft(&store.read_board()?, &id)?;
            ops::apply(&store, Some(expected_version), op)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(shape_applied(result))
    }
}

/// What the surfaced ticket needs, in precedence order. Landing wins over everything: a human has already approved it,
/// it is minutes of git, and until it lands its dependents stay blocked and the next worktree branches off a stale main.
/// Rework comes next — a card a human sent back is already specced and already built, so the newest `changes requested:`
/// note is the only spec that round needs, whatever `status` says. `stub` then means the spec itself is the work.
///
/// The column is what separates landing from the rest: `accepted` only ever means anything in `review`, and
/// [`derive::next_ticket`] has already applied the full eligibility rules by the time this names the answer.
fn action_for(t: &crate::store::model::Ticket) -> &'static str {
    if t.column.id() == crate::store::model::ColumnId::Review {
        "land"
    } else if t.changes_requested {
        "rework"
    } else if t.status == Status::Stub {
        "refine"
    } else {
        "implement"
    }
}

/// How many `waiting` entries either list carries before it is cut short. The answer is read on every idle poll of a
/// loop that may run for hours; a board with two hundred blocked tickets needs to say so, not recite them.
const WAITING_CAP: usize = 20;

/// One `waiting` list for the wire, capped — and saying so when it was, because a silently truncated list reads exactly
/// like a complete one.
fn waiting_list(entries: &[(TicketId, String)]) -> serde_json::Value {
    let shown: Vec<serde_json::Value> = entries
        .iter()
        .take(WAITING_CAP)
        .map(|(id, why)| serde_json::json!({ "ticket": id, "reason": why }))
        .collect();
    match entries.len().checked_sub(WAITING_CAP) {
        Some(more) if more > 0 => serde_json::json!({ "tickets": shown, "not_shown": more }),
        _ => serde_json::json!({ "tickets": shown }),
    }
}

/// The one-line answer to "why is nothing eligible?" — which has to distinguish a board with no work from a board whose
/// work is all waiting on something, because a loop that cannot tell those apart idles forever on the second.
fn nothing_eligible(todo: &[(TicketId, String)], stalled: &[(TicketId, String)]) -> String {
    let tickets = |n: usize| if n == 1 { "1 ticket".to_owned() } else { format!("{n} tickets") };
    match (todo.is_empty(), stalled.is_empty()) {
        (true, true) => "no eligible ticket: nothing is waiting in todo".to_owned(),
        (true, false) => format!(
            "no eligible ticket: todo is empty, and {} in review cannot be proven landed — see waiting.review",
            tickets(stalled.len())
        ),
        (false, true) => format!(
            "no eligible ticket: {} in todo, none of them eligible — see waiting.todo",
            tickets(todo.len())
        ),
        (false, false) => format!(
            "no eligible ticket: {} in todo, none of them eligible, and {} in review cannot be proven landed — see waiting",
            tickets(todo.len()),
            tickets(stalled.len())
        ),
    }
}

/// Shape an applied op for the wire: the op's own result, plus the board's new version and any ids it minted.
/// The author stamped on a verdict pressed by a review loop, so a note reads "accepted by auto-review" — honest that no
/// human made the call.
const REVIEW_AUTHOR: &str = "auto-review";

/// Map a `kanban_review_comment` request to its op and version. Pure, like [`review_verdict_op`], and deliberately
/// without an `auto_review` argument: the flag gates verdicts, never reviews. Anchors with a bad `side` are refused
/// here rather than silently read as the new side.
fn review_comment_op(p: ReviewCommentParams) -> Result<(Op, u64), OpError> {
    let inline = p.inline.unwrap_or_default().into_iter().map(|i| {
        let side = i.side.as_deref().unwrap_or("new").parse::<Side>().map_err(OpError::Invalid)?;
        Ok(ReviewKind::Inline {
            path: i.path,
            line: i.line,
            side,
            text: i.text,
        })
    });
    let text = p
        .text
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .map(|text| Ok(ReviewKind::Comment { text }));
    let entries = inline.chain(text).collect::<Result<Vec<_>, OpError>>()?;
    let author = p.agent.unwrap_or_else(|| REVIEW_AUTHOR.into());
    Ok((
        Op::AddReview {
            id: TicketId(p.ticket),
            author,
            entries,
        },
        p.expected_version,
    ))
}

/// Map a `kanban_review` request to the verdict op, gated on the board's `auto_review`. Pure so the gate and the
/// verdict routing are unit-testable without a store: the tool loads the flag and applies whatever this returns.
fn review_verdict_op(auto_review: bool, id: TicketId, verdict: &str, comment: Option<&str>) -> Result<Op, OpError> {
    if !auto_review {
        return Err(OpError::Invalid(format!(
            "auto_review is off for this board — the verdict on {id} is the human's to give in the browser. Record your \
             assessment with kanban_review_comment and leave the card in review."
        )));
    }
    let comment = comment.map(str::trim).filter(|c| !c.is_empty());
    let review = VerdictReview::text(comment.unwrap_or_default());
    let by = REVIEW_AUTHOR.to_owned();
    match verdict.parse::<Verdict>().map_err(OpError::Invalid)? {
        Verdict::Accept => Ok(Op::AcceptReview { id, by, review }),
        Verdict::RequestChanges => match comment {
            Some(_) => Ok(Op::RequestChanges { id, by, review }),
            None => Err(OpError::Invalid(
                "request_changes needs a comment — say what has to change; it becomes the rework spec".into(),
            )),
        },
        Verdict::Discard => Ok(Op::DiscardTicket {
            id,
            reason: comment.unwrap_or("discarded by auto-review").to_owned(),
            by,
            review,
        }),
    }
}

fn shape_applied(result: Result<Applied, OpError>) -> CallToolResult {
    match result {
        Ok(Applied {
            version,
            created_ids,
            mut result,
        }) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("version".into(), version.into());
                if !created_ids.is_empty() {
                    obj.insert("created".into(), serde_json::json!(created_ids));
                }
            }
            CallToolResult::structured(result)
        }
        Err(e) => tool_error(&e),
    }
}

/// Drafts are the human's scratch space — the standing instruction is "never touch draft tickets" — so Claude's edit tools
/// refuse them even though the ops layer and the browser both allow the edit.
fn refuse_draft(board: &Board, id: &TicketId) -> Result<(), OpError> {
    match board.ticket(id) {
        None => Err(OpError::NotFound(id.to_string())),
        Some(t) if t.status == Status::Draft => Err(OpError::Invalid(format!(
            "{id} is a draft — drafts are the human's to shape, not yours to edit"
        ))),
        Some(_) => Ok(()),
    }
}

// ---- board shaping ---------------------------------------------------------------------------------------------------------

/// What replaces the omitted done tickets, spelled out so a fresh session knows both ways back to the full text.
const DONE_OMITTED_NOTE: &str = "done tickets omitted — call kanban_board with include_done=true, or column=\"done\", to read them";

/// Shape a board read for the wire. Pure and synchronous; the shared read model itself is never trimmed, because the
/// HTML views walk the same [`BoardView`] and the browser board shows done in full.
///
/// An explicit `column` wins outright and is honoured verbatim — the caller asked for one column and gets exactly it,
/// with no `done` summary bolted on, which is what makes `column="done"` the unchanged way to read finished work.
/// Otherwise done tickets are dropped unless `include_done`, and a summary of their ids takes their place.
fn shape(view: BoardView, column: Option<ColumnId>, include_done: bool) -> serde_json::Value {
    match column {
        Some(col) => to_value(&restrict_to_column(view, col)),
        None if include_done => to_value(&view),
        None => omit_done(view),
    }
}

fn to_value(view: &BoardView) -> serde_json::Value {
    serde_json::to_value(view).unwrap_or_default()
}

/// One ticket from a board view, by id — the lookup `kanban_ticket` uses, and `kanban_epic` reuses for its `tickets`.
fn ticket_value(view: &BoardView, id: &TicketId) -> Option<serde_json::Value> {
    view.tickets
        .iter()
        .find(|t| t.ticket.id == *id)
        .map(|t| serde_json::to_value(t).unwrap_or_default())
}

/// One epic plus the tickets that belong to it, from a board view. `epic` mirrors an entry of `kanban_board`'s `epics`
/// array (including its own checklist `tickets`); the outer `tickets` carries each member ticket in full, exactly as
/// `kanban_ticket` would return it.
fn epic_value(view: &BoardView, id: &EpicId) -> Option<serde_json::Value> {
    let epic = view.epics.iter().find(|e| e.epic.id == *id)?;
    let tickets: Vec<_> = view.tickets.iter().filter(|t| t.ticket.epic.as_ref() == Some(id)).collect();
    Some(serde_json::json!({ "epic": epic, "tickets": tickets }))
}

/// Keep only what sits in one column. Epics are filtered by their *derived* column, exactly as before.
fn restrict_to_column(mut view: BoardView, col: ColumnId) -> BoardView {
    view.tickets.retain(|t| t.ticket.column.id() == col);
    view.epics.retain(|e| e.column == col);
    view
}

/// Drop the done tickets, replacing them with a `done` summary. Epics are left alone: a derived column of `done` is
/// legitimate and an epic object is small. `version` is untouched — it is the board's version, not the subset's.
fn omit_done(mut view: BoardView) -> serde_json::Value {
    let landed = done_ids(&view, false);
    let discarded = done_ids(&view, true);
    let summary = serde_json::json!({
        "count": landed.len() + discarded.len(),
        "landed": landed,
        "discarded": discarded,
        "note": DONE_OMITTED_NOTE,
    });
    view.tickets.retain(|t| t.ticket.column.id() != ColumnId::Done);
    let mut value = to_value(&view);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("done".into(), summary);
    }
    value
}

/// The ids of done tickets on one side of the discarded line. Landed and discarded are reported separately on purpose:
/// a discarded ticket is closed but never satisfies a dependency, and one flat list would invite that wrong inference.
fn done_ids(view: &BoardView, want_discarded: bool) -> Vec<&str> {
    view.tickets
        .iter()
        .filter(|t| matches!(t.ticket.column, Column::Done { discarded, .. } if discarded == want_discarded))
        .map(|t| t.ticket.id.0.as_str())
        .collect()
}

/// Domain failures are *tool-level* errors — Claude should read them and adapt; only infrastructure failures become
/// protocol errors. A version conflict tells Claude exactly how to recover.
fn tool_error(e: &OpError) -> CallToolResult {
    let text = match e.version_conflict() {
        Some((_, actual)) => {
            format!("version conflict: the board is now at version {actual} — call kanban_board and retry with expected_version={actual}")
        }
        None => e.to_string(),
    };
    CallToolResult::error(vec![ContentBlock::text(text)])
}

fn parse_status_or(s: Option<&str>, default: Status) -> Result<Status, ErrorData> {
    match s {
        None => Ok(default),
        Some(s) => s.parse().map_err(|e: String| ErrorData::invalid_params(e, None)),
    }
}

/// An absent effort is "inherit"; a present one must name a level, so a typo is refused rather than silently dropped.
fn parse_effort(s: Option<&str>) -> Result<Option<Effort>, ErrorData> {
    s.map(|s| s.parse().map_err(|e: String| ErrorData::invalid_params(e, None)))
        .transpose()
}

fn new_ticket_spec(p: NewTicketParam) -> Result<NewTicketSpec, ErrorData> {
    Ok(NewTicketSpec {
        title: p.title,
        body: p.body.unwrap_or_default(),
        labels: p.labels.unwrap_or_default(),
        model: p.model,
        effort: parse_effort(p.effort.as_deref())?,
        auto_merge: p.auto_merge.unwrap_or(false),
        depends_on: p.depends_on.unwrap_or_default(),
        epic: p.epic.map(EpicId),
    })
}

fn new_epic_spec(p: NewEpicParam) -> Result<NewEpicSpec, ErrorData> {
    Ok(NewEpicSpec {
        title: p.title,
        color: p.color,
        body: p.body.unwrap_or_default(),
        tickets: p
            .tickets
            .unwrap_or_default()
            .into_iter()
            .map(new_ticket_spec)
            .collect::<Result<_, _>>()?,
    })
}

#[tool_handler]
#[allow(clippy::unused_async_trait_impl)] // the macro generates an async fn with no awaits; not ours to change
impl ServerHandler for KanbanServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(INSTRUCTIONS.into());
        info
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::store::model::{Board, Epic, Ticket};

    fn ticket(id: &str, epic: &str, column: Column) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: id.into(),
            epic: Some(EpicId(epic.into())),
            status: Status::Ready,
            body: String::new(),
            labels: vec![],
            model: None,
            effort: None,
            auto_merge: false,
            changes_requested: false,
            accepted: false,
            landing_blocked: false,
            depends_on: vec![],
            notes: vec![],
            review: vec![],
            external: None,
            pr: None,
            column,
        }
    }

    fn done(discarded: bool) -> Column {
        Column::Done {
            branch: None,
            completed_at: Utc::now(),
            discarded,
        }
    }

    fn epic(id: &str) -> Epic {
        Epic {
            id: EpicId(id.into()),
            title: id.into(),
            color: "#fff".into(),
            status: Status::Ready,
            body: String::new(),
            auto_merge: false,
        }
    }

    /// Tickets in all four columns, one of the done ones discarded. EP-1 spans the board (derived column `doing`);
    /// EP-2's tickets are all done, so its derived column is `done` — the case the default shape must not eat.
    fn view() -> BoardView {
        let mut board = Board::empty();
        board.version = 216;
        board.epics = vec![epic("EP-1"), epic("EP-2")];
        board.tickets = vec![
            ticket("K-1", "EP-1", Column::Todo),
            ticket(
                "K-2",
                "EP-1",
                Column::Doing {
                    owner: "claude".into(),
                    branch: None,
                },
            ),
            ticket("K-3", "EP-1", Column::Review { owner: None, branch: None }),
            ticket("K-4", "EP-1", done(false)),
            ticket("K-5", "EP-2", done(false)),
            ticket("K-6", "EP-2", done(true)),
        ];
        derive::board_view(&board, &[])
    }

    fn ids(value: &serde_json::Value, key: &str) -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn review_verdict_is_refused_unless_auto_review_is_on() {
        let id = TicketId("K-7".into());
        // Off: every verdict refuses with the "human's to give" message, whatever the verdict or comment.
        for verdict in ["accept", "request_changes", "discard"] {
            let err = review_verdict_op(false, id.clone(), verdict, Some("x")).unwrap_err();
            assert!(
                matches!(&err, OpError::Invalid(m) if m.contains("auto_review is off")),
                "{verdict}: {err}"
            );
        }
    }

    #[test]
    fn review_comment_builds_one_append_and_needs_no_auto_review() {
        let params = |text: Option<&str>, inline: Option<Vec<InlineParam>>, agent: Option<&str>| ReviewCommentParams {
            ticket: "K-7".into(),
            text: text.map(str::to_owned),
            inline,
            agent: agent.map(str::to_owned),
            expected_version: 9,
        };
        let anchor = |path: &str, line: u32, side: Option<&str>, text: &str| InlineParam {
            path: path.into(),
            line,
            side: side.map(str::to_owned),
            text: text.into(),
        };

        // Inline entries lead, the prose follows, and the author defaults to the review loop's name.
        let (op, version) = review_comment_op(params(
            Some("  looks right overall  "),
            Some(vec![
                anchor("src/a.rs", 7, None, "nit"),
                anchor("src/a.rs", 3, Some("old"), "why gone?"),
            ]),
            None,
        ))
        .unwrap();
        assert_eq!(version, 9);
        let Op::AddReview { id, author, entries } = op else {
            panic!("not an AddReview: {op:?}")
        };
        assert_eq!(id, TicketId("K-7".into()));
        assert_eq!(author, REVIEW_AUTHOR);
        assert_eq!(
            entries,
            vec![
                ReviewKind::Inline {
                    path: "src/a.rs".into(),
                    line: 7,
                    side: Side::New,
                    text: "nit".into()
                },
                ReviewKind::Inline {
                    path: "src/a.rs".into(),
                    line: 3,
                    side: Side::Old,
                    text: "why gone?".into()
                },
                ReviewKind::Comment {
                    text: "looks right overall".into()
                },
            ]
        );

        // Blank prose is not an entry; a named agent is the author; a bad side is refused up front.
        let (op, _) = review_comment_op(params(Some("   "), Some(vec![anchor("x", 1, None, "y")]), Some("sam"))).unwrap();
        assert!(matches!(op, Op::AddReview { author, entries, .. } if author == "sam" && entries.len() == 1));
        assert!(matches!(
            review_comment_op(params(Some("t"), Some(vec![anchor("x", 1, Some("left"), "y")]), None)),
            Err(OpError::Invalid(_))
        ));
    }

    #[test]
    fn review_verdict_maps_each_verdict_to_its_op_when_on() {
        let id = TicketId("K-7".into());
        assert!(matches!(
            review_verdict_op(true, id.clone(), "accept", None).unwrap(),
            Op::AcceptReview { by, .. } if by == REVIEW_AUTHOR
        ));
        assert!(matches!(
            review_verdict_op(true, id.clone(), "request_changes", Some("  tighten naming  ")).unwrap(),
            Op::RequestChanges { review, by, .. } if review.text == "tighten naming" && by == REVIEW_AUTHOR
        ));
        assert!(matches!(
            review_verdict_op(true, id.clone(), "discard", Some("dead end")).unwrap(),
            Op::DiscardTicket { reason, .. } if reason == "dead end"
        ));
        // request_changes with no usable comment is refused — the note is the rework spec, so it cannot be empty.
        assert!(matches!(
            review_verdict_op(true, id.clone(), "request_changes", Some("   ")),
            Err(OpError::Invalid(_))
        ));
        assert!(matches!(
            review_verdict_op(true, id.clone(), "request_changes", None),
            Err(OpError::Invalid(_))
        ));
        // discard falls back to a default reason; an unknown verdict is refused.
        assert!(matches!(
            review_verdict_op(true, id.clone(), "discard", None).unwrap(),
            Op::DiscardTicket { .. }
        ));
        assert!(matches!(review_verdict_op(true, id, "approve", None), Err(OpError::Invalid(_))));
    }

    #[test]
    fn the_default_board_read_omits_done_tickets_and_says_so() {
        let shaped = shape(view(), None, false);
        assert_eq!(
            ids(&shaped, "tickets"),
            ["K-1", "K-2", "K-3"],
            "the default shape carries todo/doing/review only"
        );
        assert_eq!(
            shaped["done"]["count"], 3,
            "the summary counts every done ticket, discarded included"
        );
        assert_eq!(shaped["done"]["landed"], serde_json::json!(["K-4", "K-5"]));
        assert_eq!(shaped["done"]["discarded"], serde_json::json!(["K-6"]));
        assert!(
            shaped["done"]["note"].as_str().unwrap().contains("include_done=true"),
            "the summary must name the way back to the full text: {shaped}"
        );
    }

    #[test]
    fn include_done_returns_the_whole_board_unshaped() {
        let view = view();
        let shaped = shape(view.clone(), None, true);
        assert_eq!(
            shaped,
            serde_json::to_value(&view).unwrap(),
            "include_done is the byte-identical compatibility escape hatch"
        );
        assert!(
            shaped.get("done").is_none(),
            "nothing was omitted, so there is nothing to summarize"
        );
    }

    #[test]
    fn an_explicit_column_is_honoured_verbatim_and_adds_no_summary() {
        // include_done is deliberately absent throughout: asking for a column answers that column, done or not.
        let got: Vec<(Vec<String>, bool)> = ColumnId::ALL
            .iter()
            .map(|&col| {
                let shaped = shape(view(), Some(col), false);
                (ids(&shaped, "tickets"), shaped.get("done").is_some())
            })
            .collect();
        let want = [vec!["K-1"], vec!["K-2"], vec!["K-3"], vec!["K-4", "K-5", "K-6"]]
            .map(|ids| (ids.iter().map(ToString::to_string).collect::<Vec<_>>(), false));
        assert_eq!(
            got,
            want.to_vec(),
            "in ColumnId::ALL order, each column returns exactly its own tickets and never a done summary — which is what \
             makes column=\"done\" the unchanged way to read finished work"
        );
    }

    #[test]
    fn a_discarded_done_ticket_is_listed_apart_from_landed_ones() {
        let shaped = shape(view(), None, false);
        let landed = shaped["done"]["landed"].as_array().unwrap();
        let discarded = shaped["done"]["discarded"].as_array().unwrap();
        assert!(
            discarded.contains(&serde_json::json!("K-6")),
            "a discarded ticket is still done and still reported"
        );
        assert!(
            !landed.contains(&serde_json::json!("K-6")),
            "but never as landed — a discarded dependency does not unblock its dependents"
        );
    }

    #[test]
    fn the_version_is_the_boards_version_whatever_the_shape() {
        let shapes = [
            shape(view(), None, false),
            shape(view(), None, true),
            shape(view(), Some(ColumnId::Todo), false),
        ];
        let versions: Vec<&serde_json::Value> = shapes.iter().map(|s| &s["version"]).collect();
        assert_eq!(
            versions,
            [&serde_json::json!(216); 3],
            "version means the board's version, not the version of the subset returned — so every shape yields a token \
             that is still valid as expected_version"
        );
    }

    #[test]
    fn epics_survive_the_default_shape() {
        let shaped = shape(view(), None, false);
        let epics = ids(&shaped, "epics");
        assert_eq!(
            epics,
            ["EP-1", "EP-2"],
            "epics are never filtered — a derived column of done is legitimate"
        );
        assert_eq!(shaped["epics"][1]["column"], "done", "and EP-2 is exactly that case");
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use crate::store::model::{Column, Status, Ticket, TicketId};

    fn bare(status: Status, changes_requested: bool) -> Ticket {
        Ticket {
            id: TicketId("K-1".into()),
            title: "t".into(),
            epic: None,
            status,
            body: String::new(),
            labels: vec![],
            model: None,
            effort: None,
            auto_merge: false,
            changes_requested,
            accepted: false,
            landing_blocked: false,
            depends_on: vec![],
            notes: vec![],
            review: vec![],
            external: None,
            pr: None,
            column: Column::Todo,
        }
    }

    /// The precedence the work loop depends on: rework beats refine beats implement.
    #[test]
    fn rework_beats_refine_beats_implement() {
        assert_eq!(action_for(&bare(Status::Ready, false)), "implement");
        assert_eq!(action_for(&bare(Status::Stub, false)), "refine");
        assert_eq!(action_for(&bare(Status::Ready, true)), "rework");
        assert_eq!(
            action_for(&bare(Status::Stub, true)),
            "rework",
            "feedback outranks even an unwritten spec"
        );
    }
}
