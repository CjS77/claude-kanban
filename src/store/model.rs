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

/// The board schema this binary reads and writes. Schema 1 (implicit — the field was absent) had three columns; schema 2
/// added `review` between `doing` and `done`, per-ticket `pr` bindings, and the `discarded` flag on done.
pub const CURRENT_SCHEMA: u32 = 2;

/// What an absent `schema` field means: a board written before the field existed.
fn schema_v1() -> u32 {
    1
}

// serde's skip_serializing_if demands fn(&T) — the reference is the contract, not a choice.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// The whole of `.kanban/board.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// Board format version, distinct from `version` below: bumped only when the *shape* changes. Absent means 1.
    /// A schema newer than [`CURRENT_SCHEMA`] refuses to load — see [`migrate`].
    #[serde(default = "schema_v1")]
    pub schema: u32,
    /// Optimistic-concurrency counter: bumped on every write; a write whose expected version no longer matches is rejected.
    pub version: u64,
    /// The number the next ticket id takes: `K-<next_ticket_seq>`. Monotonic — it climbs as ids are minted and never
    /// falls back when a ticket is deleted, so a number is issued exactly once in a board's life. That matters because a
    /// deleted ticket outlives its card: its `k-<n>/<slug>` branch, its worktree and any PR opened from it all still name
    /// the number, and the landing sweep matches review tickets by branch. Absent means "derive it from the tickets" — see
    /// [`Board::reconcile_id_seqs`], which also floors it above every id in use.
    #[serde(default)]
    pub next_ticket_seq: u64,
    /// The number the next epic id takes: `EP-<next_epic_seq>`, monotonic by the same rule.
    #[serde(default)]
    pub next_epic_seq: u64,
    /// Column metadata only (title etc.) — never membership lists, which could drift out of sync with the tickets.
    pub columns: Vec<ColumnMeta>,
    pub epics: Vec<Epic>,
    /// Array order is priority: among tickets sharing a column, earlier means higher on the board.
    pub tickets: Vec<Ticket>,
}

/// Metadata for one of the four fixed columns. Membership is never stored here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub id: ColumnId,
    pub title: String,
}

/// The four workflow states, in board order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    Todo,
    Doing,
    Review,
    Done,
}

impl ColumnId {
    /// All columns in board order — the canonical iteration order for rendering and validation.
    pub const ALL: [ColumnId; 4] = [ColumnId::Todo, ColumnId::Doing, ColumnId::Review, ColumnId::Done];

