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
    store::{Store, derive, model::{EpicId, Status, TicketId}},
};

/// What Claude is called on the board when a tool doesn't say otherwise.
const DEFAULT_AGENT: &str = "claude";

/// The workflow contract, shipped to the client as server instructions.
const INSTRUCTIONS: &str = "A local Kanban board shared with a human (who sees it live in a browser). \
The lifecycle of a ready ticket: kanban_claim → kanban_worktree_start → work in the worktree, committing as you go → \
kanban_note progress → kanban_worktree_finish → kanban_move to done. \
Stubs are specs to write, not code to build: kanban_claim (the card sits pink in doing) → research → kanban_refine, \
which lands it back in todo at status=review for the human — no worktree. \
Only claim tickets kanban_next surfaces — ready (implement) or stub (refine), in todo, unblocked; never claim \
spontaneously outside an explicit work loop. Never touch draft tickets. Tickets you create default to status=review so \
the human vets them. Mutating tools need expected_version from your latest kanban_board read; on a version conflict, \
re-read and retry.";

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
    /// Restrict to one column: "todo", "doing", or "done". Omit for the whole board.
    #[serde(default)]
    pub column: Option<String>,
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
    /// Destination column: "todo", "doing", or "done".
    pub to: String,
    /// Position within the destination column, 0 = top. Omit for the bottom. Position IS priority.
    #[serde(default)]
    pub position: Option<usize>,
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
    /// Read the whole board: columns, tickets (with derived blocked/claim facts), epics (with derived columns), the
    /// current version — pass that version as `expected_version` to mutating tools — and `max_workers`, how many tickets
    /// a work loop may drive concurrently.
    #[tool]
    async fn kanban_board(&self, Parameters(p): Parameters<BoardParams>) -> Result<CallToolResult, ErrorData> {
        let column = match p.column.as_deref() {
            None => None,
            Some(c) => Some(c.parse::<crate::store::model::ColumnId>().map_err(|e| ErrorData::invalid_params(e, None))?),
        };
        self.read(move |store| {
            let max_workers = Config::load(store.dir())?.max_workers();
            let mut view = derive::board_view(&store.read_board()?, &store.read_claims()?);
            if let Some(col) = column {
                view.tickets.retain(|t| t.ticket.column.id() == col);
                view.epics.retain(|e| e.column == col);
            }
            let mut value = serde_json::to_value(&view).unwrap_or_default();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("max_workers".into(), max_workers.into());
            }
            Ok(value)
        })
        .await
    }

    /// The next thing to work on: the highest ticket in todo that is unblocked, unclaimed, non-external, and either
    /// ready (action "implement") or stub (action "refine" — write its spec, don't build it). Returns the full ticket
    /// plus the action, or explains that nothing is eligible.
    #[tool]
    async fn kanban_next(&self) -> Result<CallToolResult, ErrorData> {
        self.read(|store| {
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

    /// Move a ticket to a column at a position (0 = top; position is priority). Moving to done stamps `completed_at`,
    /// keeps the branch, and drops the live claim — that's the close-out step.
    #[tool]
    async fn kanban_move(&self, Parameters(p): Parameters<MoveParams>) -> Result<CallToolResult, ErrorData> {
        let to = p.to.parse().map_err(|e: String| ErrorData::invalid_params(e, None))?;
        let op = Op::MoveTicket { id: TicketId(p.ticket), to, position: p.position, owner: None };
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
    /// moving the ticket to done with `kanban_move` afterwards. Merging or pushing the branch is the human's call — never
    /// do it unasked.
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
            split_tickets: p.split_tickets.unwrap_or_default().into_iter().map(new_ticket_spec).collect(),
            split_epics: p.split_epics.unwrap_or_default().into_iter().map(new_epic_spec).collect(),
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

fn new_ticket_spec(p: NewTicketParam) -> NewTicketSpec {
    NewTicketSpec {
        title: p.title,
        body: p.body.unwrap_or_default(),
        labels: p.labels.unwrap_or_default(),
        depends_on: p.depends_on.unwrap_or_default(),
        epic: p.epic.map(EpicId),
    }
}

fn new_epic_spec(p: NewEpicParam) -> NewEpicSpec {
    NewEpicSpec {
        title: p.title,
        color: p.color,
        body: p.body.unwrap_or_default(),
        tickets: p.tickets.unwrap_or_default().into_iter().map(new_ticket_spec).collect(),
    }
}

#[tool_handler]
impl ServerHandler for KanbanServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(INSTRUCTIONS.into());
        info
    }
}
