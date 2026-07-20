//! Askama template structs and the view-model builders that feed them.
//!
//! Templates are dumb: every derived fact (colours, badges, human times, csv joins) is computed here from the read model in
//! [`crate::store::derive`], so the `.html` files stay declarative. Handlers call `render()` and wrap the result in
//! [`axum::response::Html`] — no askama/axum integration crate, which keeps us off that version treadmill.

use std::collections::HashSet;

use askama::Template;
use chrono::{DateTime, Utc};

use crate::{
    server::search::Query,
    store::{
        Claim,
        derive::{self, BoardView, ClaimView, EpicView, TicketView},
        model::{Board, ColumnId, Effort, Epic, Status},
    },
};

/// The four statuses in workflow order, for the status button groups.
const STATUSES: [Status; 4] = [Status::Draft, Status::Stub, Status::Review, Status::Ready];

/// The model aliases the `<datalist>` suggests. Only suggestions — the field is free text, because `--model` takes a
/// full id (`claude-opus-4-8`) just as happily as an alias.
pub const MODEL_SUGGESTIONS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// A ticket's model/effort preference as one badge: `opus · xhigh`, or whichever half is set. `None` when neither is —
/// the overwhelming majority of tickets, which should stay visually quiet.
fn run_badge(ticket: &crate::store::model::Ticket) -> Option<String> {
    match (ticket.model.as_deref(), ticket.effort) {
        (None, None) => None,
        (Some(m), None) => Some(m.to_owned()),
        (None, Some(e)) => Some(e.to_string()),
        (Some(m), Some(e)) => Some(format!("{m} · {e}")),
    }
}

/// The board's active filters, straight from the query string. Empty strings mean "no filter" (that's what empty form
/// fields submit). The epic dropdown stays its own parameter — it is a *discovery* affordance for ids and titles nobody
/// memorises — and ANDs with the search box.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Filters {
    #[serde(default)]
    pub epic: String,
    /// The search box, raw. Parsed by [`crate::server::search::Query::parse`].
    #[serde(default)]
    pub q: String,
}

impl Filters {
    /// Whether these filters hide nothing — the board only drags while that holds. The query half is the parsed
    /// query's own emptiness, never a string check on `q`: a query that parses to no terms hides nothing by
    /// construction.
    fn is_empty(&self, q: &Query) -> bool {
        self.epic.is_empty() && q.is_empty()
    }

    fn admits_ticket(&self, q: &Query, t: &TicketView, epics: &[EpicView]) -> bool {
        (self.epic.is_empty() || t.ticket.epic.as_ref().is_some_and(|e| e.0 == self.epic)) && q.matches(t, epics)
    }