    /// The wire name (`todo` / `doing` / `review` / `done`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnId::Todo => "todo",
            ColumnId::Doing => "doing",
            ColumnId::Review => "review",
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
            "review" => Ok(ColumnId::Review),
            "done" => Ok(ColumnId::Done),
            other => Err(format!("'{other}' is not a column (todo/doing/review/done)")),
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

/// How much reasoning effort a ticket's work deserves. The five levels the harness accepts; which of them a given model
/// actually supports is the harness's business, not the board's — this is a preference, not a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// Every level, highest-intent last — the order the UI offers them in.
    pub const ALL: [Effort; 5] = [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

    /// The wire name (`low` / `medium` / `high` / `xhigh` / `max`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Effort, String> {
        match s {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::Xhigh),
            "max" => Ok(Effort::Max),
            other => Err(format!("'{other}' is not an effort level (low/medium/high/xhigh/max)")),
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
    /// The model this ticket's work should run on — an alias (`opus`) or a full id (`claude-opus-4-8`), whatever the
    /// harness's `--model` accepts. Advisory: this binary launches nothing, so it is `/kanban:work` that reads this and
    /// decides how to dispatch. Absent means "whatever the session is already running".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort this ticket's work deserves, honoured the same advisory way as `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Let this ticket's branch land in main without a human looking at the merge. Advisory, like `model` and `effort`:
    /// this binary merges nothing — it is `/kanban:work` that reads the flag and does the rebase-and-land. The effective
    /// answer also takes in the ticket's epic (see [`crate::store::derive::auto_merge`]); this field is only the ticket's
    /// own say, never the inherited one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_merge: bool,
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
    /// The GitHub PR tracking this ticket's branch, once one is known: recorded by the Create PR button, or discovered
    /// by the serve poller by branch (which is how skill- and daemon-created PRs get bound with no extra step). Survives
    /// column moves — rework keeps it, done keeps it as provenance. `state` and `merged_commit` are the poll's durable
    /// answers, so "PR merged — pull main" derives offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrRef>,
    /// Workflow state and that state's data, as one tagged object.
    pub column: Column,
}

/// A ticket's workflow state. Internally tagged on `"id"` so each state carries exactly its own fields: `{"id":"todo"}`,
/// `{"id":"doing","owner":…,"branch":…}`, `{"id":"review","branch":…}`, `{"id":"done","branch":…,"completed_at":…}`.
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
    /// Code-complete but not landed: the worktree is finished (or the external work delivered), and the branch or PR is
    /// waiting to reach the local main branch. Nobody owns a review ticket — entering review drops the claim; rework is
    /// a fresh claim.
    Review {
        /// The branch carrying the finished work, carried over from `doing` (or supplied on the move for a companion
        /// subtask that shared its parent's branch).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// Landed in the local main branch — or explicitly discarded.
    Done {
        /// The branch the work landed on, carried over from `review`/`doing`. Data, not a format: an external delegate's
        /// branch name is recorded verbatim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        completed_at: DateTime<Utc>,
        /// True when the work was retired without landing. A discarded ticket is closed but does NOT satisfy
        /// dependencies — tickets depending on it stay blocked until a human intervenes.
        #[serde(default, skip_serializing_if = "is_false")]
        discarded: bool,
    },
}

impl Column {
    /// The workflow state this column value sits in, without its data.
    #[must_use]
    pub fn id(&self) -> ColumnId {
        match self {
            Column::Todo => ColumnId::Todo,
            Column::Doing { .. } => ColumnId::Doing,
            Column::Review { .. } => ColumnId::Review,
            Column::Done { .. } => ColumnId::Done,
        }
    }

    /// The branch recorded on this column state, if any.
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        match self {
            Column::Todo => None,
            Column::Doing { branch, .. } | Column::Review { branch, .. } | Column::Done { branch, .. } => branch.as_deref(),
        }
    }
}

/// A ticket's bound GitHub pull request. `number`/`url` identify it; `state` and `merged_commit` are the last polled
/// answers, recorded on the board so every consumer (and every restart) sees them without asking the network again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "PrState::is_open")]
    pub state: PrState,
    /// The commit the PR merged as (GitHub's `mergeCommit.oid`) — the thing that must become an ancestor of the local
    /// main branch before the ticket counts as landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_commit: Option<String>,
}

/// The lifecycle of a bound PR as GitHub reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    #[default]
    Open,
    Merged,
    /// Closed without merging. The ticket stays in review, flagged — retiring or reworking it is the human's call.
    Closed,
}

impl PrState {
    /// Whether this is the default state (used to keep `open` off the wire).
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, PrState::Open)
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
    /// Let every ticket in this epic land without a human looking at the merge — one dial for a whole work stream.
    /// Inheritance is a read-side derivation ([`crate::store::derive::auto_merge`]) and nothing more: the flag is never
    /// written onto the epic's tickets, so clearing it here takes the permission back from all of them at once. As on a
    /// ticket, the board itself merges nothing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_merge: bool,
}

