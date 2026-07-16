//! The board's wire format: serde shapes that match the JSON in `.kanban/board.json` exactly.
//!
//! The example board in design.md is the contract, and a round-trip test in this module pins it. Two properties of the shape are
//! load-bearing and worth restating here:
//!
//! - [`Column`] is an internally-tagged enum on `"id"`, so a ticket structurally cannot sit in one column while carrying another
//!   column's fields (`doing` has an `owner`, `done` has a `completed_at`, `todo` has nothing).
//! - Priority is the order of [`Board::tickets`]. Among tickets sharing a column, earlier in the array means higher on the board.
//!   There are no rank numbers to rebalance, and the file stays hand-editable.
//!
//! Parsing is deliberately lenient — no `deny_unknown_fields` — because the file is meant to survive hand edits. The trade-off
//! (accepted for v1) is that unknown fields are dropped on the next programmatic rewrite.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The whole of `.kanban/board.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// Optimistic-concurrency counter: bumped on every write; a write whose expected version no longer matches is rejected.
    pub version: u64,
    /// Column metadata only (title etc.) — never membership lists, which could drift out of sync with the tickets.
    pub columns: Vec<ColumnMeta>,
    pub epics: Vec<Epic>,
    /// Array order is priority: among tickets sharing a column, earlier means higher on the board.
    pub tickets: Vec<Ticket>,
}

/// Metadata for one of the three fixed columns. Membership is never stored here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub id: ColumnId,
    pub title: String,
}

/// The three workflow states, in board order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    Todo,
    Doing,
    Done,
}

impl ColumnId {
    /// All columns in board order — the canonical iteration order for rendering and validation.
    pub const ALL: [ColumnId; 3] = [ColumnId::Todo, ColumnId::Doing, ColumnId::Done];

    /// The wire name (`todo` / `doing` / `done`).
    #[must_use] 
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnId::Todo => "todo",
            ColumnId::Doing => "doing",
            ColumnId::Done => "done",
        }
    }
}

impl fmt::Display for ColumnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ColumnId {
    type Err = String;

    fn from_str(s: &str) -> Result<ColumnId, String> {
        match s {
            "todo" => Ok(ColumnId::Todo),
            "doing" => Ok(ColumnId::Doing),
            "done" => Ok(ColumnId::Done),
            other => Err(format!("'{other}' is not a column (todo/doing/done)")),
        }
    }
}

/// A ticket id, e.g. `K-7`. Transparent newtype: serializes as a bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TicketId(pub String);

/// An epic id, e.g. `EP-2`. Transparent newtype: serializes as a bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpicId(pub String);

impl fmt::Display for TicketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EpicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How well-defined a ticket or epic is. Orthogonal to the column: the column is where the work sits in the workflow, `status`
/// is whether the work is defined enough to do at all. Only `ready` tickets are eligible for pickup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Still being defined by the user. Ignored from a work point of view.
    Draft,
    /// A rough outline the user wants fleshed out (see `kanban_refine`).
    Stub,
    /// Fleshed out and awaiting the user's verdict: promote to `ready` or push back to `stub`.
    Review,
    /// Fully specified and ready to be picked up and implemented.
    Ready,
}

impl Status {
    /// The wire name (`draft` / `stub` / `review` / `ready`).
    #[must_use] 
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Draft => "draft",
            Status::Stub => "stub",
            Status::Review => "review",
            Status::Ready => "ready",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Status, String> {
        match s {
            "draft" => Ok(Status::Draft),
            "stub" => Ok(Status::Stub),
            "review" => Ok(Status::Review),
            "ready" => Ok(Status::Ready),
            other => Err(format!("'{other}' is not a status (draft/stub/review/ready)")),
        }
    }
}

/// A unit of work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    /// The epic this ticket belongs to, giving its card its colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicId>,
    pub status: Status,
    /// Markdown body — the spec of the work. Rendered client-side; stored raw.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Until every ticket named here is `done`, this ticket is *blocked*: visible in `todo`, skipped by `kanban_next`.
    /// Must form a DAG with the other tickets; checked on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TicketId>,
    /// Progress log, appended to by `kanban_note`. Newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// Binding to a work item in another system (e.g. a GitHub issue a delegate daemon works). External tickets are never
    /// given worktrees or branches by this binary; the binding is just an address for other tools to act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<External>,
    /// Workflow state and that state's data, as one tagged object.
    pub column: Column,
}