    fn admits_epic(&self, q: &Query, e: &EpicView) -> bool {
        (self.epic.is_empty() || e.epic.id.0 == self.epic) && q.matches_epic(e)
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
    /// Model aliases the create form's `<datalist>` suggests; the field itself stays free text.
    pub models: [&'static str; 4],
    /// Effort levels for the create form's select, all unselected — a new ticket inherits by default.
    pub efforts: Vec<EffortOptCtx>,
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
        models: MODEL_SUGGESTIONS,
        efforts: effort_options(None),
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
    /// A done ticket retired without landing: closed, but its dependents stay blocked.
    pub discarded: bool,
    /// The bound PR, rendered as a linked badge on cards still in flight (done cards drop it — the story is over).
    pub pr: Option<PrCtx>,
    /// A review ticket whose recorded branch no longer exists locally and nothing proves it landed — the human's call.
    pub branch_gone: bool,
    /// The *effective* auto-merge grant — the ticket's own flag or its epic's. A card wearing it will move main with
    /// nobody watching, so it gets a warning badge.
    pub auto_merge: bool,
    /// The grant is the epic's alone, so the badge says `auto-merge (epic)` — clearing it is done on the epic.
    pub auto_merge_inherited: bool,
    pub labels: Vec<String>,
    /// The model/effort preference, pre-rendered as one badge — the point is spotting the expensive tickets at a glance.
    pub run: Option<String>,
    pub external: Option<String>,
    pub claim: Option<ClaimCtx>,
    pub branch: Option<String>,
    /// The `doing` owner, for cards that are owned but not live-claimed (e.g. dragged into doing by a human).
    pub owner: Option<String>,
}

/// A bound PR, pre-rendered for the templates: one linked badge, label and colour chosen by state.
#[derive(Debug)]
pub struct PrCtx {
    pub url: String,
    pub label: String,
    pub class: &'static str,
    pub title: &'static str,
}

fn pr_ctx(pr: &crate::store::model::PrRef) -> PrCtx {
    use crate::store::model::PrState;
    let (label, class, title) = match pr.state {
        PrState::Open => (format!("PR #{}", pr.number), "badge-ghost", "open on GitHub"),
        PrState::Merged => (
            format!("PR #{} merged — pull main", pr.number),
            "badge-warning",
            "merged on GitHub; lands in done once the merge reaches your local main branch",
        ),
        PrState::Closed => (format!("PR #{} closed", pr.number), "badge-error", "closed without merging — rework the ticket, or discard it"),
    };
    PrCtx { url: pr.url.clone(), label, class, title }
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
pub fn board(view: &BoardView, filters: &Filters, heads: Option<&HashSet<String>>) -> BoardTpl {
    // Parsed once per render, not once per card: a linear scan over ~26 tickets is dwarfed by the git subprocess the
    // handler already spawns, but re-parsing per ticket would be gratuitous.
    let query = Query::parse(&filters.q);
    let columns = view
        .columns
        .iter()
        .map(|meta| ColumnCtx {
            id: meta.id,
            title: meta.title.clone(),
            cards: view
                .tickets
                .iter()
                .filter(|t| t.ticket.column.id() == meta.id && filters.admits_ticket(&query, t, &view.epics))
                .map(|t| card(t, view, heads))
                .collect(),
            epics: view.epics.iter().filter(|e| e.column == meta.id && filters.admits_epic(&query, e)).map(epic_card).collect(),
        })
        .collect();
    BoardTpl {
        version: view.version,
        draggable: filters.is_empty(&query),
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

fn card(t: &TicketView, view: &BoardView, heads: Option<&HashSet<String>>) -> CardCtx {
    let done = t.ticket.column.id() == ColumnId::Done;
    CardCtx {
        id: t.ticket.id.to_string(),
        title: t.ticket.title.clone(),
        color: epic_color(view, t.ticket.epic.as_ref().map(|e| e.0.as_str())),
        status: t.ticket.status,
        status_badge: status_badge(t.ticket.status),
        done,
        blocked: t.blocked,
        refining: t.ticket.status == Status::Stub && t.ticket.column.id() == ColumnId::Doing,
        discarded: matches!(t.ticket.column, crate::store::model::Column::Done { discarded: true, .. }),
        pr: (!done).then(|| t.ticket.pr.as_ref().map(pr_ctx)).flatten(),
        branch_gone: t.ticket.column.id() == ColumnId::Review
            && t.ticket.external.is_none()
            && t.ticket.column.branch().is_some_and(|b| heads.is_some_and(|h| !h.contains(b))),
        // Already derived once per board render, by `derive::board_view` — the card has no `Board` to ask again.
        auto_merge: t.auto_merge_effective,
        auto_merge_inherited: t.auto_merge_effective && !t.ticket.auto_merge,
        labels: t.ticket.labels.clone(),
        run: run_badge(&t.ticket),
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

// Not a state machine: each bool is an independent display flag with its own badge or button.
#[allow(clippy::struct_excessive_bools)]
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
    /// The model/effort preference as one badge, same as on the card.
    pub run: Option<String>,
    pub claim: Option<ClaimCtx>,
    pub branch: Option<String>,
    pub completed_at: Option<String>,
    /// Whether the pane shows the Create PR button — computed by the handlers via `pr::eligible` (it needs subprocesses,
    /// and views stay pure).
    pub can_pr: bool,
    /// The bound PR — shown in the pane whatever the column, as provenance once the ticket lands.
    pub pr: Option<PrCtx>,
    /// Review tickets can be retired without landing: done with `discarded: true`, dependents stay blocked.
    pub can_discard: bool,
    pub discarded: bool,
    /// The effective auto-merge grant, same as the card's — it fills the toggle button and raises the warning badge.
    pub auto_merge: bool,
    /// The grant is the epic's alone: the button says so, and its confirm explains it cannot take the epic's away.
    pub auto_merge_inherited: bool,
    /// The whole text of the toggle's confirmation, spelled out server-side — see [`ticket_auto_merge_confirm`].
    pub auto_merge_confirm: String,
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

/// The scare text shared by both auto-merge toggles: what the machine does to main once a flagged ticket reaches review.
/// Written out server-side like [`epic_delete_confirm`] — a cost this size belongs in the dialog, not in the docs.
fn auto_merge_on_confirm(target: &str, subject: &str) -> String {
    format!(
        "Turn on auto-merge for {target}? When {subject} reaches review, /kanban:work will rebase its branch onto main \
         and fast-forward main into it with no human review of the merge, resolving any rebase conflicts on its own. \
         There is no undo once main has moved."
    )
}

/// What the ticket's toggle asks. Turning it off is the safe direction and stays terse — unless the grant is the epic's,
/// where the honest answer is that this button cannot take it away. `from_epic` is `Some` only in that case.
fn ticket_auto_merge_confirm(id: &str, title: &str, on: bool, from_epic: Option<&Epic>) -> String {
    match (on, from_epic) {
        (false, _) => auto_merge_on_confirm(&format!("{id} — {title}"), "this ticket"),
        (true, Some(e)) => format!(
            "Auto-merge for {id} — {title} comes from {} — {}, not from the ticket. Clearing the ticket's own flag \
             leaves the epic's grant standing, so {id} still auto-merges — switch it off on the epic instead.",
            e.id, e.title
        ),
        (true, None) => format!("Turn off auto-merge for {id} — {title}? It goes back to waiting for your review before it lands."),
    }
}

/// What the epic's toggle asks. Turning it on names how many tickets the grant reaches: it is one click for the list.
fn epic_auto_merge_confirm(id: &str, title: &str, on: bool, count: usize) -> String {
    if on {
        return format!("Turn off auto-merge for {id} — {title}? Its tickets keep whatever flags they set for themselves.");
    }
    let plural = if count == 1 { "ticket" } else { "tickets" };
    let target = if count == 0 { format!("{id} — {title}") } else { format!("{id} — {title} and its {count} {plural}") };
    let subject = if count == 0 { "a ticket filed under it" } else { "any of them" };
    auto_merge_on_confirm(&target, subject)
}

pub fn detail(board: &Board, claims: &[Claim], id: &crate::store::model::TicketId, can_pr: bool) -> Option<DetailTpl> {
    use crate::store::model::Column;
    let t = board.ticket(id)?;
    let claim = crate::store::find_claim(claims, id).map(|c| claim_ctx(&ClaimView::from(c)));
    let completed_at = match &t.column {
        Column::Done { completed_at, .. } => Some(human_time(*completed_at)),
        _ => None,
    };
    let epic = t.epic.as_ref().and_then(|eid| board.epic(eid));
    let auto_merge = derive::auto_merge(t, board);
    // The epic granted it and the ticket's own flag is clear — which is exactly when the toggle cannot switch it off.
    let auto_merge_inherited = auto_merge && !t.auto_merge;
    Some(DetailTpl {
        ticket: TicketCtx {
            id: t.id.to_string(),
            title: t.title.clone(),
            status: t.status,
            status_badge: status_badge(t.status),
            column: t.column.id(),
            blocked: derive::blocked(t, board),
            external: t.external.as_ref().map(|e| format!("{} {}#{}", e.provider, e.kind, e.number)),
            epic: epic.map(|e| EpicRefCtx { id: e.id.to_string(), title: e.title.clone(), color: e.color.clone() }),
            labels: t.labels.clone(),
            run: run_badge(t),
            claim,
            branch: t.column.branch().map(str::to_owned),
            completed_at,
            can_pr,
            pr: t.pr.as_ref().map(pr_ctx),
            can_discard: t.column.id() == ColumnId::Review,
            discarded: matches!(t.column, Column::Done { discarded: true, .. }),
            auto_merge,
            auto_merge_inherited,
            auto_merge_confirm: ticket_auto_merge_confirm(
                &t.id.to_string(),
                &t.title,
                auto_merge,
                auto_merge_inherited.then_some(epic).flatten(),
            ),
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
    pub models: [&'static str; 4],
    pub efforts: Vec<EffortOptCtx>,
}

#[derive(Debug)]
pub struct EditCtx {
    pub id: String,
    pub title: String,
    pub body: String,
    pub labels_csv: String,
    pub deps_csv: String,
    /// Free text: an alias or a full model id. Empty means "inherit the session's".
    pub model: String,
}

/// One `<option>` of the effort select.
#[derive(Debug)]
pub struct EffortOptCtx {
    pub name: &'static str,
    pub selected: bool,
}

/// The effort options, with the ticket's own level pre-selected. Mirrors `epic_options`: an empty leading option is the
/// "inherit" case and lives in the template.
fn effort_options(current: Option<Effort>) -> Vec<EffortOptCtx> {
    Effort::ALL.map(|e| EffortOptCtx { name: e.as_str(), selected: Some(e) == current }).into()
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
            model: t.model.clone().unwrap_or_default(),
        },
        epics: epic_options(board, t.epic.as_ref().map(|e| e.0.as_str())),
        models: MODEL_SUGGESTIONS,
        efforts: effort_options(t.effort),
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
    /// The whole text of the delete confirmation, spelled out server-side — deletion cascades and there is no undo, so
    /// the dialog has to name what goes with the epic before the human clicks.
    pub delete_confirm: String,
    /// The epic's own auto-merge grant, which every ticket under it inherits.
    pub auto_merge: bool,
    /// The whole text of the toggle's confirmation — see [`epic_auto_merge_confirm`].
    pub auto_merge_confirm: String,
}

/// Spell out what deleting `EP-n — title` costs, counting the tickets that go with it and the done ones among them.
fn epic_delete_confirm(id: &str, title: &str, items: &[ItemCtx]) -> String {
    if items.is_empty() {
        return format!("Delete {id} — {title}? It has no tickets.");
    }
    let count = items.len();
    let plural = if count == 1 { "ticket" } else { "tickets" };
    let done = items.iter().filter(|i| i.done).count();
    let already_done = if done == 0 { String::new() } else { format!(" ({done} already done)") };
    format!(
        "Delete {id} — {title} and its {count} {plural}{already_done}? The tickets are deleted with it, other tickets' \
         dependencies on them are removed, and any worktrees or branches stay on disk. There is no undo."
    )
}

#[must_use]
pub fn epic_detail(board: &Board, id: &crate::store::model::EpicId) -> Option<EpicDetailTpl> {
    use crate::store::model::Column;
    let e = board.epic(id)?;
    let items: Vec<ItemCtx> = board
        .tickets
        .iter()
        .filter(|t| t.epic.as_ref() == Some(id))
        .map(|t| ItemCtx { ticket: t.id.to_string(), title: t.title.clone(), done: matches!(t.column, Column::Done { .. }) })
        .collect();
    Some(EpicDetailTpl {
        epic: EpicDetailCtx {
            id: e.id.to_string(),
            title: e.title.clone(),
            color: e.color.clone(),
            status: e.status,
            status_badge: status_badge(e.status),
            column: derive::epic_column(id, board),
            has_body: !e.body.is_empty(),
            delete_confirm: epic_delete_confirm(&e.id.to_string(), &e.title, &items),
            auto_merge: e.auto_merge,
            auto_merge_confirm: epic_auto_merge_confirm(&e.id.to_string(), &e.title, e.auto_merge, items.len()),
            items,
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

// ---- settings ---------------------------------------------------------------------------------------------------------

/// The settings pane: `.kanban/config.json` as a form. Values render raw (empty = unset, defaults live in the
/// placeholders), so what the user sees is exactly what the file will say.
#[derive(Debug, Template)]
#[template(path = "settings.html")]
pub struct SettingsTpl {
    pub worktree_root: String,
    /// One entry per line in the textarea.
    pub copy_to_worktrees: String,
    pub max_workers: String,
    pub idle_time: String,
    pub port: String,
    pub main_branch: String,
    pub poll_interval: String,
    /// True right after a save — shows the confirmation (and the port-needs-restart caveat).
    pub saved: bool,
}

#[must_use]
pub fn settings(config: &crate::config::Config, saved: bool) -> SettingsTpl {
    fn show<T: std::fmt::Display>(v: Option<&T>) -> String {
        v.map(ToString::to_string).unwrap_or_default()
    }
    SettingsTpl {
        worktree_root: config.worktree_root.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        copy_to_worktrees: config.copy_to_worktrees.join("\n"),
        max_workers: show(config.max_workers.as_ref()),
        idle_time: show(config.idle_time.as_ref()),
        port: show(config.port.as_ref()),
        main_branch: config.main_branch.clone().unwrap_or_default(),
        poll_interval: show(config.poll_interval.as_ref()),
        saved,
    }
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
