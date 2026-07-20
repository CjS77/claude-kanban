//! The single write funnel: every board mutation, from every face of the binary, is one [`Op`] applied by [`apply`].
//!
//! Routing every write through typed operations is the point of the whole design — Claude editing the raw file with a text
//! edit is how a task tracker silently corrupts itself. HTTP handlers and MCP tools each parse their own inputs, build an
//! `Op`, and call [`apply`]; the worktree code stamps its results through here too. Nothing else writes the store.
//!
//! [`apply`] wraps [`Store::mutate`], so each op inherits the whole write discipline: advisory lock, fresh read, optimistic
//! version check, whole-board validation, atomic write. On top of that this module owns the *semantic* rules — what a move
//! does to a ticket's column data, what claiming requires, what refine creates.

use std::collections::HashSet;

use chrono::Utc;
use serde_json::{Value, json};

use crate::store::{
    Claim, Store, StoreError, find_claim,
    model::{Board, Column, ColumnId, Effort, Epic, EpicId, Note, Status, Ticket, TicketId},
    remove_claim, upsert_claim,
};

/// Default epic colours, assigned round-robin when `CreateEpic` gets no explicit colour. Muted, dark-and-light-friendly.
const EPIC_PALETTE: [&str; 8] = ["#7c9cf5", "#5cb8a7", "#d4a45c", "#c98bc9", "#e08787", "#8fb965", "#5eb3d6", "#a58fd6"];

/// Every mutation the board supports. One enum, one funnel.
#[derive(Debug, Clone)]
pub enum Op {
    /// Create a ticket at the bottom of `todo`.
    CreateTicket {
        title: String,
        body: String,
        epic: Option<EpicId>,
        labels: Vec<String>,
        depends_on: Vec<TicketId>,
        status: Status,
        model: Option<String>,
        effort: Option<Effort>,
    },
    /// Create an epic. `color: None` picks the next palette colour.
    CreateEpic { title: String, color: Option<String>, body: String, status: Status },
    /// Patch a ticket's descriptive fields. Column and status move via their own ops.
    UpdateTicket { id: TicketId, patch: TicketPatch },
    /// Patch an epic's descriptive fields.
    UpdateEpic { id: EpicId, patch: EpicPatch },
    /// Delete a ticket, cascade-removing it from other tickets' `depends_on` and dropping its claim.
    DeleteTicket { id: TicketId },
    /// Delete an epic and every ticket in it, cascade-removing those tickets from other tickets' `depends_on` and
    /// dropping their claims.
    DeleteEpic { id: EpicId },
    /// Move a ticket to a column at a position (`None` = bottom). `owner` is required to *enter* `doing` unless the ticket
    /// is already there; entering `review` or `done` drops any live claim; entering `done` stamps `completed_at`.
    /// `branch` overrides the branch recorded on the destination state — the close-out for a companion subtask worked on
    /// its parent ticket's branch, which otherwise reaches review branchless and unlandable.
    MoveTicket { id: TicketId, to: ColumnId, position: Option<usize>, owner: Option<String>, branch: Option<String> },
    SetTicketStatus { id: TicketId, status: Status },
    SetEpicStatus { id: EpicId, status: Status },
    /// Take a ticket: requires `ready`, unblocked, unclaimed, and not yet done. Moves it to `doing` owned by `agent` and
    /// records the live claim. A pure board mutation — git is untouched (that's `worktree start`).
    Claim { id: TicketId, agent: String },
    /// Give a ticket back: drops the claim and returns the card to the top of `todo` (it was priority work when claimed).
    Release { id: TicketId },
    /// Append to the ticket's progress log.
    AddNote { id: TicketId, text: String, author: Option<String> },
    /// Bind (or unbind, with `None`) a ticket to a work item in another system — the delegate skill records where a
    /// mirrored ticket went. The binding is an address for other tools; this binary never touches the network.
    BindExternal { id: TicketId, external: Option<crate::store::model::External> },
    /// Record a refinement pass (the thinking happened in the caller — this binary never talks to an LLM): replace the
    /// target's spec, optionally split off new tickets and epics, and land everything touched or created in `review`.
    /// All-or-nothing.
    Refine { target: RefineTarget, title: Option<String>, body: String, split_tickets: Vec<NewTicketSpec>, split_epics: Vec<NewEpicSpec> },
    /// Land a review ticket in `done` because its code has provably reached the local main branch. Constructed only by
    /// the landing sweep; re-checks its evidence under the lock (still in review, branch unchanged) and refuses —
    /// harmlessly — when the board moved underneath the slow git checks. Appends `reason` as a progress note so the
    /// human always sees why a card jumped.
    LandTicket { id: TicketId, expected_branch: Option<String>, reason: String },
    /// Retire a review ticket without landing it: `done` with `discarded: true`, which never satisfies dependencies.
    /// Always an explicit human action (the Discard button) — the sweep never constructs this.
    DiscardTicket { id: TicketId, reason: String },
    /// Record (or clear) the ticket's bound GitHub PR — the Create PR button on creation, the serve poller on discovery
    /// and on state transitions. Pure data recording; refuses nothing.
    SetPr { id: TicketId, pr: Option<crate::store::model::PrRef> },
    /// Stamp a started worktree's branch onto the ticket's `doing` state and its path onto the live claim.
    /// Constructed only by `worktree::start` — still funnelled through here so it obeys every rule.
    StampWorktree { id: TicketId, branch: String, path: std::path::PathBuf },
    /// Drop the worktree path from the live claim (the claim itself survives — the ticket is in flight until moved to
    /// `review`). Constructed only by `worktree::finish`.
    ClearWorktreePath { id: TicketId },
}

/// What `Refine` refines.
#[derive(Debug, Clone)]
pub enum RefineTarget {
    Ticket(TicketId),
    Epic(EpicId),
}

/// Descriptive fields of a ticket that `UpdateTicket` may change. `None` leaves a field alone; `epic` uses a nested option
/// so "detach from its epic" (`Some(None)`) and "don't touch" (`None`) are distinct.
#[derive(Debug, Clone, Default)]
pub struct TicketPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub labels: Option<Vec<String>>,
    pub depends_on: Option<Vec<TicketId>>,
    pub epic: Option<Option<EpicId>>,
    /// Nested like `epic`: `Some(None)` clears the preference, `None` leaves it alone.
    pub model: Option<Option<String>>,
    pub effort: Option<Option<Effort>>,
}

/// Descriptive fields of an epic that `UpdateEpic` may change.
#[derive(Debug, Clone, Default)]
pub struct EpicPatch {
    pub title: Option<String>,
    pub color: Option<String>,
    pub body: Option<String>,
}

