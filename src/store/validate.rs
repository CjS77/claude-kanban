//! Whole-board invariants, enforced on every load and after every mutation — hand-edited files get exactly the same checks.
//!
//! A failing validation never reaches disk: `Store::mutate` runs it on the mutated board before writing, and a load failure
//! surfaces the full list of problems rather than the first one, because a hand-editor wants to fix everything in one pass.

use std::collections::{HashMap, HashSet, VecDeque};

use super::model::{Board, ColumnId, TicketId};

/// Check every invariant, returning all problems found (empty = valid).
///
/// - column metadata covers exactly the three known columns, once each
/// - ticket and epic ids are unique
/// - every `ticket.epic` names an existing epic
/// - every `depends_on` entry names an existing ticket, no ticket depends on itself, and the graph is acyclic
pub fn validate(board: &Board) -> Result<(), Vec<String>> {
    let problems: Vec<String> = check_columns(board)
        .chain(check_unique_ids(board))
        .chain(check_epic_refs(board))
        .chain(check_dependencies(board))
        .collect();
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

fn check_columns(board: &Board) -> impl Iterator<Item = String> + '_ {
    ColumnId::ALL.into_iter().filter_map(|id| {
        match board.columns.iter().filter(|c| c.id == id).count() {
            0 => Some(format!("columns: missing metadata for '{id}'")),
            1 => None,
            n => Some(format!("columns: '{id}' defined {n} times")),
        }
    })
}

fn check_unique_ids(board: &Board) -> impl Iterator<Item = String> + '_ {
    let dup_tickets = duplicates(board.tickets.iter().map(|t| t.id.0.as_str()));
    let dup_epics = duplicates(board.epics.iter().map(|e| e.id.0.as_str()));
    dup_tickets
        .into_iter()
        .map(|id| format!("tickets: duplicate id '{id}'"))
        .chain(dup_epics.into_iter().map(|id| format!("epics: duplicate id '{id}'")))
}

fn duplicates<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.filter(|id| !seen.insert(*id)).map(str::to_owned).collect()
}

fn check_epic_refs(board: &Board) -> impl Iterator<Item = String> + '_ {
    let epic_ids: HashSet<&str> = board.epics.iter().map(|e| e.id.0.as_str()).collect();
    board
        .tickets
        .iter()
        .filter_map(move |t| match &t.epic {
            Some(epic) if !epic_ids.contains(epic.0.as_str()) => {
                Some(format!("{}: epic '{}' does not exist", t.id, epic))
            }
            _ => None,
        })
}

/// Dangling and self references, then Kahn's algorithm for cycles. Reference errors don't hide the cycle check: unknown ids
/// simply don't contribute edges.
fn check_dependencies(board: &Board) -> impl Iterator<Item = String> + '_ {
    let ticket_ids: HashSet<&TicketId> = board.tickets.iter().map(|t| &t.id).collect();
    let mut problems: Vec<String> = board
        .tickets
        .iter()
        .flat_map(|t| t.depends_on.iter().map(move |dep| (t, dep)))
        .filter_map(|(t, dep)| {
            if dep == &t.id {
                Some(format!("{}: depends on itself", t.id))
            } else if !ticket_ids.contains(dep) {
                Some(format!("{}: depends_on '{}' does not exist", t.id, dep))
            } else {
                None
            }
        })
        .collect();
    problems.extend(find_cycle_members(board).map(|stuck| format!("depends_on cycle involving: {}", stuck.join(", "))));
    problems.into_iter()
}

