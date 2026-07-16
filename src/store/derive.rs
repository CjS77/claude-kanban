//! Read-side derivations: everything the board *means* but deliberately does not *store*.
//!
//! Epics store no column — their place on the board is a pure function of their tickets, computed here on every read so it
//! can never disagree with them. Likewise a ticket's blocked-ness, its live claim, and "the next thing to work on" are all
//! derived views over `(Board, claims)`. [`board_view`] assembles the whole read model that both the HTML views and the
//! `kanban_board` MCP tool serialize, so the two faces can never drift apart.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    claims::{self, Claim},
    model::{Board, Column, ColumnId, ColumnMeta, Epic, EpicId, Ticket, TicketId},
    model::Status,
};

/// An epic's derived column: `done` iff every one of its tickets is done (and it has at least one), `doing` once any ticket
/// has reached `doing` or `done`, `todo` otherwise. An epic with no tickets is `todo` — nothing has started.
#[must_use] 
pub fn epic_column(epic: &EpicId, board: &Board) -> ColumnId {
    let mut any = false;
    let mut all_done = true;
    let mut any_started = false;
    board
        .tickets
        .iter()
        .filter(|t| t.epic.as_ref() == Some(epic))
        .for_each(|t| {
            any = true;
            match t.column {
                Column::Done { .. } => any_started = true,
                Column::Doing { .. } => {
                    any_started = true;
                    all_done = false;
                }
                Column::Todo => all_done = false,
            }
        });
    match (any, all_done, any_started) {
        (true, true, _) => ColumnId::Done,
        (true, false, true) => ColumnId::Doing,
        // No tickets at all, or none started: nothing has begun, the epic sits in todo.
        (false, ..) | (true, false, false) => ColumnId::Todo,
    }
}

/// A ticket is blocked while any of its dependencies is not yet `done`. Dangling dependency ids count as blocking, though
/// validation refuses to load or write a board that has any.
#[must_use] 
pub fn blocked(ticket: &Ticket, board: &Board) -> bool {
    ticket
        .depends_on
        .iter()
        .any(|dep| !matches!(board.ticket(dep).map(|t| &t.column), Some(Column::Done { .. })))
}

/// The handoff contract: the highest ticket in `todo` that is `ready`, unblocked, unclaimed, and not `external` (external
/// tickets are worked elsewhere). `None` when nothing is eligible.
#[must_use] 
pub fn next_ticket<'a>(board: &'a Board, claims: &[Claim]) -> Option<&'a Ticket> {
    board
        .tickets_in(ColumnId::Todo)
        .find(|t| t.status == Status::Ready && t.external.is_none() && !blocked(t, board) && claims::find(claims, &t.id).is_none())
}

/// The full read model: the board joined with its live claims and every derived fact, ready to serialize. Built fresh on
/// every read — nothing here is ever stored.
#[must_use] 
pub fn board_view(board: &Board, claims: &[Claim]) -> BoardView {
    let tickets: Vec<TicketView> = board
        .tickets
        .iter()
        .map(|t| TicketView {
            ticket: t.clone(),
            blocked: blocked(t, board),
            claim: claims::find(claims, &t.id).map(ClaimView::from),
        })
        .collect();
    let epics = board
        .epics
        .iter()
        .map(|e| EpicView {
            epic: e.clone(),
            column: epic_column(&e.id, board),
            tickets: board
                .tickets
                .iter()
                .filter(|t| t.epic.as_ref() == Some(&e.id))
                .map(|t| ChecklistItem { ticket: t.id.clone(), title: t.title.clone(), done: matches!(t.column, Column::Done { .. }) })
                .collect(),
        })
        .collect();
    BoardView { version: board.version, columns: board.columns.clone(), tickets, epics }
}

/// Everything a consumer of the board sees: stored state plus derived facts. Serialized as-is by `kanban_board` and walked
/// by the HTML views.
#[derive(Debug, Clone, Serialize)]
pub struct BoardView {
    pub version: u64,
    pub columns: Vec<ColumnMeta>,
    pub tickets: Vec<TicketView>,
    pub epics: Vec<EpicView>,
}

/// A ticket plus its derived facts. The ticket's own fields are flattened to the top level on serialization.
#[derive(Debug, Clone, Serialize)]
pub struct TicketView {
    #[serde(flatten)]
    pub ticket: Ticket,
    /// True while any dependency is not yet `done`. Blocked tickets stay visible in `todo` but are skipped by `kanban_next`.
    pub blocked: bool,
    /// The live claim, when someone is working this right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimView>,
}

/// A live claim as consumers see it, including whether its worktree has vanished (e.g. a /tmp wipe) — "worktree missing"
/// must read as *restorable*, not as live work.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimView {
    pub agent: String,
    pub since: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub worktree_missing: bool,
}