/// A ticket created by `Refine`. `depends_on` entries may be existing ticket ids or `new:<i>` placeholders naming the i-th
/// entry of the refine's `split_tickets` (ids are minted in array order, so forward references work).
#[derive(Debug, Clone)]
pub struct NewTicketSpec {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub depends_on: Vec<String>,
    /// `None` inherits the refine target's epic (the target itself, when refining an epic).
    pub epic: Option<EpicId>,
    /// A split can mark the hard subtask: which model and effort its work deserves.
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

/// An epic created by `Refine`, with its own tickets — nested so a brand-new ticket can belong to a brand-new epic whose id
/// doesn't exist yet. Nested tickets cannot be named by `new:<i>` placeholders; only top-level `split_tickets` can.
#[derive(Debug, Clone)]
pub struct NewEpicSpec {
    pub title: String,
    pub color: Option<String>,
    pub body: String,
    pub tickets: Vec<NewTicketSpec>,
}

/// What a successful [`apply`] reports back.
#[derive(Debug, Clone)]
pub struct Applied {
    /// The board's new version — clients carry this into their next mutation.
    pub version: u64,
    /// Ids minted by `Create*` and `Refine`, in creation order.
    pub created_ids: Vec<String>,
    /// Op-specific payload, ready for an MCP tool result.
    pub result: Value,
}

/// Everything an op can refuse for. Version conflicts arrive wrapped from the store — see [`OpError::version_conflict`].
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{id} is already claimed by {agent}")]
    AlreadyClaimed { id: TicketId, agent: String },
    #[error("{0} is external — it is worked elsewhere and never gets a worktree here")]
    External(TicketId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl OpError {
    /// The `(expected, actual)` versions when this is an optimistic-concurrency conflict — the case HTTP maps to 409 and
    /// MCP maps to a "re-read and retry" tool error.
    #[must_use]
    pub fn version_conflict(&self) -> Option<(u64, u64)> {
        match self {
            OpError::Store(StoreError::VersionConflict { expected, actual }) => Some((*expected, *actual)),
            _ => None,
        }
    }
}

/// Apply one operation under the full write discipline. See the module docs; this is the only path that mutates the board,
/// so one log line here covers every mutation from both faces (UI and MCP).
pub fn apply(store: &Store, expected_version: Option<u64>, op: Op) -> Result<Applied, OpError> {
    let name = op.name();
    tracing::debug!(?op, "applying {name}");
    match store.mutate(expected_version, |board, claims| transition(op, board, claims)) {
        Ok((out, version)) => {
            tracing::info!(version, created = ?out.created_ids, "{name} applied");
            Ok(Applied { version, created_ids: out.created_ids, result: out.result })
        }
        Err(e) => {
            tracing::warn!(error = %e, "{name} refused");
            Err(e)
        }
    }
}

impl Op {
    /// The op's name for log lines: the discriminant without its payload.
    fn name(&self) -> &'static str {
        match self {
            Op::CreateTicket { .. } => "create_ticket",
            Op::CreateEpic { .. } => "create_epic",
            Op::UpdateTicket { .. } => "update_ticket",
            Op::UpdateEpic { .. } => "update_epic",
            Op::DeleteTicket { .. } => "delete_ticket",
            Op::DeleteEpic { .. } => "delete_epic",
            Op::MoveTicket { .. } => "move_ticket",
            Op::SetTicketStatus { .. } => "set_ticket_status",
            Op::SetEpicStatus { .. } => "set_epic_status",
            Op::Claim { .. } => "claim",
            Op::Release { .. } => "release",
            Op::AddNote { .. } => "add_note",
            Op::BindExternal { .. } => "bind_external",
            Op::Refine { .. } => "refine",
            Op::LandTicket { .. } => "land_ticket",
            Op::DiscardTicket { .. } => "discard_ticket",
            Op::SetPr { .. } => "set_pr",
            Op::StampWorktree { .. } => "stamp_worktree",
            Op::ClearWorktreePath { .. } => "clear_worktree_path",
        }
    }
}

/// A transition's output before the store stamps the new version on it.
struct OpOutput {
    created_ids: Vec<String>,
    result: Value,
}

impl OpOutput {
    fn created(ids: Vec<String>, result: Value) -> OpOutput {
        OpOutput { created_ids: ids, result }
    }

    fn plain(result: Value) -> OpOutput {
        OpOutput { created_ids: Vec::new(), result }
    }
}

/// Dispatch an op against fresh, locked state. Preconditions and column-data rules live in the per-op functions below;
/// whole-board invariants (dangling refs, cycles) are enforced by `Store::mutate` after this returns.
fn transition(op: Op, board: &mut Board, claims: &mut Vec<Claim>) -> Result<OpOutput, OpError> {
    match op {
        Op::CreateTicket { title, body, epic, labels, depends_on, status, model, effort } => {
            Ok(create_ticket(board, NewTicket { title, body, epic, labels, depends_on, status, model, effort }))
        }
        Op::CreateEpic { title, color, body, status } => Ok(create_epic(board, title, color, body, status)),
        Op::UpdateTicket { id, patch } => update_ticket(board, &id, patch),
        Op::UpdateEpic { id, patch } => update_epic(board, &id, patch),
        Op::DeleteTicket { id } => delete_ticket(board, claims, &id),
        Op::DeleteEpic { id } => delete_epic(board, claims, &id),
        Op::MoveTicket { id, to, position, owner, branch } => move_ticket(board, claims, &id, to, position, owner, branch),
        Op::SetTicketStatus { id, status } => set_ticket_status(board, &id, status),
        Op::SetEpicStatus { id, status } => set_epic_status(board, &id, status),
        Op::Claim { id, agent } => claim(board, claims, &id, &agent),
        Op::Release { id } => release(board, claims, &id),
        Op::AddNote { id, text, author } => add_note(board, &id, text, author),
        Op::BindExternal { id, external } => bind_external(board, &id, external),
        Op::Refine { target, title, body, split_tickets, split_epics } => refine(board, claims, &target, title, body, split_tickets, split_epics),
        Op::LandTicket { id, expected_branch, reason } => land_ticket(board, claims, &id, expected_branch.as_deref(), &reason),
        Op::DiscardTicket { id, reason } => discard_ticket(board, claims, &id, &reason),
        Op::SetPr { id, pr } => set_pr(board, &id, pr),
        Op::StampWorktree { id, branch, path } => stamp_worktree(board, claims, &id, &branch, &path),
        Op::ClearWorktreePath { id } => Ok(clear_worktree_path(claims, &id)),
    }
}

fn ticket_mut<'a>(board: &'a mut Board, id: &TicketId) -> Result<&'a mut Ticket, OpError> {
    board.ticket_mut(id).ok_or_else(|| OpError::NotFound(id.to_string()))
}

fn epic_mut<'a>(board: &'a mut Board, id: &EpicId) -> Result<&'a mut Epic, OpError> {
    board.epic_mut(id).ok_or_else(|| OpError::NotFound(id.to_string()))
}

/// `Op::CreateTicket`'s payload, bundled so the helper takes one argument instead of eight.
struct NewTicket {
    title: String,
    body: String,
    epic: Option<EpicId>,
    labels: Vec<String>,
    depends_on: Vec<TicketId>,
    status: Status,
    model: Option<String>,
    effort: Option<Effort>,
}

fn create_ticket(board: &mut Board, new: NewTicket) -> OpOutput {
    let NewTicket { title, body, epic, labels, depends_on, status, model, effort } = new;
    let id = board.mint_ticket_id();
    board.tickets.push(Ticket {
        id: id.clone(),
        title,
        epic,
        status,
        body,
        labels,
        model,
        effort,
        depends_on,
        notes: vec![],
        external: None,
        pr: None,
        column: Column::Todo,
    });
    OpOutput::created(vec![id.to_string()], json!({ "id": id }))
}

fn create_epic(board: &mut Board, title: String, color: Option<String>, body: String, status: Status) -> OpOutput {
    let id = board.mint_epic_id();
    let color = color.unwrap_or_else(|| EPIC_PALETTE[board.epics.len() % EPIC_PALETTE.len()].to_owned());
    board.epics.push(Epic { id: id.clone(), title, color, status, body });
    OpOutput::created(vec![id.to_string()], json!({ "id": id }))
}

fn update_ticket(board: &mut Board, id: &TicketId, patch: TicketPatch) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    let TicketPatch { title, body, labels, depends_on, epic, model, effort } = patch;
    if let Some(title) = title {
        ticket.title = title;
    }
    if let Some(body) = body {
        ticket.body = body;
    }
    if let Some(labels) = labels {
        ticket.labels = labels;
    }
    if let Some(depends_on) = depends_on {
        ticket.depends_on = depends_on;
    }
    if let Some(epic) = epic {
        ticket.epic = epic;
    }
    if let Some(model) = model {
        ticket.model = model;
    }
    if let Some(effort) = effort {
        ticket.effort = effort;
    }
    Ok(OpOutput::plain(json!({ "id": id })))
}