/// Kahn's algorithm: peel tickets with no unresolved dependencies until none remain; anything left sits on a cycle.
/// Returns the (sorted) ids stuck on cycles, or `None` when the graph is a DAG.
fn find_cycle_members(board: &Board) -> Option<Vec<String>> {
    let known: HashSet<&TicketId> = board.tickets.iter().map(|t| &t.id).collect();
    // in_degree counts each ticket's valid dependencies; dependents is the reverse edge list.
    let mut in_degree: HashMap<&TicketId, usize> = board.tickets.iter().map(|t| (&t.id, 0)).collect();
    let mut dependents: HashMap<&TicketId, Vec<&TicketId>> = HashMap::new();
    board
        .tickets
        .iter()
        .flat_map(|t| t.depends_on.iter().map(move |dep| (&t.id, dep)))
        .filter(|(id, dep)| known.contains(dep) && dep != id)
        .for_each(|(id, dep)| {
            *in_degree.get_mut(id).expect("all ids present") += 1;
            dependents.entry(dep).or_default().push(id);
        });

    let mut queue: VecDeque<&TicketId> = in_degree.iter().filter(|&(_, &d)| d == 0).map(|(&id, _)| id).collect();
    let mut peeled = 0usize;
    while let Some(id) = queue.pop_front() {
        peeled += 1;
        for &dependent in dependents.get(id).into_iter().flatten() {
            let d = in_degree.get_mut(dependent).expect("all ids present");
            *d -= 1;
            if *d == 0 {
                queue.push_back(dependent);
            }
        }
    }

    if peeled == board.tickets.len() {
        return None;
    }
    let mut stuck: Vec<String> =
        in_degree.iter().filter(|&(_, &d)| d > 0).map(|(id, _)| id.0.clone()).collect();
    stuck.sort();
    Some(stuck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{Column, ColumnMeta, Epic, EpicId, Status, Ticket};

    fn ticket(id: &str, deps: &[&str]) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: id.into(),
            epic: None,
            status: Status::Ready,
            body: String::new(),
            labels: vec![],
            depends_on: deps.iter().map(|d| TicketId((*d).into())).collect(),
            notes: vec![],
            external: None,
            pr: None,
            column: Column::Todo,
        }
    }

    fn board_with(tickets: Vec<Ticket>) -> Board {
        Board { tickets, ..Board::empty() }
    }

    #[test]
    fn the_empty_board_is_valid() {
        assert!(validate(&Board::empty()).is_ok());
    }

    #[test]
    fn missing_column_metadata_is_rejected() {
        let mut board = Board::empty();
        board.columns.retain(|c| c.id != ColumnId::Doing);
        let problems = validate(&board).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("missing metadata for 'doing'")), "{problems:?}");
    }

    #[test]
    fn duplicate_column_metadata_is_rejected() {
        let mut board = Board::empty();
        board.columns.push(ColumnMeta { id: ColumnId::Todo, title: "Again".into() });
        assert!(validate(&board).unwrap_err().iter().any(|p| p.contains("'todo' defined 2 times")));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let board = board_with(vec![ticket("K-1", &[]), ticket("K-1", &[])]);
        assert!(validate(&board).unwrap_err().iter().any(|p| p.contains("duplicate id 'K-1'")));
    }

    #[test]
    fn dangling_epic_and_dependency_references_are_rejected() {
        let mut board = board_with(vec![ticket("K-1", &["K-99"])]);
        board.tickets[0].epic = Some(EpicId("EP-9".into()));
        let problems = validate(&board).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("epic 'EP-9' does not exist")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("depends_on 'K-99' does not exist")), "{problems:?}");
    }

    #[test]
    fn self_dependency_is_rejected() {
        let board = board_with(vec![ticket("K-1", &["K-1"])]);
        assert!(validate(&board).unwrap_err().iter().any(|p| p.contains("depends on itself")));
    }

    #[test]
    fn a_cycle_is_rejected_and_names_its_members() {
        let board = board_with(vec![ticket("K-1", &["K-2"]), ticket("K-2", &["K-3"]), ticket("K-3", &["K-1"]), ticket("K-4", &[])]);
        let problems = validate(&board).unwrap_err();
        let cycle = problems.iter().find(|p| p.contains("cycle")).expect("cycle reported");
        assert!(cycle.contains("K-1") && cycle.contains("K-2") && cycle.contains("K-3"), "{cycle}");
        assert!(!cycle.contains("K-4"), "K-4 is not on the cycle: {cycle}");
    }

    #[test]
    fn a_diamond_dependency_is_fine() {
        let board = board_with(vec![ticket("K-1", &[]), ticket("K-2", &["K-1"]), ticket("K-3", &["K-1"]), ticket("K-4", &["K-2", "K-3"])]);
        assert!(validate(&board).is_ok());
    }

    #[test]
    fn a_valid_epic_reference_passes() {
        let mut board = board_with(vec![ticket("K-1", &[])]);
        board.epics.push(Epic { id: EpicId("EP-1".into()), title: "e".into(), color: "#fff".into(), status: Status::Ready, body: String::new() });
        board.tickets[0].epic = Some(EpicId("EP-1".into()));
        assert!(validate(&board).is_ok());
    }
}