impl From<&Claim> for ClaimView {
    fn from(c: &Claim) -> ClaimView {
        ClaimView {
            agent: c.agent.clone(),
            since: c.since,
            path: c.path.clone(),
            worktree_missing: c.path.as_ref().is_some_and(|p| !p.exists()),
        }
    }
}

/// An epic plus its derived column and checklist. The epic's own fields are flattened to the top level on serialization.
#[derive(Debug, Clone, Serialize)]
pub struct EpicView {
    #[serde(flatten)]
    pub epic: Epic,
    /// Derived from the epic's tickets — never stored, so it can never disagree with them.
    pub column: ColumnId,
    /// One line per ticket, ticked when done: the epic's on-board rendering.
    pub tickets: Vec<ChecklistItem>,
}

/// One line of an epic's checklist.
#[derive(Debug, Clone, Serialize)]
pub struct ChecklistItem {
    pub ticket: TicketId,
    pub title: String,
    pub done: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(id: &str, column: Column) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: id.into(),
            epic: Some(EpicId("EP-1".into())),
            status: Status::Ready,
            body: String::new(),
            labels: vec![],
            depends_on: vec![],
            notes: vec![],
            external: None,
            column,
        }
    }

    fn done() -> Column {
        Column::Done { branch: None, completed_at: Utc::now() }
    }

    fn doing() -> Column {
        Column::Doing { owner: "claude".into(), branch: None }
    }

    fn board(tickets: Vec<Ticket>) -> Board {
        let mut b = Board::empty();
        b.epics.push(Epic { id: EpicId("EP-1".into()), title: "e".into(), color: "#fff".into(), status: Status::Ready, body: String::new() });
        b.tickets = tickets;
        b
    }

    #[test]
    fn epic_column_follows_its_tickets() {
        let ep = EpicId("EP-1".into());
        assert_eq!(epic_column(&ep, &board(vec![])), ColumnId::Todo, "empty epic is todo");
        assert_eq!(epic_column(&ep, &board(vec![ticket("K-1", Column::Todo)])), ColumnId::Todo);
        assert_eq!(epic_column(&ep, &board(vec![ticket("K-1", doing()), ticket("K-2", Column::Todo)])), ColumnId::Doing);
        assert_eq!(epic_column(&ep, &board(vec![ticket("K-1", done()), ticket("K-2", Column::Todo)])), ColumnId::Doing, "one done + one todo = in progress");
        assert_eq!(epic_column(&ep, &board(vec![ticket("K-1", done()), ticket("K-2", done())])), ColumnId::Done);
    }

    #[test]
    fn blocked_tracks_dependency_completion() {
        let mut b = board(vec![ticket("K-1", Column::Todo), ticket("K-2", Column::Todo)]);
        b.tickets[1].depends_on = vec![TicketId("K-1".into())];
        assert!(blocked(&b.tickets[1], &b));
        b.tickets[0].column = done();
        assert!(!blocked(&b.tickets[1], &b));
    }

    #[test]
    fn next_ticket_takes_the_highest_eligible_and_skips_the_rest() {
        let mut b = board(vec![
            ticket("K-1", Column::Todo), // status stub → skipped
            ticket("K-2", Column::Todo), // blocked → skipped
            ticket("K-3", Column::Todo), // claimed → skipped
            ticket("K-4", Column::Todo), // external → skipped
            ticket("K-5", Column::Todo), // eligible — but lower than all of the above
        ]);
        b.tickets[0].status = Status::Stub;
        b.tickets[1].depends_on = vec![TicketId("K-1".into())];
        b.tickets[3].external = Some(crate::store::model::External { provider: "github".into(), kind: "issue".into(), number: 1 });
        let claims = vec![Claim { ticket: TicketId("K-3".into()), agent: "claude".into(), since: Utc::now(), path: None }];
        assert_eq!(next_ticket(&b, &claims).unwrap().id.0, "K-5");
    }

    #[test]
    fn next_ticket_respects_array_order() {
        let b = board(vec![ticket("K-9", Column::Todo), ticket("K-1", Column::Todo)]);
        assert_eq!(next_ticket(&b, &[]).unwrap().id.0, "K-9", "top of the column wins, not lowest id");
    }

    #[test]
    fn board_view_joins_claims_and_flags_missing_worktrees() {
        let b = board(vec![ticket("K-1", doing())]);
        let claims = vec![Claim {
            ticket: TicketId("K-1".into()),
            agent: "claude".into(),
            since: Utc::now(),
            path: Some(PathBuf::from("/nonexistent/worktree/K-1")),
        }];
        let view = board_view(&b, &claims);
        let claim = view.tickets[0].claim.as_ref().unwrap();
        assert!(claim.worktree_missing, "a vanished worktree must read as missing, not as live work");

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["tickets"][0]["id"], "K-1", "ticket fields flatten to the top level");
        assert_eq!(json["tickets"][0]["claim"]["agent"], "claude");
        assert_eq!(json["epics"][0]["column"], "doing", "derived epic column is serialized");
    }
}
