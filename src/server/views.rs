//! Askama template structs and the view-model builders that feed them.
//!
//! Templates are dumb: every derived fact (colours, badges, human times, csv joins) is computed here from the read model in
//! [`crate::store::derive`], so the `.html` files stay declarative. Handlers call `render()` and wrap the result in
//! [`axum::response::Html`] — no askama/axum integration crate, which keeps us off that version treadmill.

use std::collections::HashSet;

use askama::Template;
use chrono::{DateTime, Utc};

use crate::store::{
    Claim,
    derive::{self, BoardView, ClaimView, EpicView, TicketView},
    model::{Board, ColumnId, Status},
};

/// The four statuses in workflow order, for the status button groups.
const STATUSES: [Status; 4] = [Status::Draft, Status::Stub, Status::Review, Status::Ready];

/// The board's active filters, straight from the query string. Empty strings mean "no filter" (that's what empty form
/// fields submit).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Filters {
    #[serde(default)]
    pub epic: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    /// The "show merged" toggle: an unchecked checkbox submits nothing, checked submits `merged=1`.
    #[serde(default)]
    pub merged: String,
}

impl Filters {
    /// Deliberately ignores `merged`: it drives `draggable`, and disabling drag on the default view would kill the
    /// board's core interaction. Consequence: with merged cards hidden, a drop *into* the Done column can land at an
    /// index offset by the hidden cards above it — cosmetic only, since order in `done` carries no priority semantics;
    /// todo/doing are untouched (only done tickets are ever hidden).
    fn is_empty(&self) -> bool {
        self.epic.is_empty() && self.label.is_empty() && self.status.is_empty()
    }

    fn show_merged(&self) -> bool {
        !self.merged.is_empty()
    }

    fn admits_ticket(&self, t: &TicketView) -> bool {
        (self.epic.is_empty() || t.ticket.epic.as_ref().is_some_and(|e| e.0 == self.epic))
            && (self.label.is_empty() || t.ticket.labels.iter().any(|l| l == &self.label))
            && (self.status.is_empty() || t.ticket.status.as_str() == self.status)
    }

    fn admits_epic(&self, e: &EpicView) -> bool {
        (self.epic.is_empty() || e.epic.id.0 == self.epic) && (self.status.is_empty() || e.epic.status.as_str() == self.status)
    }
}

// ---- page shell -------------------------------------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "page.html")]
pub struct PageTpl {
    pub title: String,
    /// Crate version for the header badge, e.g. "1.1.0" — from the manifest, so it never drifts from the real build.
    pub version: &'static str,
    /// The plugin's repo, behind the header's GitHub mark. Also manifest-sourced (`[package] repository`).
    pub repo_url: &'static str,
    pub epics: Vec<EpicOptionCtx>,
    pub filter_oob: bool,
}

/// One `<option>` of the epic dropdowns (filter bar, create/edit forms).
#[derive(Debug)]
pub struct EpicOptionCtx {
    pub id: String,
    pub title: String,
    pub selected: bool,
}

#[must_use] 
pub fn page(title: String, board: &Board) -> PageTpl {
    PageTpl {
        title,
        version: env!("CARGO_PKG_VERSION"),
        repo_url: env!("CARGO_PKG_REPOSITORY"),
        epics: epic_options(board, None),
        filter_oob: false,
    }
}

fn epic_options(board: &Board, selected: Option<&str>) -> Vec<EpicOptionCtx> {
    board
        .epics
        .iter()
        .map(|e| EpicOptionCtx { id: e.id.to_string(), title: e.title.clone(), selected: selected == Some(e.id.0.as_str()) })
        .collect()
}

// ---- the board fragment -----------------------------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "board.html")]
pub struct BoardTpl {
    pub version: u64,
    /// Dragging is disabled while filters hide cards: a drop index among visible cards is meaningless.
    pub draggable: bool,
    pub columns: Vec<ColumnCtx>,
    pub epics: Vec<EpicOptionCtx>,
    pub filter_oob: bool,
}

#[derive(Debug)]
pub struct ColumnCtx {
    pub id: ColumnId,
    pub title: String,
    pub cards: Vec<CardCtx>,
    pub epics: Vec<EpicCardCtx>,
    /// Merged done cards dropped by the default view — the Done header hints at them so they never look lost.
    /// Always 0 except Done.
    pub hidden_merged: usize,
}

