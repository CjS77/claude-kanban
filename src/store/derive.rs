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

/// An epic's derived column: `done` iff every one of its tickets is done (and it has at least one); `review` when every
/// ticket is at least code-complete (review or done) but not all have landed; `doing` once any ticket has reached
/// `doing` or beyond; `todo` otherwise. An epic with no tickets is `todo` — nothing has started.
#[must_use]
pub fn epic_column(epic: &EpicId, board: &Board) -> ColumnId {
    let (mut total, mut done, mut settled, mut started) = (0usize, 0usize, 0usize, 0usize);
    board.tickets.iter().filter(|t| t.epic.as_ref() == Some(epic)).for_each(|t| {
        total += 1;
        match t.column {
            Column::Done { .. } => {
                done += 1;
                settled += 1;
                started += 1;
            }
            Column::Review { .. } => {
                settled += 1;
                started += 1;
            }
            Column::Doing { .. } => started += 1,
            Column::Todo => {}
        }
    });
    if total > 0 && done == total {
        ColumnId::Done
    } else if total > 0 && settled == total {
        ColumnId::Review
    } else if started > 0 {
        ColumnId::Doing
    } else {
        // No tickets at all, or none started: nothing has begun, the epic sits in todo.
        ColumnId::Todo
    }
}

/// A ticket is blocked while any of its dependencies is not yet `done` — and done means *landed*: a discarded
/// dependency never unblocks its dependents, because the code they were promised does not exist. Dangling dependency
/// ids count as blocking, though validation refuses to load or write a board that has any.
#[must_use]
pub fn blocked(ticket: &Ticket, board: &Board) -> bool {
    ticket
        .depends_on
        .iter()
        .any(|dep| !matches!(board.ticket(dep).map(|t| &t.column), Some(Column::Done { discarded: false, .. })))
}

/// Whether this ticket may land without a human seeing the merge: its own flag, or its epic's. Inheritance lives here
/// rather than on the ticket so an epic's dial keeps working in both directions — nothing is copied onto the tickets, so
/// clearing the epic's flag takes the permission back from every one of them.
///
/// A missing epic degrades to the ticket's own answer. Validation refuses a dangling epic reference, so that can only be
/// a board caught mid-edit — and the safe reading of "I can't tell" is the narrower permission.
#[must_use]
pub fn auto_merge(ticket: &Ticket, board: &Board) -> bool {
    ticket.auto_merge || ticket.epic.as_ref().and_then(|id| board.epic(id)).is_some_and(|e| e.auto_merge)
}

/// The handoff contract: the highest ticket in `todo` that is unblocked, unclaimed, not `external` (external tickets are
/// worked elsewhere), and either `ready` (implement it) or `stub` (refine it into a spec). `None` when nothing is eligible.
#[must_use]
pub fn next_ticket<'a>(board: &'a Board, claims: &[Claim]) -> Option<&'a Ticket> {
    board
        .tickets_in(ColumnId::Todo)
        .find(|t| matches!(t.status, Status::Ready | Status::Stub) && t.external.is_none() && !blocked(t, board) && claims::find(claims, &t.id).is_none())
}

/// The complement of [`next_ticket`]: every todo ticket that is *not* eligible, and why. For when `next_ticket` answers
/// `None` and somebody has to explain the silence — an empty todo and a todo full of blocked work are the same answer
/// otherwise, and one of them means the board is stuck.
#[must_use]
pub fn ineligible(board: &Board, claims: &[Claim]) -> Vec<(TicketId, String)> {
    board.tickets_in(ColumnId::Todo).filter_map(|t| ineligibility(t, board, claims).map(|why| (t.id.clone(), why))).collect()
}