fn update_epic(board: &mut Board, id: &EpicId, patch: EpicPatch) -> Result<OpOutput, OpError> {
    let epic = epic_mut(board, id)?;
    let EpicPatch { title, color, body } = patch;
    if let Some(title) = title {
        epic.title = title;
    }
    if let Some(color) = color {
        epic.color = color;
    }
    if let Some(body) = body {
        epic.body = body;
    }
    Ok(OpOutput::plain(json!({ "id": id })))
}

/// Remove `doomed` tickets, cascade-removing them from surviving tickets' `depends_on` and dropping their claims.
///
/// Note that the ids themselves are *not* retired: `Board::next_ticket_id` counts from the highest surviving id, so
/// deleting the tail of the board hands the next ticket an id a deleted one already wore. That's a separate concern.
fn remove_tickets(board: &mut Board, claims: &mut Vec<Claim>, doomed: &HashSet<TicketId>) {
    board.tickets.retain(|t| !doomed.contains(&t.id));
    board.tickets.iter_mut().for_each(|t| t.depends_on.retain(|dep| !doomed.contains(dep)));
    claims.retain(|c| !doomed.contains(&c.ticket));
}

fn delete_ticket(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId) -> Result<OpOutput, OpError> {
    ticket_mut(board, id)?;
    remove_tickets(board, claims, &HashSet::from([id.clone()]));
    Ok(OpOutput::plain(json!({ "deleted": id })))
}

/// Deleting an epic takes its tickets with it — done ones included. The confirm dialog on the board is the only safety
/// net, so the returned payload names every ticket that went, for the log line and any future caller.
fn delete_epic(board: &mut Board, claims: &mut Vec<Claim>, id: &EpicId) -> Result<OpOutput, OpError> {
    epic_mut(board, id)?;
    // Collected in board order so the payload reads the way the epic's checklist did.
    let doomed: Vec<TicketId> = board.tickets.iter().filter(|t| t.epic.as_ref() == Some(id)).map(|t| t.id.clone()).collect();
    remove_tickets(board, claims, &doomed.iter().cloned().collect());
    board.epics.retain(|e| &e.id != id);
    Ok(OpOutput::plain(json!({ "deleted": id, "tickets": doomed })))
}

fn move_ticket(
    board: &mut Board,
    claims: &mut Vec<Claim>,
    id: &TicketId,
    to: ColumnId,
    position: Option<usize>,
    owner: Option<String>,
    branch: Option<String>,
) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    ticket.column = next_column_state(&ticket.column, to, owner, branch, id)?;
    if matches!(to, ColumnId::Review | ColumnId::Done) {
        remove_claim(claims, id);
    }
    reposition(board, id, to, position);
    Ok(OpOutput::plain(json!({ "id": id, "column": to })))
}

/// The column-data rules for a move. Entering a state fills exactly that state's fields:
/// - `todo` carries nothing — owner and branch are dropped.
/// - `doing` needs an owner: the one supplied, or the existing one when the ticket is already `doing`. The branch survives.
/// - `review` carries the branch — code-complete, waiting to land. Nobody owns review work.
/// - `done` stamps `completed_at` now and carries the branch over. Moves land as kept (`discarded: false`); retiring
///   work without landing it is the explicit discard operation, never a plain move.
///
/// `branch_override` beats the branch carried on the current state — a companion subtask worked on its parent's branch
/// records that shared branch as it moves, having never had a worktree of its own.
fn next_column_state(
    current: &Column,
    to: ColumnId,
    owner: Option<String>,
    branch_override: Option<String>,
    id: &TicketId,
) -> Result<Column, OpError> {
    let branch = branch_override.or_else(|| current.branch().map(str::to_owned));
    match to {
        ColumnId::Todo => Ok(Column::Todo),
        ColumnId::Doing => {
            let existing = match current {
                Column::Doing { owner, .. } => Some(owner.clone()),
                _ => None,
            };
            let owner = owner
                .or(existing)
                .ok_or_else(|| OpError::Invalid(format!("moving {id} into doing needs an owner — claim it (kanban_claim), or supply one")))?;
            Ok(Column::Doing { owner, branch })
        }
        ColumnId::Review => Ok(Column::Review { branch }),
        ColumnId::Done => match current {
            Column::Done { .. } if branch.as_deref() == current.branch() => Ok(current.clone()),
            Column::Done { completed_at, discarded, .. } => Ok(Column::Done { branch, completed_at: *completed_at, discarded: *discarded }),
            _ => Ok(Column::Done { branch, completed_at: Utc::now(), discarded: false }),
        },
    }
}

/// The landing sweep's move: review → done, with the evidence re-checked under the lock. The sweep's git checks run
/// *outside* the lock (they are slow), so by the time this applies, the human may have dragged the card elsewhere or a
/// rework may have re-stamped the branch — in either case the evidence no longer describes the ticket and the landing
/// refuses, harmlessly: the next sweep re-derives everything from fresh state.
fn land_ticket(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId, expected_branch: Option<&str>, reason: &str) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    let Column::Review { branch } = &ticket.column else {
        return Err(OpError::Invalid(format!("{id} is no longer in review — not landing it")));
    };
    if branch.as_deref() != expected_branch {
        return Err(OpError::Invalid(format!("{id}'s branch changed since the evidence was gathered — not landing it")));
    }
    ticket.column = Column::Done { branch: branch.clone(), completed_at: Utc::now(), discarded: false };
    ticket.notes.push(Note { at: Utc::now(), author: Some("kanban".into()), text: reason.to_owned() });
    remove_claim(claims, id); // review tickets are unclaimed by construction, but never leave a ghost behind
    reposition(board, id, ColumnId::Done, None);
    Ok(OpOutput::plain(json!({ "id": id, "column": ColumnId::Done, "landed_from": "review" })))
}

/// The explicit human retirement: review → done with `discarded: true`. Dependents stay blocked — that is the point.
fn discard_ticket(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId, reason: &str) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    let Column::Review { branch } = &ticket.column else {
        return Err(OpError::Invalid(format!("{id} is not in review — only code-complete, unlanded work can be discarded")));
    };
    ticket.column = Column::Done { branch: branch.clone(), completed_at: Utc::now(), discarded: true };
    ticket.notes.push(Note { at: Utc::now(), author: Some("kanban".into()), text: reason.to_owned() });
    remove_claim(claims, id);
    reposition(board, id, ColumnId::Done, None);
    Ok(OpOutput::plain(json!({ "id": id, "column": ColumnId::Done, "discarded": true })))
}

fn set_pr(board: &mut Board, id: &TicketId, pr: Option<crate::store::model::PrRef>) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    ticket.pr = pr;
    Ok(OpOutput::plain(json!({ "id": id, "pr": ticket.pr })))
}

/// Put `id` at `position` among the tickets of `column` (`None` or past-the-end = bottom), preserving everyone else's
/// order. Positions count the column's members only — the array interleaves columns freely.
fn reposition(board: &mut Board, id: &TicketId, column: ColumnId, position: Option<usize>) {
    let index = board.tickets.iter().position(|t| &t.id == id).expect("caller verified the ticket exists");
    let ticket = board.tickets.remove(index);
    let members: Vec<usize> =
        board.tickets.iter().enumerate().filter(|(_, t)| t.column.id() == column).map(|(i, _)| i).collect();
    let end = members.last().map_or(board.tickets.len(), |&i| i + 1);
    let global = position.map_or(end, |p| members.get(p).copied().unwrap_or(end));
    board.tickets.insert(global, ticket);
}

fn set_ticket_status(board: &mut Board, id: &TicketId, status: Status) -> Result<OpOutput, OpError> {
    ticket_mut(board, id)?.status = status;
    Ok(OpOutput::plain(json!({ "id": id, "status": status })))
}

fn set_epic_status(board: &mut Board, id: &EpicId, status: Status) -> Result<OpOutput, OpError> {
    epic_mut(board, id)?.status = status;
    Ok(OpOutput::plain(json!({ "id": id, "status": status })))
}

