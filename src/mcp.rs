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
    ops::{self, Applied, NewEpicSpec, NewTicketSpec, Op, OpError, RefineTarget},
    store::{Store, derive, derive::BoardView, model::{Column, ColumnId, Effort, EpicId, Status, TicketId}},
};

/// What Claude is called on the board when a tool doesn't say otherwise.
const DEFAULT_AGENT: &str = "claude";

/// The workflow contract, shipped to the client as server instructions.
const INSTRUCTIONS: &str = "A local Kanban board shared with a human (who sees it live in a browser). \
The lifecycle of a ready ticket: kanban_claim → kanban_worktree_start → work in the worktree, committing as you go → \
kanban_note progress → kanban_worktree_finish → kanban_move to review. \
Done is not yours to declare: the board lands review tickets in done automatically once their branch or PR is merged \
into the local main branch — done means landed, and dependencies unblock only then (a discarded done ticket never \
unblocks anything). A review ticket can be claimed again for rework (PR feedback); its branch is kept and \
kanban_worktree_start re-attaches. \
Stubs are specs to write, not code to build: kanban_claim (the card sits pink in doing) → research → kanban_refine, \
which lands it back in todo at status=review for the human — no worktree. \
Only claim tickets kanban_next surfaces — ready (implement) or stub (refine), in todo, unblocked; never claim \
spontaneously outside an explicit work loop. Never touch draft tickets. Tickets you create default to status=review so \
the human vets them. Mutating tools need expected_version from your latest kanban_board read (kanban_next also returns \
one — use it, its landing sweep may have advanced the board); on a version conflict, re-read and retry. \
kanban_board omits done tickets by default and returns a `done` summary of their ids instead — their specs and progress \
logs are the bulk of the board and finished work is not input to your next decision; pass include_done=true, or \
column=\"done\", when you actually need to read them.";

/// The MCP server: a thin adapter from tools onto the shared ops layer and read model.
#[derive(Debug, Clone)]
pub struct KanbanServer {
    store: Store,
}