/// Why this todo ticket is not the next thing to work on; `None` when it is eligible. The order mirrors
/// [`next_ticket`]'s own filter, so the reason given is the first test the ticket actually failed.
fn ineligibility(ticket: &Ticket, board: &Board, claims: &[Claim]) -> Option<String> {
    match ticket.status {
        Status::Draft => return Some("draft — the human's to shape, never an agent's".to_owned()),
        Status::Review => return Some("status review — waiting to be promoted to ready".to_owned()),
        Status::Ready | Status::Stub => {}
    }
    if ticket.external.is_some() {
        return Some("external — worked by a delegate, not from here".to_owned());
    }
    if blocked(ticket, board) {
        let unmet: Vec<&str> = ticket
            .depends_on
            .iter()
            .filter(|dep| !matches!(board.ticket(dep).map(|t| &t.column), Some(Column::Done { discarded: false, .. })))
            .map(|dep| dep.0.as_str())
            .collect();
        return Some(format!("blocked by {}", unmet.join(", ")));
    }
    claims::find(claims, &ticket.id).map(|c| format!("claimed by {}", c.agent))
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
            auto_merge_effective: auto_merge(t, board),
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
    /// The ticket's own `auto_merge` or its epic's — see [`auto_merge`]. Named apart from the stored field on purpose:
    /// `ticket` is flattened, so a sibling called `auto_merge` would emit the key twice whenever the ticket's own flag
    /// is set.
    pub auto_merge_effective: bool,
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
            model: None,
            effort: None,
            auto_merge: false,
            depends_on: vec![],
            notes: vec![],
            external: None,
            pr: None,
            column,
        }
    }

    fn done() -> Column {
        Column::Done { branch: None, completed_at: Utc::now(), discarded: false }
    }

    fn doing() -> Column {
        Column::Doing { owner: "claude".into(), branch: None }
    }

    fn board(tickets: Vec<Ticket>) -> Board {
        let mut b = Board::empty();
        b.epics.push(Epic { id: EpicId("EP-1".into()), title: "e".into(), color: "#fff".into(), status: Status::Ready, body: String::new(), auto_merge: false });
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
            ticket("K-1", Column::Todo), // status draft → skipped
            ticket("K-2", Column::Todo), // blocked → skipped
            ticket("K-3", Column::Todo), // claimed → skipped
            ticket("K-4", Column::Todo), // external → skipped
            ticket("K-5", Column::Todo), // eligible — but lower than all of the above
        ]);
        b.tickets[0].status = Status::Draft;
        b.tickets[1].depends_on = vec![TicketId("K-1".into())];
        b.tickets[3].external = Some(crate::store::model::External { provider: "github".into(), kind: "issue".into(), number: 1 });
        let claims = vec![Claim { ticket: TicketId("K-3".into()), agent: "claude".into(), since: Utc::now(), path: None }];
        assert_eq!(next_ticket(&b, &claims).unwrap().id.0, "K-5");
    }

    /// The same board `next_ticket` walks, asked the opposite question: not "what may I work?" but "why may I work
    /// nothing?". Every skip it makes silently has to have a sentence here, or an idling loop can only guess.
    #[test]
    fn ineligible_names_the_first_test_each_todo_ticket_failed() {
        let mut b = board(vec![
            ticket("K-1", Column::Todo), // draft
            ticket("K-2", Column::Todo), // blocked by K-1 and K-6
            ticket("K-3", Column::Todo), // claimed
            ticket("K-4", Column::Todo), // external
            ticket("K-5", Column::Todo), // eligible
            ticket("K-6", Column::Todo), // status review
        ]);
        b.tickets[0].status = Status::Draft;
        b.tickets[1].depends_on = vec![TicketId("K-1".into()), TicketId("K-6".into())];
        b.tickets[3].external = Some(crate::store::model::External { provider: "github".into(), kind: "issue".into(), number: 1 });
        b.tickets[5].status = Status::Review;
        let claims = vec![Claim { ticket: TicketId("K-3".into()), agent: "claude".into(), since: Utc::now(), path: None }];

        let why: Vec<(String, String)> = ineligible(&b, &claims).into_iter().map(|(id, why)| (id.0, why)).collect();
        assert_eq!(why.len(), 5, "only the eligible K-5 is absent: {why:?}");
        assert!(why[0].1.starts_with("draft"), "{why:?}");
        assert_eq!(why[1].1, "blocked by K-1, K-6", "both unmet dependencies, named");
        assert!(why[2].1 == "claimed by claude", "{why:?}");
        assert!(why[3].1.starts_with("external"), "{why:?}");
        assert!(why[4].1.starts_with("status review"), "{why:?}");
        assert!(!why.iter().any(|(id, _)| id == "K-5"));

        // A ticket whose dependency landed drops out; one whose dependency was *discarded* does not — the code it was
        // promised does not exist, and saying so is the only way anyone learns why the board stopped.
        b.tickets[0].status = Status::Ready;
        b.tickets[0].column = done();
        b.tickets[5].status = Status::Ready;
        b.tickets[5].column = Column::Done { branch: None, completed_at: Utc::now(), discarded: true };
        let why = ineligible(&b, &claims);
        assert_eq!(why.iter().find(|(id, _)| id.0 == "K-2").unwrap().1, "blocked by K-6");
    }

    #[test]
    fn next_ticket_offers_stubs_for_refinement() {
        let mut b = board(vec![ticket("K-1", Column::Todo), ticket("K-2", Column::Todo)]);
        b.tickets[0].status = Status::Stub;
        assert_eq!(next_ticket(&b, &[]).unwrap().id.0, "K-1", "a stub above a ready ticket wins — position is priority");
    }

    #[test]
    fn next_ticket_respects_array_order() {
        let b = board(vec![ticket("K-9", Column::Todo), ticket("K-1", Column::Todo)]);
        assert_eq!(next_ticket(&b, &[]).unwrap().id.0, "K-9", "top of the column wins, not lowest id");
    }

    /// Either flag grants it, and neither is required — the full truth table, since this decides whether main moves
    /// without a human seeing it.
    #[test]
    fn auto_merge_is_the_ticket_flag_or_its_epics() {
        let cases = [(false, false, false), (true, false, true), (false, true, true), (true, true, true)];
        let got: Vec<bool> = cases
            .iter()
            .map(|&(on_ticket, on_epic, _)| {
                let mut b = board(vec![ticket("K-1", Column::Todo)]);
                b.tickets[0].auto_merge = on_ticket;
                b.epics[0].auto_merge = on_epic;
                auto_merge(&b.tickets[0], &b)
            })
            .collect();
        assert_eq!(got, cases.iter().map(|&(_, _, want)| want).collect::<Vec<_>>(), "ticket OR epic, and nothing else");
    }

    #[test]
    fn auto_merge_without_an_epic_is_the_tickets_own_answer() {
        let mut b = board(vec![ticket("K-1", Column::Todo)]);
        b.tickets[0].epic = None;
        b.epics[0].auto_merge = true; // an epic the ticket does not belong to grants it nothing
        assert!(!auto_merge(&b.tickets[0], &b));
        b.tickets[0].auto_merge = true;
        assert!(auto_merge(&b.tickets[0], &b));
    }

    /// The inheritance is a read, never a write. If the epic's flag were copied onto its tickets, clearing it would
    /// leave every ticket still armed — the failure mode this test exists to prevent.
    #[test]
    fn an_epics_flag_is_never_written_onto_its_tickets() {
        let mut b = board(vec![ticket("K-1", Column::Todo)]);
        b.epics[0].auto_merge = true;
        let id = TicketId("K-1".into());

        assert!(auto_merge(b.ticket(&id).unwrap(), &b), "the effective answer follows the epic");
        assert!(!b.ticket(&id).unwrap().auto_merge, "but the stored flag is untouched");
        assert!(board_view(&b, &[]).tickets[0].auto_merge_effective, "and the view carries the derived value");

        b.epics[0].auto_merge = false;
        assert!(!auto_merge(b.ticket(&id).unwrap(), &b), "so clearing the epic takes the permission back from the ticket");
    }

    /// `TicketView.ticket` is flattened, so the derived field has to be named apart from the stored one or the object
    /// carries `auto_merge` twice — with whichever value serde happens to emit last.
    #[test]
    fn the_derived_flag_does_not_collide_with_the_flattened_stored_one() {
        let mut b = board(vec![ticket("K-1", Column::Todo)]);
        b.tickets[0].auto_merge = true;
        let json = serde_json::to_value(board_view(&b, &[])).unwrap();
        assert_eq!(json["tickets"][0]["auto_merge"], true, "the stored flag flattens up as itself");
        assert_eq!(json["tickets"][0]["auto_merge_effective"], true, "and the derived one sits beside it under its own name");
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