/// Claiming enforces the handoff contract: unblocked, unclaimed, not already done, and either `ready` (to implement) or
/// `stub` (to refine — the card sits in `doing`, tinted pink, while the spec is written; `refine` returns it to `todo`).
/// (External tickets *are* claimable — delegating claims them on the delegate's behalf; they just never get a worktree.)
/// A ticket in `review` is claimable too: that is the rework path (PR feedback) — the claim keeps the recorded branch,
/// and `worktree start` re-attaches to it.
fn claim(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId, agent: &str) -> Result<OpOutput, OpError> {
    if let Some(existing) = find_claim(claims, id) {
        return Err(OpError::AlreadyClaimed { id: id.clone(), agent: existing.agent.clone() });
    }
    let blocked = crate::store::derive::blocked(board.ticket(id).ok_or_else(|| OpError::NotFound(id.to_string()))?, board);
    let ticket = ticket_mut(board, id)?;
    match &ticket.column {
        Column::Done { .. } => return Err(OpError::Invalid(format!("{id} is already done — move it back to todo first"))),
        Column::Doing { owner, .. } if owner != agent => {
            return Err(OpError::AlreadyClaimed { id: id.clone(), agent: owner.clone() });
        }
        Column::Todo | Column::Doing { .. } | Column::Review { .. } => {}
    }
    if !matches!(ticket.status, Status::Ready | Status::Stub) {
        return Err(OpError::Invalid(format!("{id} is {} — only ready tickets (to implement) or stubs (to refine) can be claimed", ticket.status)));
    }
    if blocked {
        let deps = ticket.depends_on.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        return Err(OpError::Invalid(format!("{id} is blocked — its dependencies ({deps}) are not all done")));
    }
    ticket.column = Column::Doing { owner: agent.to_owned(), branch: ticket.column.branch().map(str::to_owned) };
    let refining = ticket.status == Status::Stub;
    upsert_claim(claims, Claim { ticket: id.clone(), agent: agent.to_owned(), since: Utc::now(), path: None });
    Ok(OpOutput::plain(json!({ "id": id, "owner": agent, "refining": refining })))
}

/// Releasing undoes a claim: the live claim is dropped and the card returns to the *top* of `todo` — it was priority work
/// when it was claimed, so it goes back as priority work.
fn release(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    let had_claim = remove_claim(claims, id).is_some();
    if !had_claim && !matches!(ticket.column, Column::Doing { .. }) {
        return Err(OpError::Invalid(format!("{id} is not claimed")));
    }
    ticket.column = Column::Todo;
    reposition(board, id, ColumnId::Todo, Some(0));
    Ok(OpOutput::plain(json!({ "id": id, "column": ColumnId::Todo })))
}

fn add_note(board: &mut Board, id: &TicketId, text: String, author: Option<String>) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    ticket.notes.push(Note { at: Utc::now(), author, text });
    Ok(OpOutput::plain(json!({ "id": id, "notes": ticket.notes.len() })))
}

fn bind_external(board: &mut Board, id: &TicketId, external: Option<crate::store::model::External>) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    ticket.external = external;
    Ok(OpOutput::plain(json!({ "id": id, "external": ticket.external })))
}

/// Record a refinement (see [`Op::Refine`]). Creation order — and thus id order — is: each split epic, then that epic's
/// tickets, then the top-level split tickets. `new:<i>` placeholders resolve against the top-level `split_tickets` only.
fn refine(
    board: &mut Board,
    claims: &mut Vec<Claim>,
    target: &RefineTarget,
    title: Option<String>,
    body: String,
    split_tickets: Vec<NewTicketSpec>,
    split_epics: Vec<NewEpicSpec>,
) -> Result<OpOutput, OpError> {
    let inherited_epic = retitle_target(board, target, title, body)?;

    // A stub claimed for refinement sat in `doing` while its spec was written — hand it back for the human's verdict:
    // the claim drops and the card returns to the top of `todo`, mirroring `release`.
    if let RefineTarget::Ticket(id) = target {
        remove_claim(claims, id);
        if matches!(ticket_mut(board, id)?.column, Column::Doing { .. }) {
            ticket_mut(board, id)?.column = Column::Todo;
            reposition(board, id, ColumnId::Todo, Some(0));
        }
    }

    let mut created: Vec<String> = Vec::new();

    // Split epics first: their tickets belong to them, and their ids must exist before the tickets reference them.
    let nested: Vec<(NewTicketSpec, EpicId)> = split_epics
        .into_iter()
        .flat_map(|spec| {
            let epic_id = board.mint_epic_id();
            let color = spec.color.unwrap_or_else(|| EPIC_PALETTE[board.epics.len() % EPIC_PALETTE.len()].to_owned());
            board.epics.push(Epic { id: epic_id.clone(), title: spec.title, color, status: Status::Review, body: spec.body });
            created.push(epic_id.to_string());
            spec.tickets.into_iter().map(move |t| (t, epic_id.clone())).collect::<Vec<_>>()
        })
        .collect();

    // Mint every new ticket id up front so `new:<i>` placeholders (which index split_tickets) can point forward.
    let nested_ids: Vec<TicketId> = board.mint_ticket_ids(nested.len());
    let split_ids: Vec<TicketId> = board.mint_ticket_ids(split_tickets.len());

    let nested_tickets = nested
        .into_iter()
        .zip(&nested_ids)
        .map(|((spec, epic_id), id)| build_refined_ticket(spec, id.clone(), Some(epic_id), &split_ids))
        .collect::<Result<Vec<_>, _>>()?;
    let top_tickets = split_tickets
        .into_iter()
        .zip(&split_ids)
        .map(|(spec, id)| build_refined_ticket(spec, id.clone(), inherited_epic.clone(), &split_ids))
        .collect::<Result<Vec<_>, _>>()?;

    created.extend(nested_ids.iter().chain(&split_ids).map(ToString::to_string));
    board.tickets.extend(nested_tickets);
    board.tickets.extend(top_tickets);
    let target_id = match target {
        RefineTarget::Ticket(id) => id.to_string(),
        RefineTarget::Epic(id) => id.to_string(),
    };
    Ok(OpOutput::created(created.clone(), json!({ "refined": target_id, "created": created, "status": Status::Review })))
}

/// Apply the refined spec to the target and land it in `review`. Returns the epic that top-level split tickets inherit:
/// the target's own epic for a ticket, the target itself for an epic.
fn retitle_target(board: &mut Board, target: &RefineTarget, title: Option<String>, body: String) -> Result<Option<EpicId>, OpError> {
    let (status, id) = match target {
        RefineTarget::Ticket(id) => {
            let t = ticket_mut(board, id)?;
            (t.status, id.to_string())
        }
        RefineTarget::Epic(id) => {
            let e = epic_mut(board, id)?;
            (e.status, id.to_string())
        }
    };
    if !matches!(status, Status::Stub | Status::Review) {
        return Err(OpError::Invalid(format!("{id} is {status} — only stub (or re-refined review) items can be refined")));
    }
    match target {
        RefineTarget::Ticket(id) => {
            let t = ticket_mut(board, id)?;
            if let Some(title) = title {
                t.title = title;
            }
            t.body = body;
            t.status = Status::Review;
            Ok(t.epic.clone())
        }
        RefineTarget::Epic(id) => {
            let e = epic_mut(board, id)?;
            if let Some(title) = title {
                e.title = title;
            }
            e.body = body;
            e.status = Status::Review;
            Ok(Some(id.clone()))
        }
    }
}