// Not a state machine: each bool is an independent display flag with its own badge or styling.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct CardCtx {
    pub id: String,
    pub title: String,
    pub color: String,
    pub status: Status,
    pub status_badge: &'static str,
    pub done: bool,
    pub blocked: bool,
    /// A stub sitting in `doing` is having its spec written right now — the card renders pink while that lasts.
    pub refining: bool,
    /// A done ticket whose branch has landed in the main checkout's HEAD (or is gone) — purple badge, hidden by default.
    pub merged: bool,
    pub labels: Vec<String>,
    pub external: Option<String>,
    pub claim: Option<ClaimCtx>,
    pub branch: Option<String>,
    /// The `doing` owner, for cards that are owned but not live-claimed (e.g. dragged into doing by a human).
    pub owner: Option<String>,
}

#[derive(Debug)]
pub struct ClaimCtx {
    pub agent: String,
    pub since: String,
    pub path: Option<String>,
    pub worktree_missing: bool,
}

#[derive(Debug)]
pub struct EpicCardCtx {
    pub id: String,
    pub title: String,
    pub color: String,
    pub status: Status,
    pub status_badge: &'static str,
    pub items: Vec<ItemCtx>,
}

#[derive(Debug)]
pub struct ItemCtx {
    pub ticket: String,
    pub title: String,
    pub done: bool,
}

/// The default colour of a ticket with no epic: a neutral grey stripe.
const NO_EPIC_COLOR: &str = "#9ca3af";

// A view-model builder has one caller and no use for custom hashers.
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn board(view: &BoardView, filters: &Filters, unmerged: Option<&HashSet<String>>) -> BoardTpl {
    let columns = view
        .columns
        .iter()
        .map(|meta| {
            let (shown, hidden): (Vec<_>, Vec<_>) = view
                .tickets
                .iter()
                .filter(|t| t.ticket.column.id() == meta.id && filters.admits_ticket(t))
                .partition(|t| filters.show_merged() || !is_merged(t, unmerged));
            ColumnCtx {
                id: meta.id,
                title: meta.title.clone(),
                cards: shown.into_iter().map(|t| card(t, view, unmerged)).collect(),
                epics: view
                    .epics
                    .iter()
                    .filter(|e| e.column == meta.id && filters.admits_epic(e))
                    .map(epic_card)
                    .collect(),
                hidden_merged: hidden.len(),
            }
        })
        .collect();
    BoardTpl {
        version: view.version,
        draggable: filters.is_empty(),
        columns,
        epics: view
            .epics
            .iter()
            .map(|e| EpicOptionCtx {
                id: e.epic.id.to_string(),
                title: e.epic.title.clone(),
                selected: e.epic.id.0 == filters.epic,
            })
            .collect(),
        filter_oob: true,
    }
}

/// Whether the ticket reads as merged: done, non-external, with a recorded branch that is *absent* from the unmerged
/// set — either its tip is an ancestor of the anchor or the branch is gone (merged-and-deleted; see
/// [`crate::git::unmerged_branches`]). External tickets never wear the badge: their `branch` is whatever the delegate
/// created on the far side and was never a local branch, so its absence proves nothing. Done tickets with no branch are
/// never merged — there is nothing to check; they stay visible. `unmerged: None` (no git answer) flags nothing.
fn is_merged(t: &TicketView, unmerged: Option<&HashSet<String>>) -> bool {
    t.ticket.column.id() == ColumnId::Done
        && t.ticket.external.is_none()
        && t.ticket.column.branch().is_some_and(|b| unmerged.is_some_and(|u| !u.contains(b)))
}

fn card(t: &TicketView, view: &BoardView, unmerged: Option<&HashSet<String>>) -> CardCtx {
    CardCtx {
        id: t.ticket.id.to_string(),
        title: t.ticket.title.clone(),
        color: epic_color(view, t.ticket.epic.as_ref().map(|e| e.0.as_str())),
        status: t.ticket.status,
        status_badge: status_badge(t.ticket.status),
        done: t.ticket.column.id() == ColumnId::Done,
        blocked: t.blocked,
        refining: t.ticket.status == Status::Stub && t.ticket.column.id() == ColumnId::Doing,
        merged: is_merged(t, unmerged),
        labels: t.ticket.labels.clone(),
        external: t.ticket.external.as_ref().map(|e| format!("{} {}#{}", e.provider, e.kind, e.number)),
        claim: t.claim.as_ref().map(claim_ctx),
        branch: t.ticket.column.branch().map(str::to_owned),
        owner: match &t.ticket.column {
            crate::store::model::Column::Doing { owner, .. } => Some(owner.clone()),
            _ => None,
        },
    }
}