/// Upgrade an older board in memory. Returns whether anything changed (the caller's next write persists it); a schema
/// *newer* than this binary understands comes back as `Err` with the found version — misreading it would be worse than
/// stopping, so the caller must refuse loudly.
pub fn migrate(board: &mut Board) -> Result<bool, u32> {
    if board.schema > CURRENT_SCHEMA {
        return Err(board.schema);
    }
    // Independent of the schema, and deliberately not reported as a change: the id counters are additive fields that
    // default to zero, so a board written before they existed just needs flooring above the ids already in use. Nothing
    // has to be persisted eagerly — every mutation reads the board through here first, so the next write records the
    // reconciled counters, and no id is at risk before then because minting takes the same floor.
    board.reconcile_id_seqs();
    if board.schema == CURRENT_SCHEMA {
        return Ok(false);
    }
    // v1 → v2: the review column arrives between doing and done. Ticket columns are deliberately NOT rewritten — a v1
    // done ticket stays done even if its branch never merged; re-blocking previously satisfied dependencies on load
    // would be a surprise. Drag a card back to review to opt old work into v2 semantics.
    if !board.columns.iter().any(|c| c.id == ColumnId::Review) {
        let at = board.columns.iter().position(|c| c.id == ColumnId::Done).unwrap_or(board.columns.len());
        board.columns.insert(at, ColumnMeta { id: ColumnId::Review, title: "Review".into() });
    }
    board.schema = CURRENT_SCHEMA;
    Ok(true)
}