/// Run the stdio MCP server until the client disconnects.
pub fn run(store: Store) -> anyhow::Result<()> {
    tracing::info!(store = %store.dir().display(), "MCP server on stdio");
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let service = KanbanServer { store }.serve(stdio()).await?;
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
pub struct ClaimParams {
    /// The ticket id, e.g. "K-7".
    pub ticket: String,
    /// Who is claiming it. Defaults to "claude".
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
    /// The model this ticket's work should run on — an alias like "opus"/"sonnet"/"haiku", or a full id like
    /// "claude-opus-4-8". Omit to inherit whatever the worker session is already running.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for this ticket's work: "low", "medium", "high", "xhigh", or "max". Omit to inherit.
    #[serde(default)]
    pub effort: Option<String>,
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
    /// Who is writing. Defaults to "claude".
    #[serde(default)]
    pub agent: Option<String>,
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
    /// Model for this subtask's work — an alias like "opus", or a full id. Omit to inherit; set it on the hard split.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for this subtask: "low", "medium", "high", "xhigh", or "max". Omit to inherit.
    #[serde(default)]
    pub effort: Option<String>,
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
    /// loop may drive concurrently, and `idle_time`, how many seconds it sleeps between polls when nothing is eligible.
    ///
    /// Done tickets are **omitted by default**: finished work is not input to the next decision, and its specs and
    /// progress logs are the bulk of the board. In their place comes a `done` summary — `count` plus the `landed` and
    /// `discarded` id lists, kept apart because a discarded ticket never unblocks a dependent. Two ways to the full
    /// text: `include_done=true` for the whole board, or `column="done"` for that column alone. `version` always means
    /// the board's version whatever the shape, so it is a valid `expected_version` either way.
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
            }
            Ok(value)
        })
        .await
    }

    /// The next thing to work on: the highest ticket in todo that is unblocked, unclaimed, non-external, and either
    /// ready (action "implement") or stub (action "refine" — write its spec, don't build it). Returns the full ticket
    /// plus the action, or explains that nothing is eligible. First auto-lands any review tickets whose branches have
    /// provably reached the local main branch (offline git, no network), so use the `version` this tool returns for
    /// your next mutation — the sweep may have advanced it.
    #[tool]
    async fn kanban_next(&self) -> Result<CallToolResult, ErrorData> {
        self.read(|store| {
            // Land what has already reached local main before computing eligibility — dependents unblock even in an
            // MCP-only session with no serve process. A failing sweep only delays landings; never fail the read.
            if let Err(e) = crate::land::sweep(store) {
                tracing::warn!(error = %e, "landing sweep before kanban_next failed — answering from the board as-is");
            }
            let board = store.read_board()?;
            let claims = store.read_claims()?;
            Ok(match derive::next_ticket(&board, &claims) {
                Some(t) => {
                    let action = if t.status == Status::Stub { "refine" } else { "implement" };
                    serde_json::json!({ "version": board.version, "ticket": t, "action": action })
                }
                None => serde_json::json!({ "version": board.version, "ticket": null,
                    "reason": "no eligible ticket: nothing in todo is ready or stub, unblocked, unclaimed, and non-external" }),
            })
        })
        .await
    }

    /// Claim a ticket: moves it to doing owned by you and records the live claim. Requires unblocked + unclaimed, and
    /// status ready (to implement) or stub (to refine — the card shows pink while you write the spec). A pure board
    /// mutation — create the worktree with `kanban_worktree_start` afterwards (implementation only; refining needs none).
    /// Review tickets are claimable too: that is the rework path (PR feedback) — the branch is kept and
    /// `kanban_worktree_start` re-attaches to it.
    #[tool]
    async fn kanban_claim(&self, Parameters(p): Parameters<ClaimParams>) -> Result<CallToolResult, ErrorData> {
        let op = Op::Claim { id: TicketId(p.ticket), agent: p.agent.unwrap_or_else(|| DEFAULT_AGENT.into()) };
        self.apply(Some(p.expected_version), op).await
    }

    /// Give a claimed ticket back: drops the claim and returns the card to the top of todo.
    #[tool]
    async fn kanban_release(&self, Parameters(p): Parameters<ReleaseParams>) -> Result<CallToolResult, ErrorData> {
        self.apply(Some(p.expected_version), Op::Release { id: TicketId(p.ticket) }).await
    }

    /// Move a ticket to a column at a position (0 = top; position is priority). Moving to review is the close-out for
    /// finished work: it keeps the branch (pass `branch` for a companion subtask worked on its parent's branch) and
    /// drops the live claim. Done means *landed in local main* and normally happens automatically — the board lands
    /// review tickets itself once their branch or PR merges.
    #[tool]
    async fn kanban_move(&self, Parameters(p): Parameters<MoveParams>) -> Result<CallToolResult, ErrorData> {
        let to = p.to.parse().map_err(|e: String| ErrorData::invalid_params(e, None))?;
        let op = Op::MoveTicket { id: TicketId(p.ticket), to, position: p.position, owner: None, branch: p.branch };
        self.apply(Some(p.expected_version), op).await
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
            depends_on: p.depends_on.unwrap_or_default().into_iter().map(TicketId).collect(),
            status,
        };
        self.apply(Some(p.expected_version), op).await
    }

    /// Create an epic. Defaults to `status=review` — the human promotes it to ready.
    #[tool]
    async fn kanban_create_epic(&self, Parameters(p): Parameters<CreateEpicParams>) -> Result<CallToolResult, ErrorData> {
        let status = parse_status_or(p.status.as_deref(), Status::Review)?;
        let op = Op::CreateEpic { title: p.title, color: p.color, body: p.body.unwrap_or_default(), status };
        self.apply(Some(p.expected_version), op).await
    }

    /// Append a timestamped entry to a ticket's progress log. Note progress after each meaningful chunk of work.
    #[tool]
    async fn kanban_note(&self, Parameters(p): Parameters<NoteParams>) -> Result<CallToolResult, ErrorData> {
        let op = Op::AddNote { id: TicketId(p.ticket), text: p.text, author: Some(p.agent.unwrap_or_else(|| DEFAULT_AGENT.into())) };
        self.apply(Some(p.expected_version), op).await
    }

    /// Bind a ticket to a work item in another system (e.g. the GitHub issue it was mirrored to) — used by the delegate
    /// flow. Bound tickets are worked elsewhere: they never get a worktree here and `kanban_next` skips them. Omit provider
    /// to unbind.
    #[tool]
    async fn kanban_bind_external(&self, Parameters(p): Parameters<BindExternalParams>) -> Result<CallToolResult, ErrorData> {
        let external = match (p.provider, p.kind, p.number) {
            (Some(provider), Some(kind), Some(number)) => Some(crate::store::model::External { provider, kind, number }),
            (None, None, None) => None,
            _ => return Err(ErrorData::invalid_params("provider, kind, and number must be given together (or all omitted to unbind)", None)),
        };
        self.apply(Some(p.expected_version), Op::BindExternal { id: TicketId(p.ticket), external }).await
    }

    /// Create (or re-attach) the claimed ticket's branch and worktree — `k-7/<slug>`, sparse, excluding .kanban/ — and
    /// stamp them onto the board. Work in the reported path for the ticket's lifetime; subtasks of a ticket in progress
    /// stay in their parent's worktree (never create a worktree from inside a worktree). Commit after logical chunks.
    #[tool]
    async fn kanban_worktree_start(&self, Parameters(p): Parameters<WorktreeStartParams>) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let opts = crate::worktree::StartOpts { base: p.base, slug: p.slug, dir: None, no_sparse: false };
        let result = tokio::task::spawn_blocking(move || crate::worktree::start(&store, &TicketId(p.ticket), &opts))
            .await
            .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(match result {
            Ok(report) => CallToolResult::structured(serde_json::to_value(&report).unwrap_or_default()),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }

    /// Remove the ticket's worktree once the work is committed (refuses if dirty), keeping the branch. Close out by
    /// moving the ticket to **review** with `kanban_move` afterwards — the board lands it in done once the branch or its
    /// PR merges into local main. Merging or pushing the branch is the human's call — never do it unasked.
    #[tool]
    async fn kanban_worktree_finish(&self, Parameters(p): Parameters<WorktreeFinishParams>) -> Result<CallToolResult, ErrorData> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::worktree::finish(&store, &TicketId(p.ticket), p.force_discard.unwrap_or(false), p.merge.unwrap_or(false))
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
        let target = if p.target.starts_with("EP-") { RefineTarget::Epic(EpicId(p.target)) } else { RefineTarget::Ticket(TicketId(p.target)) };
        let op = Op::Refine {
            target,
            title: p.title,
            body: p.body,
            split_tickets: p.split_tickets.unwrap_or_default().into_iter().map(new_ticket_spec).collect::<Result<_, _>>()?,
            split_epics: p.split_epics.unwrap_or_default().into_iter().map(new_epic_spec).collect::<Result<_, _>>()?,
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
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || ops::apply(&store, expected_version, op))
            .await
            .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?;
        Ok(match result {
            Ok(Applied { version, created_ids, mut result }) => {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("version".into(), version.into());
                    if !created_ids.is_empty() {
                        obj.insert("created".into(), serde_json::json!(created_ids));
                    }
                }
                CallToolResult::structured(result)
            }
            Err(e) => tool_error(&e),
        })
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
    s.map(|s| s.parse().map_err(|e: String| ErrorData::invalid_params(e, None))).transpose()
}