fn claim_ctx(c: &ClaimView) -> ClaimCtx {
    ClaimCtx {
        agent: c.agent.clone(),
        since: human_time(c.since),
        path: c.path.as_ref().map(|p| p.display().to_string()),
        worktree_missing: c.worktree_missing,
    }
}

fn epic_card(e: &EpicView) -> EpicCardCtx {
    EpicCardCtx {
        id: e.epic.id.to_string(),
        title: e.epic.title.clone(),
        color: e.epic.color.clone(),
        status: e.epic.status,
        status_badge: status_badge(e.epic.status),
        items: e.tickets.iter().map(|i| ItemCtx { ticket: i.ticket.to_string(), title: i.title.clone(), done: i.done }).collect(),
    }
}

fn epic_color(view: &BoardView, epic: Option<&str>) -> String {
    epic.and_then(|id| view.epics.iter().find(|e| e.epic.id.0 == id))
        .map_or_else(|| NO_EPIC_COLOR.to_owned(), |e| e.epic.color.clone())
}

// ---- detail panes -----------------------------------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "detail.html")]
pub struct DetailTpl {
    pub ticket: TicketCtx,
}

#[derive(Debug)]
pub struct TicketCtx {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub status_badge: &'static str,
    pub column: ColumnId,
    pub blocked: bool,
    pub external: Option<String>,
    pub epic: Option<EpicRefCtx>,
    pub labels: Vec<String>,
    pub claim: Option<ClaimCtx>,
    pub branch: Option<String>,
    pub completed_at: Option<String>,
    /// Whether the pane shows the Create PR button — computed by the handlers via `pr::eligible` (it needs subprocesses,
    /// and views stay pure).
    pub can_pr: bool,
    pub deps: Vec<DepCtx>,
    pub notes: Vec<NoteCtx>,
    pub statuses: Vec<StatusOptCtx>,
}

#[derive(Debug)]
pub struct EpicRefCtx {
    pub id: String,
    pub title: String,
    pub color: String,
}

#[derive(Debug)]
pub struct DepCtx {
    pub id: String,
    pub title: String,
    pub done: bool,
}

#[derive(Debug)]
pub struct NoteCtx {
    pub at: String,
    pub author: Option<String>,
    pub text: String,
}

#[derive(Debug)]
pub struct StatusOptCtx {
    pub name: &'static str,
    pub current: bool,
}

pub fn detail(board: &Board, claims: &[Claim], id: &crate::store::model::TicketId, can_pr: bool) -> Option<DetailTpl> {
    use crate::store::model::Column;
    let t = board.ticket(id)?;
    let claim = crate::store::find_claim(claims, id).map(|c| claim_ctx(&ClaimView::from(c)));
    let completed_at = match &t.column {
        Column::Done { completed_at, .. } => Some(human_time(*completed_at)),
        _ => None,
    };
    Some(DetailTpl {
        ticket: TicketCtx {
            id: t.id.to_string(),
            title: t.title.clone(),
            status: t.status,
            status_badge: status_badge(t.status),
            column: t.column.id(),
            blocked: derive::blocked(t, board),
            external: t.external.as_ref().map(|e| format!("{} {}#{}", e.provider, e.kind, e.number)),
            epic: t.epic.as_ref().and_then(|eid| board.epic(eid)).map(|e| EpicRefCtx {
                id: e.id.to_string(),
                title: e.title.clone(),
                color: e.color.clone(),
            }),
            labels: t.labels.clone(),
            claim,
            branch: t.column.branch().map(str::to_owned),
            completed_at,
            can_pr,
            deps: t
                .depends_on
                .iter()
                .map(|dep| DepCtx {
                    id: dep.to_string(),
                    title: board.ticket(dep).map_or_else(|| "(missing)".into(), |d| d.title.clone()),
                    // The checkmark mirrors derive::blocked — a discarded dependency never satisfies.
                    done: matches!(board.ticket(dep).map(|d| &d.column), Some(Column::Done { discarded: false, .. })),
                })
                .collect(),
            notes: t
                .notes
                .iter()
                .map(|n| NoteCtx { at: human_time(n.at), author: n.author.clone(), text: n.text.clone() })
                .collect(),
            statuses: STATUSES.map(|s| StatusOptCtx { name: s.as_str(), current: s == t.status }).into(),
        },
    })
}