/// A ticket's workflow state. Internally tagged on `"id"` so each state carries exactly its own fields:
/// `{"id":"todo"}`, `{"id":"doing","owner":…,"branch":…}`, `{"id":"done","branch":…,"completed_at":…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "id", rename_all = "snake_case")]
pub enum Column {
    /// Ready to be worked (or blocked and waiting). Carries nothing extra.
    Todo,
    /// Claimed and in progress.
    Doing {
        /// Who is working the ticket — an agent name like `claude`, or a human.
        owner: String,
        /// The ticket's branch, filled in by `worktree start`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// Finished.
    Done {
        /// The branch the work landed on, carried over from `doing`. Data, not a format: an external delegate's branch name
        /// is recorded verbatim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        completed_at: DateTime<Utc>,
    },
}

impl Column {
    /// The workflow state this column value sits in, without its data.
    #[must_use] 
    pub fn id(&self) -> ColumnId {
        match self {
            Column::Todo => ColumnId::Todo,
            Column::Doing { .. } => ColumnId::Doing,
            Column::Done { .. } => ColumnId::Done,
        }
    }

    /// The branch recorded on this column state, if any.
    #[must_use] 
    pub fn branch(&self) -> Option<&str> {
        match self {
            Column::Todo => None,
            Column::Doing { branch, .. } | Column::Done { branch, .. } => branch.as_deref(),
        }
    }
}

/// A binding to a work item in another system: `{provider, kind, number}`, e.g. GitHub issue 42.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct External {
    pub provider: String,
    pub kind: String,
    pub number: u64,
}

/// One entry in a ticket's progress log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub at: DateTime<Utc>,
    /// Who wrote it: an agent name like `claude`, or `user`. Absent for hand-added notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub text: String,
}

/// A group of tickets. Epics are meta-tasks, not work: nobody claims one, and an epic stores no column — its place on the
/// board is derived from its tickets on read (see [`crate::store::derive::epic_column`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epic {
    pub id: EpicId,
    pub title: String,
    /// The colour its tickets' cards carry, as a CSS hex colour like `#7c9cf5`.
    pub color: String,
    pub status: Status,
    /// Markdown body. Rendered client-side; stored raw.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

impl Board {
    /// An empty board with the three classic columns, as `init` seeds it.
    #[must_use] 
    pub fn empty() -> Board {
        Board {
            version: 0,
            columns: vec![
                ColumnMeta { id: ColumnId::Todo, title: "To do".into() },
                ColumnMeta { id: ColumnId::Doing, title: "Doing".into() },
                ColumnMeta { id: ColumnId::Done, title: "Done".into() },
            ],
            epics: Vec::new(),
            tickets: Vec::new(),
        }
    }

    #[must_use] 
    pub fn ticket(&self, id: &TicketId) -> Option<&Ticket> {
        self.tickets.iter().find(|t| &t.id == id)
    }

    pub fn ticket_mut(&mut self, id: &TicketId) -> Option<&mut Ticket> {
        self.tickets.iter_mut().find(|t| &t.id == id)
    }

    #[must_use] 
    pub fn epic(&self, id: &EpicId) -> Option<&Epic> {
        self.epics.iter().find(|e| &e.id == id)
    }

    pub fn epic_mut(&mut self, id: &EpicId) -> Option<&mut Epic> {
        self.epics.iter_mut().find(|e| &e.id == id)
    }

    /// Tickets sitting in `column`, in priority order.
    pub fn tickets_in(&self, column: ColumnId) -> impl Iterator<Item = &Ticket> {
        self.tickets.iter().filter(move |t| t.column.id() == column)
    }

    /// The next unused ticket id: `K-<n>` for the smallest `n` above every existing numeric suffix (min `K-1`).
    #[must_use] 
    pub fn next_ticket_id(&self) -> TicketId {
        TicketId(format!("K-{}", next_numeric_suffix(self.tickets.iter().map(|t| t.id.0.as_str()), "K-")))
    }

    /// The next unused epic id: `EP-<n>`, by the same rule as [`Board::next_ticket_id`].
    #[must_use] 
    pub fn next_epic_id(&self) -> EpicId {
        EpicId(format!("EP-{}", next_numeric_suffix(self.epics.iter().map(|e| e.id.0.as_str()), "EP-")))
    }
}

