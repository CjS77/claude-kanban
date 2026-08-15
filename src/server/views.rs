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
    /// The board's model vocabulary ([`crate::config::Config::models`]) for the create form's `<datalist>`; the field
    /// itself stays free text.
    pub models: Vec<String>,
    /// The first vocabulary entry, as the model input's placeholder.
    pub model_placeholder: String,
    /// Effort levels for the create form's select, all unselected — a new ticket inherits by default.
    pub efforts: Vec<EffortOptCtx>,
    /// Whether the binary was built with the `minesweeper` feature — gates the create modal's handoff checkbox. The
    /// checkbox is deliberately toggle-blind: ticking it is the per-ticket opt-in, config or no config.
    pub minesweeper_available: bool,
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
pub fn page(title: String, board: &Board, models: Vec<String>) -> PageTpl {
    PageTpl {
        title,
        version: env!("CARGO_PKG_VERSION"),
        repo_url: env!("CARGO_PKG_REPOSITORY"),
        epics: epic_options(board, None),
        model_placeholder: models.first().cloned().unwrap_or_default(),
        models,
        efforts: effort_options(None),
        minesweeper_available: cfg!(feature = "minesweeper"),
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
    /// A reviewer sent this back: it sits in `doing` unclaimed, waiting for a worker. Badged so it is distinguishable
    /// at a glance from a card nobody has started.
    pub changes_requested: bool,
    /// A done ticket retired without landing: closed, but its dependents stay blocked.
    pub discarded: bool,
    /// The bound PR, rendered as a linked badge on cards still in flight (done cards drop it — the story is over).
    pub pr: Option<PrCtx>,
    /// A review ticket whose recorded branch no longer exists locally and nothing proves it landed — the human's call.
    pub branch_gone: bool,
    /// The human has cleared this to land; the work loop has not got to it yet.
    pub accepted: bool,
    /// A landing attempt hit something only a human can resolve. The card wears a danger badge *and* a red wash: unlike
    /// every other flag here this one is a dead end until somebody goes to the branch, so it is meant to be seen across
    /// the board.
    pub landing_blocked: bool,
    /// The *effective* auto-merge grant — the ticket's own flag or its epic's. A card wearing it will move main with
    /// nobody watching, so it gets a warning badge.
    pub auto_merge: bool,
    /// The grant is the epic's alone, so the badge says `auto-merge (epic)` — clearing it is done on the epic.
    pub auto_merge_inherited: bool,
    pub labels: Vec<String>,
    /// The model/effort preference, pre-rendered as one badge — the point is spotting the expensive tickets at a glance.
    pub run: Option<String>,
    pub external: Option<String>,
    /// Minesweeper trouble observed on the bound issue: a flag label verbatim, or the closed-without-a-PR shape. The
    /// card wears it until a human acts — the progress log carries the full story.
    pub external_flag: Option<String>,
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
    /// An in-flight landing rather than ownership — the card says "landing this", never "has this".
    pub landing: bool,
    /// That landing has outlived its staleness cutoff, so the loop may take it again. Said out loud on the card so a
    /// stalled landing reads as stalled rather than as busy forever.
    pub landing_stale: bool,
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
        changes_requested: t.ticket.changes_requested,
        discarded: matches!(t.ticket.column, crate::store::model::Column::Done { discarded: true, .. }),
        pr: (!done).then(|| t.ticket.pr.as_ref().map(pr_ctx)).flatten(),
        branch_gone: t.ticket.column.id() == ColumnId::Review
            && t.ticket.external.is_none()
            && t.ticket.column.branch().is_some_and(|b| heads.is_some_and(|h| !h.contains(b))),
        accepted: t.ticket.accepted,
        landing_blocked: t.ticket.landing_blocked,
        // Already derived once per board render, by `derive::board_view` — the card has no `Board` to ask again.
        auto_merge: t.auto_merge_effective,
        auto_merge_inherited: t.auto_merge_effective && !t.ticket.auto_merge,
        labels: t.ticket.labels.clone(),
        run: run_badge(&t.ticket),
        external: t.ticket.external.as_ref().map(|e| format!("{} {}#{}", e.provider, e.kind, e.number)),
        external_flag: external_flag(&t.ticket),
        claim: t.claim.as_ref().map(claim_ctx),
        branch: t.ticket.column.branch().map(str::to_owned),
        owner: match &t.ticket.column {
            crate::store::model::Column::Doing { owner, .. } => Some(owner.clone()),
            _ => None,
        },
    }
}