#[derive(Debug, Template)]
#[template(path = "detail_edit.html")]
pub struct DetailEditTpl {
    pub ticket: EditCtx,
    pub epics: Vec<EpicOptionCtx>,
}

#[derive(Debug)]
pub struct EditCtx {
    pub id: String,
    pub title: String,
    pub body: String,
    pub labels_csv: String,
    pub deps_csv: String,
}

pub fn detail_edit(board: &Board, id: &crate::store::model::TicketId) -> Option<DetailEditTpl> {
    let t = board.ticket(id)?;
    Some(DetailEditTpl {
        ticket: EditCtx {
            id: t.id.to_string(),
            title: t.title.clone(),
            body: t.body.clone(),
            labels_csv: t.labels.join(", "),
            deps_csv: t.depends_on.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        },
        epics: epic_options(board, t.epic.as_ref().map(|e| e.0.as_str())),
    })
}

#[derive(Debug, Template)]
#[template(path = "epic_detail.html")]
pub struct EpicDetailTpl {
    pub epic: EpicDetailCtx,
}

#[derive(Debug)]
pub struct EpicDetailCtx {
    pub id: String,
    pub title: String,
    pub color: String,
    pub status: Status,
    pub status_badge: &'static str,
    pub column: ColumnId,
    pub has_body: bool,
    pub items: Vec<ItemCtx>,
    pub statuses: Vec<StatusOptCtx>,
}

#[must_use] 
pub fn epic_detail(board: &Board, id: &crate::store::model::EpicId) -> Option<EpicDetailTpl> {
    use crate::store::model::Column;
    let e = board.epic(id)?;
    Some(EpicDetailTpl {
        epic: EpicDetailCtx {
            id: e.id.to_string(),
            title: e.title.clone(),
            color: e.color.clone(),
            status: e.status,
            status_badge: status_badge(e.status),
            column: derive::epic_column(id, board),
            has_body: !e.body.is_empty(),
            items: board
                .tickets
                .iter()
                .filter(|t| t.epic.as_ref() == Some(id))
                .map(|t| ItemCtx { ticket: t.id.to_string(), title: t.title.clone(), done: matches!(t.column, Column::Done { .. }) })
                .collect(),
            statuses: STATUSES.map(|s| StatusOptCtx { name: s.as_str(), current: s == e.status }).into(),
        },
    })
}

#[derive(Debug, Template)]
#[template(path = "epic_edit.html")]
pub struct EpicEditTpl {
    pub epic: EpicEditCtx,
}

#[derive(Debug)]
pub struct EpicEditCtx {
    pub id: String,
    pub title: String,
    pub color: String,
    pub body: String,
}

#[must_use] 
pub fn epic_edit(board: &Board, id: &crate::store::model::EpicId) -> Option<EpicEditTpl> {
    let e = board.epic(id)?;
    Some(EpicEditTpl {
        epic: EpicEditCtx { id: e.id.to_string(), title: e.title.clone(), color: e.color.clone(), body: e.body.clone() },
    })
}

// ---- toasts -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "toast.html")]
pub struct ToastTpl {
    pub kind: &'static str,
    pub message: String,
}

impl ToastTpl {
    #[must_use] 
    pub fn error(message: String) -> ToastTpl {
        ToastTpl { kind: "alert-error", message }
    }

    #[must_use] 
    pub fn warning(message: String) -> ToastTpl {
        ToastTpl { kind: "alert-warning", message }
    }
}

// ---- shared helpers ---------------------------------------------------------------------------------------------------

/// The `DaisyUI` badge class for a status: how well-defined the work is, at a glance.
fn status_badge(s: Status) -> &'static str {
    match s {
        Status::Draft => "badge-ghost",
        Status::Stub => "badge-warning",
        Status::Review => "badge-info",
        Status::Ready => "badge-success",
    }
}

fn human_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}