/// Turn a [`NewTicketSpec`] into a real ticket in `todo` with status `review`, resolving `new:<i>` dependency placeholders
/// against the minted `split_ids`.
fn build_refined_ticket(spec: NewTicketSpec, id: TicketId, default_epic: Option<EpicId>, split_ids: &[TicketId]) -> Result<Ticket, OpError> {
    let depends_on = spec
        .depends_on
        .iter()
        .map(|dep| match dep.strip_prefix("new:") {
            None => Ok(TicketId(dep.clone())),
            Some(index) => index
                .parse::<usize>()
                .ok()
                .and_then(|i| split_ids.get(i).cloned())
                .ok_or_else(|| OpError::Invalid(format!("dependency placeholder '{dep}' does not name a split ticket"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Ticket {
        id,
        title: spec.title,
        epic: spec.epic.or(default_epic),
        status: Status::Review,
        body: spec.body,
        labels: spec.labels,
        model: spec.model,
        effort: spec.effort,
        depends_on,
        notes: vec![],
        external: None,
        pr: None,
        column: Column::Todo,
    })
}

/// Stamp `worktree start`'s results: the branch onto the ticket's `doing` state, the path onto the live claim. Refuses
/// external tickets (they are worked elsewhere) and tickets not in `doing` (claim first — the lifecycle is explicit).
fn stamp_worktree(board: &mut Board, claims: &mut Vec<Claim>, id: &TicketId, branch: &str, path: &std::path::Path) -> Result<OpOutput, OpError> {
    let ticket = ticket_mut(board, id)?;
    if ticket.external.is_some() {
        return Err(OpError::External(id.clone()));
    }
    let owner = match &ticket.column {
        Column::Doing { owner, .. } => owner.clone(),
        _ => return Err(OpError::Invalid(format!("{id} is not in doing — claim it first (kanban_claim)"))),
    };
    ticket.column = Column::Doing { owner: owner.clone(), branch: Some(branch.to_owned()) };
    // Upsert defensively: the claim normally exists (Claim created it), but a hand-cleared sidecar shouldn't wedge `start`.
    let since = find_claim(claims, id).map_or_else(Utc::now, |c| c.since);
    upsert_claim(claims, Claim { ticket: id.clone(), agent: owner, since, path: Some(path.to_owned()) });
    Ok(OpOutput::plain(json!({ "id": id, "branch": branch, "path": path })))
}

/// Drop the path from the ticket's live claim after `worktree finish`. Idempotent: a missing claim is fine.
fn clear_worktree_path(claims: &mut [Claim], id: &TicketId) -> OpOutput {
    claims.iter_mut().filter(|c| &c.ticket == id).for_each(|c| c.path = None);
    OpOutput::plain(json!({ "id": id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::derive;

    fn scratch() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join(".kanban"));
        store.init().unwrap();
        (dir, store)
    }

    fn create(store: &Store, title: &str) -> TicketId {
        let applied = apply(
            store,
            None,
            Op::CreateTicket { title: title.into(), body: String::new(), epic: None, labels: vec![], depends_on: vec![], status: Status::Ready, model: None, effort: None },
        )
        .unwrap();
        TicketId(applied.created_ids[0].clone())
    }

    /// The nested option, exercised on both new fields: `None` leaves a preference alone, `Some(None)` clears it. Getting
    /// this backwards would make every ordinary edit-form save wipe the ticket's model.
    #[test]
    fn patching_model_and_effort_distinguishes_untouched_from_cleared() {
        let (_dir, store) = scratch();
        let id = create(&store, "hard one");
        let patch = |p| apply(&store, None, Op::UpdateTicket { id: id.clone(), patch: p }).unwrap();
        let read = || {
            let b = store.read_board().unwrap();
            let t = b.ticket(&id).unwrap();
            (t.model.clone(), t.effort)
        };

        patch(TicketPatch { model: Some(Some("opus".into())), effort: Some(Some(Effort::Xhigh)), ..TicketPatch::default() });
        assert_eq!(read(), (Some("opus".into()), Some(Effort::Xhigh)));

        patch(TicketPatch { title: Some("retitled".into()), ..TicketPatch::default() });
        assert_eq!(read(), (Some("opus".into()), Some(Effort::Xhigh)), "a patch that doesn't mention them leaves them alone");

        patch(TicketPatch { model: Some(None), ..TicketPatch::default() });
        assert_eq!(read(), (None, Some(Effort::Xhigh)), "clearing one leaves the other standing");

        patch(TicketPatch { effort: Some(None), ..TicketPatch::default() });
        assert_eq!(read(), (None, None));
    }

    #[test]
    fn create_claim_move_to_done_is_the_happy_path() {
        let (_dir, store) = scratch();
        let id = create(&store, "do the thing");
        assert_eq!(id.0, "K-1");

        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        let board = store.read_board().unwrap();
        assert!(matches!(&board.ticket(&id).unwrap().column, Column::Doing { owner, .. } if owner == "claude"));
        assert_eq!(store.read_claims().unwrap().len(), 1);

        apply(&store, None, Op::MoveTicket { id: id.clone(), to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();
        let board = store.read_board().unwrap();
        assert!(matches!(board.ticket(&id).unwrap().column, Column::Done { .. }));
        assert!(store.read_claims().unwrap().is_empty(), "reaching done drops the live claim");
    }

    #[test]
    fn refining_a_claimed_stub_returns_it_to_todo_and_drops_the_claim() {
        let (_dir, store) = scratch();
        let filler = create(&store, "filler at the top of todo");
        let id = create(&store, "vague idea");
        apply(&store, None, Op::SetTicketStatus { id: id.clone(), status: Status::Stub }).unwrap();

        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        let board = store.read_board().unwrap();
        assert!(matches!(&board.ticket(&id).unwrap().column, Column::Doing { owner, .. } if owner == "claude"), "a claimed stub sits in doing while its spec is written");
        assert_eq!(store.read_claims().unwrap().len(), 1);

        let op = Op::Refine { target: RefineTarget::Ticket(id.clone()), title: None, body: "now specific".into(), split_tickets: vec![], split_epics: vec![] };
        apply(&store, None, op).unwrap();
        let board = store.read_board().unwrap();
        let refined = board.ticket(&id).unwrap();
        assert_eq!(refined.status, Status::Review);
        assert_eq!(refined.body, "now specific");
        assert_eq!(board.tickets_in(ColumnId::Todo).next().unwrap().id, id, "the refined stub returns to the TOP of todo, above {filler}");
        assert!(store.read_claims().unwrap().is_empty(), "refine drops the live claim");
    }

    #[test]
    fn claiming_enforces_the_handoff_contract() {
        let (_dir, store) = scratch();
        let a = create(&store, "first");
        let b = create(&store, "second");

        // Not ready (and not a stub — those are claimable for refinement).
        apply(&store, None, Op::SetTicketStatus { id: a.clone(), status: Status::Draft }).unwrap();
        let err = apply(&store, None, Op::Claim { id: a.clone(), agent: "claude".into() }).unwrap_err();
        assert!(err.to_string().contains("only ready"), "{err}");
        apply(&store, None, Op::SetTicketStatus { id: a.clone(), status: Status::Ready }).unwrap();

        // Blocked.
        apply(&store, None, Op::UpdateTicket { id: b.clone(), patch: TicketPatch { depends_on: Some(vec![a.clone()]), ..TicketPatch::default() } }).unwrap();
        let err = apply(&store, None, Op::Claim { id: b.clone(), agent: "claude".into() }).unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");

        // Already claimed.
        apply(&store, None, Op::Claim { id: a.clone(), agent: "claude".into() }).unwrap();
        let err = apply(&store, None, Op::Claim { id: a.clone(), agent: "other".into() }).unwrap_err();
        assert!(matches!(err, OpError::AlreadyClaimed { agent, .. } if agent == "claude"));

        // Done.
        apply(&store, None, Op::MoveTicket { id: a.clone(), to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();
        let err = apply(&store, None, Op::Claim { id: a, agent: "claude".into() }).unwrap_err();
        assert!(err.to_string().contains("already done"), "{err}");
    }

    #[test]
    fn release_returns_the_card_to_the_top_of_todo() {
        let (_dir, store) = scratch();
        let a = create(&store, "a");
        let b = create(&store, "b");
        apply(&store, None, Op::Claim { id: b.clone(), agent: "claude".into() }).unwrap();
        apply(&store, None, Op::Release { id: b.clone() }).unwrap();

        let board = store.read_board().unwrap();
        let todo: Vec<&str> = board.tickets_in(ColumnId::Todo).map(|t| t.id.0.as_str()).collect();
        assert_eq!(todo, vec!["K-2", "K-1"], "released ticket outranks {a}");
        assert!(store.read_claims().unwrap().is_empty());
        assert!(apply(&store, None, Op::Release { id: b }).is_err(), "releasing an unclaimed ticket refuses");
    }

    #[test]
    fn moving_between_and_within_columns_lands_at_the_requested_position() {
        let (_dir, store) = scratch();
        let ids: Vec<TicketId> = ["a", "b", "c"].iter().map(|t| create(&store, t)).collect();

        // Within a column: bottom → top.
        apply(&store, None, Op::MoveTicket { id: ids[2].clone(), to: ColumnId::Todo, position: Some(0), owner: None, branch: None }).unwrap();
        let board = store.read_board().unwrap();
        let todo: Vec<&str> = board.tickets_in(ColumnId::Todo).map(|t| t.id.0.as_str()).collect();
        assert_eq!(todo, vec!["K-3", "K-1", "K-2"]);

        // Across columns with an owner; position past the end clamps to bottom.
        apply(&store, None, Op::MoveTicket { id: ids[0].clone(), to: ColumnId::Doing, position: Some(99), owner: Some("user".into()), branch: None }).unwrap();
        let board = store.read_board().unwrap();
        assert!(matches!(&board.ticket(&ids[0]).unwrap().column, Column::Doing { owner, .. } if owner == "user"));

        // Entering doing without any owner refuses.
        let err = apply(&store, None, Op::MoveTicket { id: ids[1].clone(), to: ColumnId::Doing, position: None, owner: None, branch: None }).unwrap_err();
        assert!(err.to_string().contains("needs an owner"), "{err}");

        // Back to todo drops the owner; the branch is dropped too (todo carries nothing).
        apply(&store, None, Op::MoveTicket { id: ids[0].clone(), to: ColumnId::Todo, position: None, owner: None, branch: None }).unwrap();
        assert!(matches!(store.read_board().unwrap().ticket(&ids[0]).unwrap().column, Column::Todo));
    }

    #[test]
    fn moving_to_done_and_back_keeps_branch_semantics() {
        let (_dir, store) = scratch();
        let id = create(&store, "with branch");
        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "k-1/with-branch".into(), path: "/tmp/wt".into() }).unwrap();
        apply(&store, None, Op::MoveTicket { id: id.clone(), to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();

        let board = store.read_board().unwrap();
        match &board.ticket(&id).unwrap().column {
            Column::Done { branch, .. } => assert_eq!(branch.as_deref(), Some("k-1/with-branch"), "done keeps the branch"),
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[test]
    fn version_conflicts_surface_through_apply() {
        let (_dir, store) = scratch();
        create(&store, "a");
        let err = apply(&store, Some(0), Op::SetTicketStatus { id: TicketId("K-1".into()), status: Status::Draft }).unwrap_err();
        assert_eq!(err.version_conflict(), Some((0, 1)));
    }

    #[test]
    fn deleting_a_ticket_cascades_out_of_depends_on_and_claims() {
        let (_dir, store) = scratch();
        let a = create(&store, "a");
        let b = create(&store, "b");
        apply(&store, None, Op::UpdateTicket { id: b.clone(), patch: TicketPatch { depends_on: Some(vec![a.clone()]), ..TicketPatch::default() } }).unwrap();
        apply(&store, None, Op::Claim { id: a.clone(), agent: "claude".into() }).unwrap();
        apply(&store, None, Op::DeleteTicket { id: a }).unwrap();

        let board = store.read_board().unwrap();
        assert!(board.ticket(&b).unwrap().depends_on.is_empty(), "the dangling dependency is removed, not left to fail validation");
        assert!(store.read_claims().unwrap().is_empty());
    }

    /// Create an epic and return its id — the three cascade tests below all start here.
    fn create_epic_id(store: &Store, title: &str) -> EpicId {
        let applied =
            apply(store, None, Op::CreateEpic { title: title.into(), color: None, body: String::new(), status: Status::Ready }).unwrap();
        EpicId(applied.created_ids[0].clone())
    }

    fn put_in_epic(store: &Store, ticket: &TicketId, epic: &EpicId) {
        let patch = TicketPatch { epic: Some(Some(epic.clone())), ..TicketPatch::default() };
        apply(store, None, Op::UpdateTicket { id: ticket.clone(), patch }).unwrap();
    }

    #[test]
    fn deleting_an_epic_deletes_its_tickets() {
        let (_dir, store) = scratch();
        let epic_id = create_epic_id(&store, "auth");
        let inside = create(&store, "in epic");
        let outside = create(&store, "no epic");
        put_in_epic(&store, &inside, &epic_id);
        apply(&store, None, Op::DeleteEpic { id: epic_id }).unwrap();

        let board = store.read_board().unwrap();
        assert!(board.epics.is_empty());
        assert!(board.ticket(&inside).is_none(), "the epic's tickets go with it");
        assert!(board.ticket(&outside).is_some(), "a ticket in no epic is untouched");
    }

    /// Cascade takes completed history and live work alike: a done ticket is deleted, not spared, and a claimed one is
    /// deleted with its claim dropped (its worktree and branch survive on disk — git is not this op's business).
    #[test]
    fn deleting_an_epic_takes_done_and_claimed_tickets_with_it() {
        let (_dir, store) = scratch();
        let epic_id = create_epic_id(&store, "auth");
        let finished = create(&store, "already done");
        let working = create(&store, "under way");
        put_in_epic(&store, &finished, &epic_id);
        put_in_epic(&store, &working, &epic_id);

        apply(&store, None, Op::MoveTicket { id: finished.clone(), to: ColumnId::Done, position: None, owner: None, branch: None }).unwrap();
        apply(&store, None, Op::Claim { id: working.clone(), agent: "claude".into() }).unwrap();
        apply(&store, None, Op::StampWorktree { id: working.clone(), branch: "k-2/under-way".into(), path: "/tmp/wt".into() }).unwrap();
        assert_eq!(store.read_claims().unwrap().len(), 1, "the claimed ticket really is claimed before the delete");

        apply(&store, None, Op::DeleteEpic { id: epic_id }).unwrap();
        let board = store.read_board().unwrap();
        assert!(board.tickets.is_empty(), "neither the done ticket nor the claimed one survives: {:?}", board.tickets);
        assert!(store.read_claims().unwrap().is_empty(), "the claim on the deleted ticket is dropped");
    }

    /// A dangling `depends_on` fails whole-board validation, so the cascade has to clean the survivors in the same
    /// transition — an `Ok` here is the proof that it wrote at all.
    #[test]
    fn deleting_an_epic_clears_dependencies_on_its_tickets() {
        let (_dir, store) = scratch();
        let epic_id = create_epic_id(&store, "auth");
        let inside = create(&store, "in epic");
        let outside = create(&store, "depends on it");
        put_in_epic(&store, &inside, &epic_id);
        let patch = TicketPatch { depends_on: Some(vec![inside]), ..TicketPatch::default() };
        apply(&store, None, Op::UpdateTicket { id: outside.clone(), patch }).unwrap();

        apply(&store, None, Op::DeleteEpic { id: epic_id }).expect("the cascade must leave the board valid");
        let board = store.read_board().unwrap();
        assert!(board.ticket(&outside).unwrap().depends_on.is_empty(), "the survivor is silently unblocked, not left dangling");
    }

    #[test]
    fn an_update_introducing_a_cycle_is_rejected_atomically() {
        let (_dir, store) = scratch();
        let a = create(&store, "a");
        let b = create(&store, "b");
        apply(&store, None, Op::UpdateTicket { id: b.clone(), patch: TicketPatch { depends_on: Some(vec![a.clone()]), ..TicketPatch::default() } }).unwrap();
        let before = std::fs::read(store.board_path()).unwrap();
        let err =
            apply(&store, None, Op::UpdateTicket { id: a, patch: TicketPatch { depends_on: Some(vec![b]), ..TicketPatch::default() } }).unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
        assert_eq!(std::fs::read(store.board_path()).unwrap(), before, "a rejected op leaves the file untouched");
    }

    #[test]
    fn refine_splits_atomically_and_lands_everything_in_review() {
        let (_dir, store) = scratch();
        let target = create(&store, "big vague thing");
        apply(&store, None, Op::SetTicketStatus { id: target.clone(), status: Status::Stub }).unwrap();

        let spec = |title: &str, deps: Vec<&str>| NewTicketSpec {
            title: title.into(),
            body: format!("spec for {title}"),
            labels: vec![],
            depends_on: deps.into_iter().map(String::from).collect(),
            epic: None,
            model: None,
            effort: None,
        };
        let applied = apply(
            &store,
            None,
            Op::Refine {
                target: RefineTarget::Ticket(target.clone()),
                title: Some("big, now specific, thing".into()),
                body: "# The plan\ndetails".into(),
                split_tickets: vec![spec("part one", vec![]), spec("part two", vec!["new:0", &target.0])],
                split_epics: vec![NewEpicSpec {
                    title: "follow-on epic".into(),
                    color: None,
                    body: String::new(),
                    tickets: vec![spec("epic child", vec![])],
                }],
            },
        )
        .unwrap();

        // Creation order: epic, its child, then the split tickets.
        assert_eq!(applied.created_ids, vec!["EP-1", "K-2", "K-3", "K-4"]);
        let board = store.read_board().unwrap();
        let refined = board.ticket(&target).unwrap();
        assert_eq!(refined.status, Status::Review);
        assert_eq!(refined.title, "big, now specific, thing");
        let part_two = board.ticket(&TicketId("K-4".into())).unwrap();
        assert_eq!(part_two.depends_on, vec![TicketId("K-3".into()), target.clone()], "new:0 resolves to the first split ticket");
        let child = board.ticket(&TicketId("K-2".into())).unwrap();
        assert_eq!(child.epic.as_ref().unwrap().0, "EP-1", "nested tickets belong to their new epic");
        assert!(board.tickets.iter().skip(1).all(|t| t.status == Status::Review), "everything created is review");
    }

    #[test]
    fn refine_refuses_bad_targets_and_bad_placeholders_atomically() {
        let (_dir, store) = scratch();
        let ready = create(&store, "ready one");
        let err = apply(
            &store,
            None,
            Op::Refine { target: RefineTarget::Ticket(ready.clone()), title: None, body: "x".into(), split_tickets: vec![], split_epics: vec![] },
        )
        .unwrap_err();
        assert!(err.to_string().contains("only stub"), "{err}");

        apply(&store, None, Op::SetTicketStatus { id: ready.clone(), status: Status::Stub }).unwrap();
        let before = std::fs::read(store.board_path()).unwrap();
        let err = apply(
            &store,
            None,
            Op::Refine {
                target: RefineTarget::Ticket(ready),
                title: None,
                body: "x".into(),
                split_tickets: vec![NewTicketSpec { title: "t".into(), body: String::new(), labels: vec![], depends_on: vec!["new:9".into()], epic: None, model: None, effort: None }],
                split_epics: vec![],
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("placeholder"), "{err}");
        assert_eq!(std::fs::read(store.board_path()).unwrap(), before, "failed refine writes nothing — not even the target's new body");
    }

    #[test]
    fn refine_split_tickets_inherit_the_targets_epic() {
        let (_dir, store) = scratch();
        let epic = apply(&store, None, Op::CreateEpic { title: "auth".into(), color: None, body: String::new(), status: Status::Ready }).unwrap();
        let epic_id = EpicId(epic.created_ids[0].clone());
        let target = create(&store, "stub in epic");
        apply(&store, None, Op::UpdateTicket { id: target.clone(), patch: TicketPatch { epic: Some(Some(epic_id.clone())), ..TicketPatch::default() } }).unwrap();
        apply(&store, None, Op::SetTicketStatus { id: target.clone(), status: Status::Stub }).unwrap();

        apply(
            &store,
            None,
            Op::Refine {
                target: RefineTarget::Ticket(target),
                title: None,
                body: "refined".into(),
                split_tickets: vec![NewTicketSpec { title: "child".into(), body: String::new(), labels: vec![], depends_on: vec![], epic: None, model: None, effort: None }],
                split_epics: vec![],
            },
        )
        .unwrap();
        let board = store.read_board().unwrap();
        assert_eq!(board.ticket(&TicketId("K-2".into())).unwrap().epic.as_ref(), Some(&epic_id));
    }

    #[test]
    fn stamp_and_clear_worktree_round_trip() {
        let (_dir, store) = scratch();
        let id = create(&store, "work");
        // Not in doing yet: refused.
        let err = apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "k-1/work".into(), path: "/tmp/wt/K-1".into() }).unwrap_err();
        assert!(err.to_string().contains("claim it first"), "{err}");

        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "k-1/work".into(), path: "/tmp/wt/K-1".into() }).unwrap();
        let claims = store.read_claims().unwrap();
        assert_eq!(claims[0].path.as_deref(), Some(std::path::Path::new("/tmp/wt/K-1")));
        assert_eq!(store.read_board().unwrap().ticket(&id).unwrap().column.branch(), Some("k-1/work"));

        apply(&store, None, Op::ClearWorktreePath { id: id.clone() }).unwrap();
        let claims = store.read_claims().unwrap();
        assert_eq!(claims.len(), 1, "the claim survives finish — the ticket is still in flight");
        assert!(claims[0].path.is_none());
    }

    #[test]
    fn stamping_an_external_ticket_is_refused_but_claiming_it_is_fine() {
        let (_dir, store) = scratch();
        let id = create(&store, "delegated");
        let external = crate::store::model::External { provider: "github".into(), kind: "issue".into(), number: 7 };
        apply(&store, None, Op::BindExternal { id: id.clone(), external: Some(external) }).unwrap();
        // Delegating claims the ticket on the daemon's behalf — external tickets are claimable…
        apply(&store, None, Op::Claim { id: id.clone(), agent: "minesweeper".into() }).unwrap();
        // …but never get a worktree here.
        let err = apply(&store, None, Op::StampWorktree { id: id.clone(), branch: "b".into(), path: "/p".into() }).unwrap_err();
        assert!(matches!(err, OpError::External(_)), "{err}");
        // Unbind clears it again.
        apply(&store, None, Op::BindExternal { id: id.clone(), external: None }).unwrap();
        assert!(store.read_board().unwrap().ticket(&id).unwrap().external.is_none());
    }

    #[test]
    fn created_epics_get_distinct_palette_colours() {
        let (_dir, store) = scratch();
        let mut colours = std::collections::HashSet::new();
        (0..3).for_each(|i| {
            let applied =
                apply(&store, None, Op::CreateEpic { title: format!("e{i}"), color: None, body: String::new(), status: Status::Draft }).unwrap();
            let board = store.read_board().unwrap();
            colours.insert(board.epics.last().unwrap().color.clone());
            assert_eq!(applied.created_ids[0], format!("EP-{}", i + 1));
        });
        assert_eq!(colours.len(), 3);
    }

    #[test]
    fn notes_append_with_authors_and_next_still_respects_the_board() {
        let (_dir, store) = scratch();
        let id = create(&store, "noted");
        apply(&store, None, Op::AddNote { id: id.clone(), text: "started".into(), author: Some("claude".into()) }).unwrap();
        apply(&store, None, Op::AddNote { id: id.clone(), text: "hmm".into(), author: None }).unwrap();
        let board = store.read_board().unwrap();
        let notes = &board.ticket(&id).unwrap().notes;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].author.as_deref(), Some("claude"));

        // Sanity: the read model still nominates it.
        assert_eq!(derive::next_ticket(&board, &store.read_claims().unwrap()).unwrap().id, id);
    }

    /// Claim + stamp + move to review: the standard close-out shape for a worked ticket.
    fn to_review(store: &Store, id: &TicketId, branch: &str) {
        apply(store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        apply(store, None, Op::StampWorktree { id: id.clone(), branch: branch.into(), path: "/tmp/unused".into() }).unwrap();
        apply(store, None, Op::MoveTicket { id: id.clone(), to: ColumnId::Review, position: None, owner: None, branch: None }).unwrap();
    }

    #[test]
    fn moving_to_review_carries_the_branch_and_drops_the_claim() {
        let (_dir, store) = scratch();
        let id = create(&store, "worked");
        to_review(&store, &id, "k-1/worked");

        let board = store.read_board().unwrap();
        assert!(matches!(&board.ticket(&id).unwrap().column, Column::Review { branch: Some(b) } if b == "k-1/worked"));
        assert!(store.read_claims().unwrap().is_empty(), "review work is nobody's — the claim drops on entry");
    }

    #[test]
    fn a_branch_override_on_the_move_records_a_companion_subtasks_shared_branch() {
        // A companion subtask is worked in its parent's worktree: claimed, never stamped. Without the override it would
        // reach review branchless and the auto-lander could never resolve it.
        let (_dir, store) = scratch();
        let id = create(&store, "companion");
        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        apply(
            &store,
            None,
            Op::MoveTicket { id: id.clone(), to: ColumnId::Review, position: None, owner: None, branch: Some("k-9/parent".into()) },
        )
        .unwrap();

        let board = store.read_board().unwrap();
        assert!(matches!(&board.ticket(&id).unwrap().column, Column::Review { branch: Some(b) } if b == "k-9/parent"));
    }

    #[test]
    fn claiming_from_review_is_the_rework_path_and_keeps_the_branch() {
        let (_dir, store) = scratch();
        let id = create(&store, "needs rework");
        to_review(&store, &id, "k-1/needs-rework");

        apply(&store, None, Op::Claim { id: id.clone(), agent: "claude".into() }).unwrap();
        let board = store.read_board().unwrap();
        assert!(
            matches!(&board.ticket(&id).unwrap().column, Column::Doing { owner, branch: Some(b) } if owner == "claude" && b == "k-1/needs-rework"),
            "rework re-claims with the branch intact, so worktree start re-attaches"
        );
    }

    #[test]
    fn land_ticket_moves_review_to_done_with_the_reason_on_the_record() {
        let (_dir, store) = scratch();
        let id = create(&store, "landed");
        to_review(&store, &id, "k-1/landed");

        apply(
            &store,
            None,
            Op::LandTicket { id: id.clone(), expected_branch: Some("k-1/landed".into()), reason: "k-1/landed merged into main".into() },
        )
        .unwrap();
        let board = store.read_board().unwrap();
        let ticket = board.ticket(&id).unwrap();
        assert!(matches!(&ticket.column, Column::Done { discarded: false, branch: Some(b), .. } if b == "k-1/landed"));
        assert_eq!(ticket.notes.last().unwrap().text, "k-1/landed merged into main");
        assert_eq!(ticket.notes.last().unwrap().author.as_deref(), Some("kanban"));
    }

    #[test]
    fn land_ticket_refuses_stale_evidence() {
        let (_dir, store) = scratch();
        let id = create(&store, "moved on");

        // Not in review at all.
        let err =
            apply(&store, None, Op::LandTicket { id: id.clone(), expected_branch: None, reason: "x".into() }).unwrap_err();
        assert!(err.to_string().contains("no longer in review"), "{err}");

        // In review, but the branch changed since the evidence was gathered.
        to_review(&store, &id, "k-1/rebranded");
        let err = apply(
            &store,
            None,
            Op::LandTicket { id: id.clone(), expected_branch: Some("k-1/original".into()), reason: "x".into() },
        )
        .unwrap_err();
        assert!(err.to_string().contains("branch changed"), "{err}");
        let board = store.read_board().unwrap();
        assert!(matches!(board.ticket(&id).unwrap().column, Column::Review { .. }), "a refused landing changes nothing");
    }

    #[test]
    fn discarding_closes_the_ticket_but_its_dependents_stay_blocked() {
        let (_dir, store) = scratch();
        let parent = create(&store, "abandoned approach");
        let child = create(&store, "follow-up");
        apply(
            &store,
            None,
            Op::UpdateTicket { id: child.clone(), patch: TicketPatch { depends_on: Some(vec![parent.clone()]), ..TicketPatch::default() } },
        )
        .unwrap();
        to_review(&store, &parent, "k-1/abandoned");

        apply(&store, None, Op::DiscardTicket { id: parent.clone(), reason: "superseded by a different design".into() }).unwrap();
        let board = store.read_board().unwrap();
        assert!(matches!(board.ticket(&parent).unwrap().column, Column::Done { discarded: true, .. }));
        assert!(derive::blocked(board.ticket(&child).unwrap(), &board), "the promised code never landed — the dependent stays blocked");
        assert!(derive::next_ticket(&board, &[]).is_none(), "nothing on this board is workable");

        // Only review tickets can be discarded — done and todo tickets refuse.
        let err = apply(&store, None, Op::DiscardTicket { id: parent, reason: "again".into() }).unwrap_err();
        assert!(err.to_string().contains("not in review"), "{err}");
    }

    #[test]
    fn set_pr_records_updates_and_clears() {
        use crate::store::model::{PrRef, PrState};
        let (_dir, store) = scratch();
        let id = create(&store, "tracked");

        let open = PrRef { number: 7, url: "https://example.invalid/pull/7".into(), state: PrState::Open, merged_commit: None };
        apply(&store, None, Op::SetPr { id: id.clone(), pr: Some(open) }).unwrap();
        let board = store.read_board().unwrap();
        assert_eq!(board.ticket(&id).unwrap().pr.as_ref().unwrap().number, 7);

        let merged =
            PrRef { number: 7, url: "https://example.invalid/pull/7".into(), state: PrState::Merged, merged_commit: Some("abc123".into()) };
        apply(&store, None, Op::SetPr { id: id.clone(), pr: Some(merged) }).unwrap();
        let board = store.read_board().unwrap();
        assert_eq!(board.ticket(&id).unwrap().pr.as_ref().unwrap().state, PrState::Merged);

        apply(&store, None, Op::SetPr { id: id.clone(), pr: None }).unwrap();
        assert!(store.read_board().unwrap().ticket(&id).unwrap().pr.is_none());
    }

    #[test]
    fn every_ticket_sharing_a_branch_lands_when_it_lands() {
        // One worktree, one branch, several tickets (a parent and its companion subtasks): when the branch reaches main,
        // the sweep lands each of them independently — none may be left behind.
        let (_dir, store) = scratch();
        let parent = create(&store, "parent");
        let companion = create(&store, "companion");
        to_review(&store, &parent, "k-1/shared");
        apply(&store, None, Op::Claim { id: companion.clone(), agent: "claude".into() }).unwrap();
        apply(
            &store,
            None,
            Op::MoveTicket { id: companion.clone(), to: ColumnId::Review, position: None, owner: None, branch: Some("k-1/shared".into()) },
        )
        .unwrap();

        for id in [&parent, &companion] {
            apply(
                &store,
                None,
                Op::LandTicket { id: id.clone(), expected_branch: Some("k-1/shared".into()), reason: "k-1/shared merged into main".into() },
            )
            .unwrap();
        }
        let board = store.read_board().unwrap();
        assert!(board.tickets.iter().all(|t| matches!(t.column, Column::Done { discarded: false, .. })));
    }
}