/// The warning a delegated ticket wears when the minesweeper poll observed trouble: a flag label verbatim, or the
/// closed-without-a-PR shape. Landed tickets drop it — the story is over — and a refined parent's closure is progress,
/// not trouble, so `sub_issues` suppresses the closed variant.
fn external_flag(t: &crate::store::model::Ticket) -> Option<String> {
    let ext = t.external.as_ref()?;
    if t.column.id() == ColumnId::Done {
        return None;
    }
    ext.flag.clone().or_else(|| {
        (ext.closed && t.column.id() == ColumnId::Doing && t.pr.is_none() && ext.sub_issues.is_empty())
            .then(|| "closed without a PR".to_owned())
    })
}

fn claim_ctx(c: &ClaimView) -> ClaimCtx {
    ClaimCtx {
        agent: c.agent.clone(),
        since: human_time(c.since),
        path: c.path.as_ref().map(|p| p.display().to_string()),
        worktree_missing: c.worktree_missing,
        landing: c.landing,
        landing_stale: c.landing_stale,
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
    /// The same minesweeper-trouble warning the card wears.
    pub external_flag: Option<String>,
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
    /// Whether the pane shows the View diff button — `diff::eligible`, which is `can_pr` without the remote requirement.
    pub can_diff: bool,
    /// The bound PR — shown in the pane whatever the column, as provenance once the ticket lands.
    pub pr: Option<PrCtx>,
    /// Review tickets can be retired without landing: done with `discarded: true`, dependents stay blocked.
    pub can_discard: bool,
    /// Whether the pane offers the Review tab: a review ticket that is ours to judge. External tickets are excluded —
    /// their verdict belongs on the delegate's issue, and `Op::RequestChanges` would refuse them anyway.
    pub can_review: bool,
    /// A reviewer sent this back and the feedback is not yet addressed — the card and pane both say so.
    pub changes_requested: bool,
    /// Cleared to land, waiting on a work loop — the same badge the card wears.
    pub accepted: bool,
    /// A landing attempt was refused: the same danger flag the card wears, so the pane you opened from it agrees.
    pub landing_blocked: bool,
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

/// The review pane (`templates/review.html`): everything a human needs to judge a review ticket, plus the three
/// verdicts. Swapped into `#detail` in place of the ordinary pane.
#[derive(Debug, Template)]
#[template(path = "review.html")]
pub struct ReviewTpl {
    pub review: ReviewCtx,
}

// Not a state machine: each bool is an independent display flag with its own banner or button.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct ReviewCtx {
    pub id: String,
    pub title: String,
    pub branch: Option<String>,
    /// The worktree kept through review — where the reviewer can read the code on disk.
    pub worktree_path: Option<String>,
    /// That worktree has uncommitted changes: the work in front of the reviewer is not all committed.
    pub worktree_dirty: bool,
    /// The landing sweep's own verdict, from [`crate::land::explain`] — what Accept is really deciding over.
    pub landing: Option<String>,
    /// Already cleared to land, waiting on a work loop.
    pub accepted: bool,
    /// A landing attempt was refused and is waiting on the human. Outranks `accepted` on the pane: it is the state
    /// they have to act on.
    pub landing_blocked: bool,
    pub notes: Vec<NoteCtx>,
    pub discard_confirm: String,
    /// Whether the pane offers its own View diff button — `diff::eligible`, the same predicate the detail pane uses.
    /// The reviewer's flow starts here, and the diff is where inline comments are written.
    pub can_diff: bool,
}

/// The review pane, for a review ticket that is ours to judge. `None` for anything else — a caller rendering this for a
/// todo ticket, or for a delegated one whose verdict belongs on its issue, is a bug rather than an empty pane.
///
/// The four inputs the views layer cannot compute itself — the worktree path, whether it is dirty, the landing verdict,
/// and diff eligibility — are passed in by the handler, which is free to run subprocesses; views stay pure.
#[must_use]
pub fn review(
    board: &Board,
    id: &crate::store::model::TicketId,
    worktree_path: Option<String>,
    worktree_dirty: bool,
    landing: Option<String>,
    can_diff: bool,
) -> Option<ReviewTpl> {
    let t = board.ticket(id)?;
    if t.column.id() != ColumnId::Review || t.external.is_some() {
        return None;
    }
    Some(ReviewTpl {
        review: ReviewCtx {
            id: t.id.to_string(),
            title: t.title.clone(),
            branch: t.column.branch().map(str::to_owned),
            worktree_path,
            worktree_dirty,
            accepted: t.accepted,
            landing_blocked: t.landing_blocked,
            discard_confirm: format!(
                "Discard {} — {}? It closes as done without landing, and tickets depending on it stay blocked.",
                t.id, t.title
            ),
            landing,
            notes: t.notes.iter().map(|n| NoteCtx { at: human_time(n.at), author: n.author.clone(), text: n.text.clone() }).collect(),
            can_diff,
        },
    })
}

/// The diff pane (`templates/diff.html`): a review branch's changes against main, ready for htmx to swap into the modal.
/// The per-file model comes from [`crate::diff::compute`]; this only tallies the summary line.
#[derive(Debug, Template)]
#[template(path = "diff.html")]
pub struct DiffTpl {
    /// Whose diff this is — stamped on the pane so glue.js can file inline comments against the right ticket's review
    /// pane. Nothing server-side reads it back.
    pub ticket_id: String,
    pub branch: String,
    pub main: String,
    pub files: Vec<crate::diff::FileDiff>,
    pub files_changed: usize,
    pub added: usize,
    pub deleted: usize,
}

#[must_use]
pub fn diff(ticket_id: String, branch: String, main: String, files: Vec<crate::diff::FileDiff>) -> DiffTpl {
    let files_changed = files.len();
    let added = files.iter().map(|f| f.added).sum();
    let deleted = files.iter().map(|f| f.deleted).sum();
    DiffTpl { ticket_id, branch, main, files, files_changed, added, deleted }
}

pub fn detail(board: &Board, claims: &[Claim], id: &crate::store::model::TicketId, can_pr: bool, can_diff: bool) -> Option<DetailTpl> {
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
            external_flag: external_flag(t),
            epic: epic.map(|e| EpicRefCtx { id: e.id.to_string(), title: e.title.clone(), color: e.color.clone() }),
            labels: t.labels.clone(),
            run: run_badge(t),
            claim,
            branch: t.column.branch().map(str::to_owned),
            completed_at,
            can_pr,
            can_diff,
            pr: t.pr.as_ref().map(pr_ctx),
            can_discard: t.column.id() == ColumnId::Review,
            can_review: t.column.id() == ColumnId::Review && t.external.is_none(),
            changes_requested: t.changes_requested,
            accepted: t.accepted,
            landing_blocked: t.landing_blocked,
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
    /// The board's model vocabulary ([`crate::config::Config::models`]); the input stays free text.
    pub models: Vec<String>,
    /// The first vocabulary entry, as the model input's placeholder.
    pub model_placeholder: String,
    pub efforts: Vec<EffortOptCtx>,
    /// Whether the form offers the "Hand to minesweeper" checkbox: feature compiled in, ticket unbound, still in todo.
    /// Anything already delegated, claimed, or past todo has nothing sensible for the box to do.
    pub can_hand_over: bool,
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

pub fn detail_edit(board: &Board, id: &crate::store::model::TicketId, models: Vec<String>) -> Option<DetailEditTpl> {
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
        model_placeholder: models.first().cloned().unwrap_or_default(),
        models,
        efforts: effort_options(t.effort),
        can_hand_over: cfg!(feature = "minesweeper") && t.external.is_none() && t.column.id() == ColumnId::Todo,
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
    pub minesweeper: bool,
    pub minesweeper_label: String,
    /// Comma-separated in the input.
    pub minesweeper_flag_labels: String,
    /// One entry per line in the textarea. The raw field, not the accessor: unset renders empty, and the placeholder
    /// names the alias defaults.
    pub models: String,
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
        minesweeper: config.minesweeper(),
        minesweeper_label: config.minesweeper_label.clone().unwrap_or_default(),
        minesweeper_flag_labels: config.minesweeper_flag_labels.join(", "),
        models: config.models.join("\n"),
        saved,
    }
}

// ---- docs -----------------------------------------------------------------------------------------------------------

/// The docs pane: a TOC on the left, the first entry's article primed on the right for glue.js to render. Every
/// subsequent click swaps just `#docs-content`, so this template is only rendered once per modal open.
#[derive(Debug, Template)]
#[template(path = "docs.html")]
pub struct DocsTpl {
    pub entries: Vec<DocEntryCtx>,
    pub first: Option<DocEntryCtx>,
}

#[derive(Debug, Clone)]
pub struct DocEntryCtx {
    pub file: String,
    pub title: String,
    /// `btn-active` on the leading entry so the TOC opens with an obvious current-selection.
    pub first: bool,
}

#[must_use]
pub fn docs(entries: Vec<crate::server::docs::DocEntry>) -> DocsTpl {
    let ctx: Vec<DocEntryCtx> = entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| DocEntryCtx { file: e.file, title: e.title, first: i == 0 })
        .collect();
    let first = ctx.first().cloned();
    DocsTpl { entries: ctx, first }
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