fn new_ticket_spec(p: NewTicketParam) -> Result<NewTicketSpec, ErrorData> {
    Ok(NewTicketSpec {
        title: p.title,
        body: p.body.unwrap_or_default(),
        labels: p.labels.unwrap_or_default(),
        model: p.model,
        effort: parse_effort(p.effort.as_deref())?,
        depends_on: p.depends_on.unwrap_or_default(),
        epic: p.epic.map(EpicId),
    })
}

fn new_epic_spec(p: NewEpicParam) -> Result<NewEpicSpec, ErrorData> {
    Ok(NewEpicSpec {
        title: p.title,
        color: p.color,
        body: p.body.unwrap_or_default(),
        tickets: p.tickets.unwrap_or_default().into_iter().map(new_ticket_spec).collect::<Result<_, _>>()?,
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
            depends_on: vec![],
            notes: vec![],
            external: None,
            pr: None,
            column,
        }
    }

    fn done(discarded: bool) -> Column {
        Column::Done { branch: None, completed_at: Utc::now(), discarded }
    }

    fn epic(id: &str) -> Epic {
        Epic { id: EpicId(id.into()), title: id.into(), color: "#fff".into(), status: Status::Ready, body: String::new() }
    }

    /// Tickets in all four columns, one of the done ones discarded. EP-1 spans the board (derived column `doing`);
    /// EP-2's tickets are all done, so its derived column is `done` — the case the default shape must not eat.
    fn view() -> BoardView {
        let mut board = Board::empty();
        board.version = 216;
        board.epics = vec![epic("EP-1"), epic("EP-2")];
        board.tickets = vec![
            ticket("K-1", "EP-1", Column::Todo),
            ticket("K-2", "EP-1", Column::Doing { owner: "claude".into(), branch: None }),
            ticket("K-3", "EP-1", Column::Review { branch: None }),
            ticket("K-4", "EP-1", done(false)),
            ticket("K-5", "EP-2", done(false)),
            ticket("K-6", "EP-2", done(true)),
        ];
        derive::board_view(&board, &[])
    }

    fn ids(value: &serde_json::Value, key: &str) -> Vec<String> {
        value[key].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn the_default_board_read_omits_done_tickets_and_says_so() {
        let shaped = shape(view(), None, false);
        assert_eq!(ids(&shaped, "tickets"), ["K-1", "K-2", "K-3"], "the default shape carries todo/doing/review only");
        assert_eq!(shaped["done"]["count"], 3, "the summary counts every done ticket, discarded included");
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
        assert_eq!(shaped, serde_json::to_value(&view).unwrap(), "include_done is the byte-identical compatibility escape hatch");
        assert!(shaped.get("done").is_none(), "nothing was omitted, so there is nothing to summarize");
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
        assert!(discarded.contains(&serde_json::json!("K-6")), "a discarded ticket is still done and still reported");
        assert!(
            !landed.contains(&serde_json::json!("K-6")),
            "but never as landed — a discarded dependency does not unblock its dependents"
        );
    }

    #[test]
    fn the_version_is_the_boards_version_whatever_the_shape() {
        let shapes = [shape(view(), None, false), shape(view(), None, true), shape(view(), Some(ColumnId::Todo), false)];
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
        assert_eq!(epics, ["EP-1", "EP-2"], "epics are never filtered — a derived column of done is legitimate");
        assert_eq!(shaped["epics"][1]["column"], "done", "and EP-2 is exactly that case");
    }
}