/// Max numeric suffix among `ids` that match `<prefix><digits>`, plus one. Ids that don't match the scheme are ignored, so a
/// hand-named ticket can't wedge id minting.
fn next_numeric_suffix<'a>(ids: impl Iterator<Item = &'a str>, prefix: &str) -> u64 {
    ids.filter_map(|id| id.strip_prefix(prefix)).filter_map(|n| n.parse::<u64>().ok()).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal example board from design.md. This test pins the wire format: if the model drifts from design.md, one of
    /// the two is wrong and the build should say so.
    const DESIGN_MD_BOARD: &str = r##"{
  "version": 12,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "done", "title": "Done" }
  ],
  "epics": [
    { "id": "EP-1", "title": "Auth", "color": "#7c9cf5", "status": "ready" }
  ],
  "tickets": [
    {
      "id": "K-1",
      "title": "Add session refresh",
      "epic": "EP-1",
      "status": "ready",
      "column": { "id": "doing", "owner": "claude", "branch": "k-1/session-refresh" }
    },
    {
      "id": "K-3",
      "title": "Password reset flow",
      "epic": "EP-1",
      "status": "ready",
      "external": { "provider": "github", "kind": "issue", "number": 42 },
      "column": { "id": "done", "branch": "myrepo-issue0042", "completed_at": "2026-07-14T09:12:00Z" }
    },
    {
      "id": "K-2",
      "title": "Rate-limit the login route",
      "epic": "EP-1",
      "status": "stub",
      "depends_on": ["K-1"],
      "column": { "id": "todo" }
    }
  ]
}"##;

    #[test]
    fn design_md_example_round_trips() {
        let board: Board = serde_json::from_str(DESIGN_MD_BOARD).expect("design.md example must parse");
        let reserialized = serde_json::to_value(&board).expect("board must serialize");
        let original: serde_json::Value = serde_json::from_str(DESIGN_MD_BOARD).unwrap();
        assert_eq!(reserialized, original, "re-serializing the design.md example must not add, drop, or alter fields");
    }

    #[test]
    fn readme_example_parses_into_the_right_shapes() {
        let board: Board = serde_json::from_str(DESIGN_MD_BOARD).unwrap();
        assert_eq!(board.version, 12);
        assert_eq!(board.columns.len(), 3);
        let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
        match &k1.column {
            Column::Doing { owner, branch } => {
                assert_eq!(owner, "claude");
                assert_eq!(branch.as_deref(), Some("k-1/session-refresh"));
            }
            other => panic!("K-1 should be doing, got {other:?}"),
        }
        let k3 = board.ticket(&TicketId("K-3".into())).unwrap();
        assert_eq!(k3.external.as_ref().unwrap().number, 42);
        assert!(matches!(k3.column, Column::Done { .. }));
        let k2 = board.ticket(&TicketId("K-2".into())).unwrap();
        assert_eq!(k2.depends_on, vec![TicketId("K-1".into())]);
        assert!(matches!(k2.column, Column::Todo));
    }

    #[test]
    fn doing_without_owner_fails_to_parse() {
        let bad = r#"{ "id": "K-1", "title": "x", "status": "ready", "column": { "id": "doing" } }"#;
        assert!(serde_json::from_str::<Ticket>(bad).is_err(), "doing requires an owner");
    }

    #[test]
    fn done_requires_completed_at() {
        let bad = r#"{ "id": "K-1", "title": "x", "status": "ready", "column": { "id": "done" } }"#;
        assert!(serde_json::from_str::<Ticket>(bad).is_err(), "done requires completed_at");
    }

    #[test]
    fn todo_cannot_carry_an_owner_column_state() {
        // The tagged enum makes "in todo but owned" structurally inexpressible; unknown fields inside the tagged object are
        // tolerated on parse (hand-editability) but the *state* is todo, with no data.
        let t: Ticket =
            serde_json::from_str(r#"{ "id": "K-1", "title": "x", "status": "ready", "column": { "id": "todo" } }"#).unwrap();
        assert!(matches!(t.column, Column::Todo));
    }

    #[test]
    fn id_minting_skips_nonconforming_ids_and_starts_at_one() {
        let mut board = Board::empty();
        assert_eq!(board.next_ticket_id().0, "K-1");
        assert_eq!(board.next_epic_id().0, "EP-1");
        board.tickets = vec!["K-2", "K-9", "custom-name", "K-x"]
            .into_iter()
            .map(|id| Ticket {
                id: TicketId(id.into()),
                title: "t".into(),
                epic: None,
                status: Status::Ready,
                body: String::new(),
                labels: vec![],
                depends_on: vec![],
                notes: vec![],
                external: None,
                column: Column::Todo,
            })
            .collect();
        assert_eq!(board.next_ticket_id().0, "K-10");
    }

    #[test]
    fn completed_at_serializes_as_rfc3339_z() {
        // Pins the chrono serde format against design.md's "2026-07-14T09:12:00Z" (internally-tagged enums buffer content
        // through serde's Content type — this also guards that path).
        let col = Column::Done { branch: None, completed_at: "2026-07-14T09:12:00Z".parse().unwrap() };
        let v = serde_json::to_value(&col).unwrap();
        assert_eq!(v["completed_at"], "2026-07-14T09:12:00Z");
    }
}