impl Board {
    /// An empty board with the four columns, as `init` seeds it.
    #[must_use]
    pub fn empty() -> Board {
        Board {
            schema: CURRENT_SCHEMA,
            version: 0,
            next_ticket_seq: 1,
            next_epic_seq: 1,
            columns: vec![
                ColumnMeta { id: ColumnId::Todo, title: "To do".into() },
                ColumnMeta { id: ColumnId::Doing, title: "Doing".into() },
                ColumnMeta { id: ColumnId::Review, title: "Review".into() },
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

    /// The id the next ticket would get, `K-<n>`, without minting it.
    #[must_use]
    pub fn next_ticket_id(&self) -> TicketId {
        TicketId(format!("K-{}", self.ticket_seq()))
    }

    /// The id the next epic would get, `EP-<n>`, without minting it.
    #[must_use]
    pub fn next_epic_id(&self) -> EpicId {
        EpicId(format!("EP-{}", self.epic_seq()))
    }

    /// Take the next ticket id and advance the counter past it. Deleting the ticket afterwards never gives the number
    /// back — see [`Board::next_ticket_seq`] for why reuse is unsafe.
    pub fn mint_ticket_id(&mut self) -> TicketId {
        let id = self.next_ticket_id();
        self.next_ticket_seq = self.ticket_seq() + 1;
        id
    }

    /// Take `count` consecutive ticket ids. The batch form exists for a refine split, whose tickets are built before any
    /// of them reaches the board — the counter, not the board's contents, is what keeps them apart.
    pub fn mint_ticket_ids(&mut self, count: usize) -> Vec<TicketId> {
        (0..count).map(|_| self.mint_ticket_id()).collect()
    }

    /// Take the next epic id and advance the counter past it.
    pub fn mint_epic_id(&mut self) -> EpicId {
        let id = self.next_epic_id();
        self.next_epic_seq = self.epic_seq() + 1;
        id
    }

    /// Floor both counters above every id in use. Idempotent, and the only thing that ever moves a counter other than
    /// minting — it exists so a board written before the counters existed (or hand-edited to carry a higher id than the
    /// counter knows about) cannot hand out an id that is already taken.
    pub fn reconcile_id_seqs(&mut self) {
        self.next_ticket_seq = self.ticket_seq();
        self.next_epic_seq = self.epic_seq();
    }

    /// The counter as minting sees it: the persisted value, never below one past the highest ticket id on the board.
    fn ticket_seq(&self) -> u64 {
        self.next_ticket_seq.max(next_numeric_suffix(self.tickets.iter().map(|t| t.id.0.as_str()), "K-"))
    }

    /// The epic counter, by the same rule.
    fn epic_seq(&self) -> u64 {
        self.next_epic_seq.max(next_numeric_suffix(self.epics.iter().map(|e| e.id.0.as_str()), "EP-"))
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
  "schema": 2,
  "version": 12,
  "next_ticket_seq": 5,
  "next_epic_seq": 2,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "review", "title": "Review" },
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
      "id": "K-4",
      "title": "Audit log for sign-ins",
      "epic": "EP-1",
      "status": "ready",
      "pr": { "number": 12, "url": "https://github.com/acme/myrepo/pull/12", "state": "merged", "merged_commit": "8f7d3a2c1b" },
      "column": { "id": "review", "branch": "k-4/audit-log" }
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

    /// A board as v1 wrote it: no `schema`, three columns, done without `discarded`. Kept verbatim as the migration
    /// fixture — real boards like this exist in real repos.
    const V1_BOARD: &str = r#"{
  "version": 7,
  "columns": [
    { "id": "todo", "title": "To do" },
    { "id": "doing", "title": "Doing" },
    { "id": "done", "title": "Done" }
  ],
  "epics": [],
  "tickets": [
    {
      "id": "K-1",
      "title": "Old finished work",
      "status": "ready",
      "column": { "id": "done", "branch": "k-1/old", "completed_at": "2026-07-14T09:12:00Z" }
    }
  ]
}"#;

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
        assert_eq!(board.schema, CURRENT_SCHEMA);
        assert_eq!(board.version, 12);
        assert_eq!(board.columns.len(), 4);
        let k1 = board.ticket(&TicketId("K-1".into())).unwrap();
        match &k1.column {
            Column::Doing { owner, branch } => {
                assert_eq!(owner, "claude");
                assert_eq!(branch.as_deref(), Some("k-1/session-refresh"));
            }
            other => panic!("K-1 should be doing, got {other:?}"),
        }
        let k4 = board.ticket(&TicketId("K-4".into())).unwrap();
        assert!(matches!(&k4.column, Column::Review { branch } if branch.as_deref() == Some("k-4/audit-log")));
        let pr = k4.pr.as_ref().unwrap();
        assert_eq!((pr.number, pr.state), (12, PrState::Merged));
        assert_eq!(pr.merged_commit.as_deref(), Some("8f7d3a2c1b"));
        let k3 = board.ticket(&TicketId("K-3".into())).unwrap();
        assert_eq!(k3.external.as_ref().unwrap().number, 42);
        assert!(matches!(k3.column, Column::Done { discarded: false, .. }));
        let k2 = board.ticket(&TicketId("K-2".into())).unwrap();
        assert_eq!(k2.depends_on, vec![TicketId("K-1".into())]);
        assert!(matches!(k2.column, Column::Todo));
    }

    #[test]
    fn a_v1_board_migrates_in_memory() {
        let mut board: Board = serde_json::from_str(V1_BOARD).expect("a schema-less v1 board still parses");
        assert_eq!(board.schema, 1, "absent schema means 1");
        assert!(migrate(&mut board).unwrap(), "the upgrade reports a change to persist");
        assert_eq!(board.schema, CURRENT_SCHEMA);
        let ids: Vec<ColumnId> = board.columns.iter().map(|c| c.id).collect();
        assert_eq!(ids, ColumnId::ALL, "review lands between doing and done");
        assert!(matches!(board.tickets[0].column, Column::Done { discarded: false, .. }), "old done stays done, not discarded");
        assert!(!migrate(&mut board).unwrap(), "migrating twice is a no-op");
    }

    #[test]
    fn a_newer_schema_is_refused_with_the_found_version() {
        let mut board: Board = serde_json::from_str(DESIGN_MD_BOARD).unwrap();
        board.schema = 3;
        assert_eq!(migrate(&mut board), Err(3), "a future board must be refused, never misread");
    }

    #[test]
    fn review_parses_bare_and_round_trips() {
        // Hand-written `{"id":"review"}` must be enough (branch optional), and both shapes must survive the
        // internally-tagged Content buffering unchanged.
        let bare: Ticket =
            serde_json::from_str(r#"{ "id": "K-1", "title": "x", "status": "ready", "column": { "id": "review" } }"#).unwrap();
        assert!(matches!(&bare.column, Column::Review { branch: None }));

        let col = Column::Review { branch: Some("k-1/x".into()) };
        let v = serde_json::to_value(&col).unwrap();
        assert_eq!(v, serde_json::json!({ "id": "review", "branch": "k-1/x" }));
        assert_eq!(serde_json::from_value::<Column>(v).unwrap(), col);
    }

    #[test]
    fn done_discarded_round_trips_and_defaults_false() {
        let kept = Column::Done { branch: None, completed_at: "2026-07-14T09:12:00Z".parse().unwrap(), discarded: false };
        let v = serde_json::to_value(&kept).unwrap();
        assert!(v.get("discarded").is_none(), "false stays off the wire — v1 done tickets are unchanged bytes");
        assert_eq!(serde_json::from_value::<Column>(v).unwrap(), kept);

        let dropped = Column::Done { branch: None, completed_at: "2026-07-14T09:12:00Z".parse().unwrap(), discarded: true };
        let v = serde_json::to_value(&dropped).unwrap();
        assert_eq!(v["discarded"], true);
        assert_eq!(serde_json::from_value::<Column>(v).unwrap(), dropped);
    }

    #[test]
    fn effort_round_trips_every_level_and_names_them_all_when_refusing() {
        let round_tripped: Vec<Effort> = Effort::ALL
            .into_iter()
            .map(|e| {
                let v = serde_json::to_value(e).unwrap();
                assert_eq!(v, serde_json::json!(e.as_str()), "the wire name is the display name");
                assert_eq!(e.as_str().parse::<Effort>().unwrap(), e, "FromStr inverts as_str");
                serde_json::from_value(v).unwrap()
            })
            .collect();
        assert_eq!(round_tripped, Effort::ALL);

        let err = "ludicrous".parse::<Effort>().unwrap_err();
        assert!(err.contains("ludicrous"), "the error quotes what was given: {err}");
        assert!(Effort::ALL.iter().all(|e| err.contains(e.as_str())), "and lists every level: {err}");
    }

    /// A ticket expressing no preference must serialize exactly as it did before the fields existed — that is the whole
    /// of the compatibility story, in both directions, with no schema bump.
    #[test]
    fn model_and_effort_stay_off_the_wire_when_unset() {
        let bare = r#"{ "id": "K-1", "title": "x", "status": "ready", "column": { "id": "todo" } }"#;
        let t: Ticket = serde_json::from_str(bare).unwrap();
        assert!(t.model.is_none() && t.effort.is_none(), "absent means inherit, not a default level");

        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("model").is_none() && v.get("effort").is_none());

        let set = Ticket { model: Some("claude-opus-4-8".into()), effort: Some(Effort::Xhigh), ..t };
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["model"], "claude-opus-4-8");
        assert_eq!(v["effort"], "xhigh");
        assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), set);
    }

    #[test]
    fn pr_state_open_stays_off_the_wire() {
        let pr = PrRef { number: 7, url: "https://example.invalid/pull/7".into(), state: PrState::Open, merged_commit: None };
        let v = serde_json::to_value(&pr).unwrap();
        assert!(v.get("state").is_none() && v.get("merged_commit").is_none());
        assert_eq!(serde_json::from_value::<PrRef>(v).unwrap(), pr);
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

    fn bare_ticket(id: &str) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: "t".into(),
            epic: None,
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
            column: Column::Todo,
        }
    }

    #[test]
    fn id_minting_skips_nonconforming_ids_and_starts_at_one() {
        let mut board = Board::empty();
        assert_eq!(board.next_ticket_id().0, "K-1");
        assert_eq!(board.next_epic_id().0, "EP-1");
        board.tickets = ["K-2", "K-9", "custom-name", "K-x"].into_iter().map(bare_ticket).collect();
        assert_eq!(board.next_ticket_id().0, "K-10", "a hand-named ticket can't wedge minting, and the floor still clears K-9");
    }

    #[test]
    fn minting_advances_the_counter_and_deleting_never_winds_it_back() {
        let mut board = Board::empty();
        let ids: Vec<String> = (0..3).map(|_| board.mint_ticket_id().0).collect();
        assert_eq!(ids, ["K-1", "K-2", "K-3"]);
        board.tickets = ids.iter().map(|id| bare_ticket(id)).collect();

        board.tickets.retain(|t| t.id.0 != "K-3");
        assert_eq!(board.next_ticket_id().0, "K-4", "K-3 is spent — its branch and worktree may still exist");
        board.tickets.clear();
        assert_eq!(board.mint_ticket_id().0, "K-4", "emptying the board doesn't reset the numbering either");
    }

    #[test]
    fn a_batch_mint_hands_out_consecutive_ids_before_any_ticket_lands() {
        // A refine split builds all its tickets before pushing any, so the counter alone has to keep them apart.
        let mut board = Board::empty();
        let ids: Vec<String> = board.mint_ticket_ids(3).into_iter().map(|id| id.0).collect();
        assert_eq!(ids, ["K-1", "K-2", "K-3"]);
        assert_eq!(board.mint_ticket_ids(2).into_iter().map(|id| id.0).collect::<Vec<_>>(), ["K-4", "K-5"]);
    }

    #[test]
    fn the_counters_survive_a_round_trip() {
        let mut board = Board::empty();
        board.mint_ticket_id();
        board.mint_epic_id();
        let json = serde_json::to_string(&board).unwrap();
        let back: Board = serde_json::from_str(&json).unwrap();
        assert_eq!((back.next_ticket_seq, back.next_epic_seq), (2, 2), "the counters are on the wire, not recomputed on load");
        assert_eq!(back, board);
    }

    /// A board written before the counters existed: they parse as zero, and load-time reconciliation must floor them
    /// above every id in use rather than starting over at one.
    #[test]
    fn a_pre_counter_board_mints_above_its_highest_ids() {
        let raw = serde_json::to_value(&Board {
            tickets: ["K-1", "K-7", "K-4"].into_iter().map(bare_ticket).collect(),
            epics: vec![Epic {
                id: EpicId("EP-3".into()),
                title: "auth".into(),
                color: "#7c9cf5".into(),
                status: Status::Ready,
                body: String::new(),
                auto_merge: false,
            }],
            ..Board::empty()
        })
        .map(|mut v| {
            let obj = v.as_object_mut().unwrap();
            obj.remove("next_ticket_seq");
            obj.remove("next_epic_seq");
            v
        })
        .unwrap();

        let mut board: Board = serde_json::from_value(raw).expect("a board without the counters still parses");
        assert_eq!((board.next_ticket_seq, board.next_epic_seq), (0, 0), "absent means zero, pending reconciliation");

        assert!(!migrate(&mut board).unwrap(), "reconciling the counters is not a schema change to persist eagerly");
        assert_eq!((board.next_ticket_seq, board.next_epic_seq), (8, 4), "floored above the highest id in use, not reset to one");
        assert_eq!(board.next_ticket_id().0, "K-8");
        assert_eq!(board.next_epic_id().0, "EP-4");
    }

    /// The board file is hand-editable, so the floor is a standing safety net and not just a one-shot upgrade: a counter
    /// that has fallen behind a hand-added id must never hand that id out a second time.
    #[test]
    fn a_counter_behind_a_hand_added_id_is_floored_on_load() {
        let mut board = Board { tickets: vec![bare_ticket("K-99")], next_ticket_seq: 3, ..Board::empty() };
        assert!(!migrate(&mut board).unwrap());
        assert_eq!(board.next_ticket_seq, 100);
        assert_eq!(board.mint_ticket_id().0, "K-100");
    }

    #[test]
    fn completed_at_serializes_as_rfc3339_z() {
        // Pins the chrono serde format against design.md's "2026-07-14T09:12:00Z" (internally-tagged enums buffer content
        // through serde's Content type — this also guards that path).
        let col = Column::Done { branch: None, completed_at: "2026-07-14T09:12:00Z".parse().unwrap(), discarded: false };
        let v = serde_json::to_value(&col).unwrap();
        assert_eq!(v["completed_at"], "2026-07-14T09:12:00Z");
    }
}
