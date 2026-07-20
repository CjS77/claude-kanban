//! The filter bar's query grammar: a comma-separated conjunction of terms over the board's tickets and epics.
//!
//! A fragment is either a bare phrase searched across a ticket's user-facing text, or a `key: value` term narrowing one
//! field. Everything is case-insensitive, terms combine by conjunction only — no `OR`, no negation, no grouping.
//!
//! **Parsing is infallible and has no error type.** The box fires on every keystroke after a 300 ms debounce, so a
//! half-typed `label:` or `landed: t` must render a board rather than an error: anything the grammar does not recognise
//! degrades to a free-text phrase. That degradation is also why no colon escaping is needed — `fix: the login bug` is a
//! phrase, because `fix` is not a key.

use crate::store::{
    derive::{EpicView, TicketView},
    model::{Column, ColumnId, Effort, Status, Ticket},
};

/// A parsed search query: a conjunction of terms. Parsing is infallible — anything the grammar does not recognise
/// degrades to a free-text term.
#[derive(Debug, Clone, Default)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq)]
enum Term {
    /// A phrase searched across the ticket's user-facing text.
    Text(String),
    Label(String),
    Epic(String),
    /// Tickets attached to no epic at all — `epic:none` / `epic:null`.
    NoEpic,
    Id(String),
    Note(String),
    Status(Status),
    Column(ColumnId),
    /// The ticket's model preference, matched as a substring so `model:opus` also finds `claude-opus-4-8`.
    Model(String),
    Effort(Effort),
    /// Done and not discarded — `landed:`.
    Landed(bool),
    Discarded(bool),
    Blocked(bool),
}

impl Query {
    /// Parse a raw query string. Never fails; a blank query yields no terms.
    #[must_use]
    pub fn parse(raw: &str) -> Query {
        Query { terms: fragments(raw).into_iter().filter_map(term).collect() }
    }

    /// True iff this query admits every ticket — i.e. it has no terms.
    ///
    /// This is *only* `terms.is_empty()`, never a re-derived condition and never a string check on the raw query: the
    /// board's dragging guard rides on it being exactly equivalent to "hides nothing". [`Query::parse`] earns that by
    /// dropping every blank fragment and never storing a term with an empty needle.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Whether a ticket satisfies every term. `epics` resolves `epic:` — a ticket stores only its epic's id, but the
    /// term matches the epic's title too.
    #[must_use]
    pub fn matches(&self, t: &TicketView, epics: &[EpicView]) -> bool {
        self.terms.iter().all(|term| admits_ticket(term, t, epics))
    }

    /// Whether an epic card satisfies every term that *can* apply to an epic. A ticket-only term (label, column,
    /// landed, …) excludes every epic card.
    #[must_use]
    pub fn matches_epic(&self, e: &EpicView) -> bool {
        self.terms.iter().all(|term| admits_epic(term, e))
    }
}

// ---- parsing ----------------------------------------------------------------------------------------------------------

/// Split on the commas that sit outside double quotes, so `label:"foo, bar"` stays one fragment. Blank fragments are
/// dropped right here — that is what makes `is_empty` structurally equivalent to "admits everything".
fn fragments(raw: &str) -> Vec<&str> {
    let (mut out, mut start, mut quoted) = (Vec::new(), 0usize, false);
    raw.char_indices().for_each(|(i, c)| match c {
        '"' => quoted = !quoted,
        ',' if !quoted => {
            out.push(&raw[start..i]);
            start = i + 1;
        }
        _ => {}
    });
    out.push(&raw[start..]);
    out.into_iter().map(str::trim).filter(|f| !f.is_empty()).collect()
}

/// One fragment → one term, or `None` when the fragment carries nothing to search for. A fragment wrapped in quotes is
/// always free text; otherwise a known key before the first colon makes it keyed, and anything else is free text.
fn term(fragment: &str) -> Option<Term> {
    if let Some(inner) = unquote(fragment) {
        return needle(inner).map(Term::Text);
    }
    fragment
        .split_once(':')
        .and_then(|(key, value)| keyed(&key.trim().to_lowercase(), unquote_or(value.trim())))
        .or_else(|| needle(fragment).map(Term::Text))
}

fn keyed(key: &str, value: &str) -> Option<Term> {
    match key {
        "text" => needle(value).map(Term::Text),
        "label" => needle(value).map(Term::Label),
        "epic" => epic(value),
        "id" => needle(value).map(Term::Id),
        "note" => needle(value).map(Term::Note),
        "status" => status(value).map(Term::Status),
        "col" | "column" => needle(value)?.parse().ok().map(Term::Column),
        "model" => needle(value).map(Term::Model),
        "effort" => effort(value).map(Term::Effort),
        "landed" => boolean(value).map(Term::Landed),
        "discarded" => boolean(value).map(Term::Discarded),
        "blocked" => boolean(value).map(Term::Blocked),
        _ => None,
    }
}

/// Strip one layer of surrounding double quotes, if the string wears them.
fn unquote(s: &str) -> Option<&str> {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
}

fn unquote_or(s: &str) -> &str {
    unquote(s).unwrap_or(s)
}

/// A lowercased, non-empty needle. Empty needles are never stored: a term matching everything would let `is_empty`
/// disagree with `matches`, and the board would stop dragging for no visible reason. A key whose value comes back empty
/// therefore fails to parse, and its fragment degrades to free text like any other malformed one.
fn needle(s: &str) -> Option<String> {
    let n = s.trim().to_lowercase();
    (!n.is_empty()).then_some(n)
}

/// A status by name, or by any prefix of one (`status:re` → review). Prefixes resolve in workflow order, so the first
/// status a prefix fits wins.
fn status(value: &str) -> Option<Status> {
    let v = needle(value)?;
    [Status::Draft, Status::Stub, Status::Review, Status::Ready].into_iter().find(|s| s.as_str().starts_with(&v))
}

/// An effort level by name, or by any prefix of one (`effort:x` → xhigh), resolving in [`Effort::ALL`] order like
/// [`status`] does. `effort:m` is therefore medium, not max — the ambiguity is the price of prefixes, and spelling the
/// level out always wins.
fn effort(value: &str) -> Option<Effort> {
    let v = needle(value)?;
    Effort::ALL.into_iter().find(|e| e.as_str().starts_with(&v))
}

/// An epic to match by id or title, or the two spellings of "no epic at all". `none` and `null` are reserved words here,
/// so an epic actually titled "none" is only reachable by its id — a fair trade for the far commoner question, "what
/// isn't filed under anything?".
fn epic(value: &str) -> Option<Term> {
    let n = needle(value)?;
    match n.as_str() {
        "none" | "null" => Some(Term::NoEpic),
        _ => Some(Term::Epic(n)),
    }
}

fn boolean(value: &str) -> Option<bool> {
    match needle(value)?.as_str() {
        "true" | "yes" | "y" | "1" | "on" => Some(true),
        "false" | "no" | "n" | "0" | "off" => Some(false),
        _ => None,
    }
}

// ---- matching ---------------------------------------------------------------------------------------------------------

fn admits_ticket(term: &Term, t: &TicketView, epics: &[EpicView]) -> bool {
    let ticket = &t.ticket;
    match term {
        Term::Text(p) => free_text(p, ticket),
        Term::Label(p) => ticket.labels.iter().any(|l| contains(l, p)),
        Term::Epic(p) => epic_named(p, ticket, epics),
        Term::NoEpic => ticket.epic.is_none(),
        Term::Id(p) => contains(&ticket.id.0, p),
        Term::Note(p) => ticket.notes.iter().any(|n| contains(&n.text, p)),
        Term::Status(s) => ticket.status == *s,
        Term::Column(c) => ticket.column.id() == *c,
        Term::Model(p) => ticket.model.as_deref().is_some_and(|m| contains(m, p)),
        Term::Effort(e) => ticket.effort == Some(*e),
        Term::Landed(want) => matches!(ticket.column, Column::Done { discarded: false, .. }) == *want,
        Term::Discarded(want) => matches!(ticket.column, Column::Done { discarded: true, .. }) == *want,
        Term::Blocked(want) => t.blocked == *want,
    }
}

fn admits_epic(term: &Term, e: &EpicView) -> bool {
    match term {
        Term::Text(p) => [&e.epic.id.0, &e.epic.title, &e.epic.body].into_iter().any(|h| contains(h, p)),
        Term::Epic(p) => contains(&e.epic.id.0, p) || contains(&e.epic.title, p),
        Term::Status(s) => e.epic.status == *s,
        // An epic has no labels, no notes and never lands: a ticket-only term can't be satisfied, and leaving the card
        // on screen would misrepresent the filter. `epic:none` asks for cards filed under nothing, which an epic card
        // is the very opposite of, so it goes the same way.
        _ => false,
    }
}

/// The text a bare phrase searches: what a human wrote about *this* ticket, or would recognise on its card.
///
/// Deliberately absent: `notes` (machine-written progress logs — near-identical on every landed ticket, so folding them
/// in would make common words match almost everything; they get the opt-in `note:` key instead), `depends_on` (ids of
/// *other* tickets), and `status`/`column` (fixed vocabularies with their own keys, so a bare `done` searches prose).
fn free_text(p: &str, t: &Ticket) -> bool {
    let rendered = [
        t.external.as_ref().map(|e| format!("{} {}#{}", e.provider, e.kind, e.number)),
        t.pr.as_ref().map(|pr| format!("#{} {}", pr.number, pr.url)),
    ];
    [t.id.0.as_str(), t.title.as_str(), t.body.as_str()]
        .into_iter()
        .chain(t.labels.iter().map(String::as_str))
        .chain(t.column.branch())
        .any(|h| contains(h, p))
        || rendered.iter().flatten().any(|h| contains(h, p))
}

/// Whether the ticket's epic answers to `p`, by id or by title.
fn epic_named(p: &str, t: &Ticket, epics: &[EpicView]) -> bool {
    t.epic
        .as_ref()
        .is_some_and(|id| contains(&id.0, p) || epics.iter().any(|e| e.epic.id == *id && contains(&e.epic.title, p)))
}

/// Substring match against an already-lowercased needle. `to_lowercase`, not the ASCII variant: labels and titles are
/// free-form user text.
fn contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::store::model::{Epic, EpicId, External, Note, TicketId};

    fn ticket(id: &str, title: &str) -> Ticket {
        Ticket {
            id: TicketId(id.into()),
            title: title.into(),
            epic: None,
            status: Status::Ready,
            body: String::new(),
            labels: vec![],
            model: None,
            effort: None,
            depends_on: vec![],
            notes: vec![],
            external: None,
            pr: None,
            column: Column::Todo,
        }
    }

    fn view(t: Ticket) -> TicketView {
        TicketView { ticket: t, blocked: false, claim: None }
    }

    fn done(discarded: bool) -> Column {
        Column::Done { branch: Some("k-1/work".into()), completed_at: Utc::now(), discarded }
    }

    fn epic_view(id: &str, title: &str) -> EpicView {
        EpicView {
            epic: Epic { id: EpicId(id.into()), title: title.into(), color: "#fff".into(), status: Status::Ready, body: "the plan".into() },
            column: ColumnId::Todo,
            tickets: vec![],
        }
    }

    /// The dragging invariant's first half: nothing that hides no cards may report itself as a filter.
    #[test]
    fn blank_and_punctuation_only_queries_are_empty() {
        let filtering: Vec<&str> =
            ["", "   ", ",,", " , ", "\t\n", ",  ,,   ,", "\"\""].into_iter().filter(|raw| !Query::parse(raw).is_empty()).collect();
        assert!(filtering.is_empty(), "these hide nothing, so they must parse to no terms: {filtering:?}");
    }

    /// The other half: a query with no terms admits every ticket, whatever shape it is in.
    #[test]
    fn an_empty_query_admits_every_ticket() {
        let mut discarded = ticket("K-9", "Retired");
        discarded.column = done(true);
        let tickets = [ticket("K-1", "Todo work"), discarded];
        let q = Query::parse("  ");
        assert!(tickets.into_iter().all(|t| q.matches(&view(t), &[])));
        assert!(q.matches_epic(&epic_view("EP-1", "Board")));
    }

    #[test]
    fn unknown_keys_degrade_to_free_text() {
        assert_eq!(Query::parse("fix: the login bug").terms, vec![Term::Text("fix: the login bug".into())]);

        let mut t = ticket("K-1", "Nothing to see");
        t.body = "We should FIX: the Login Bug before release".into();
        assert!(Query::parse("fix: the login bug").matches(&view(t), &[]));
    }

    #[test]
    fn quotes_force_free_text_and_protect_commas() {
        assert_eq!(Query::parse("\"label: ux\"").terms, vec![Term::Text("label: ux".into())]);
        assert_eq!(Query::parse("label:\"foo, bar\"").terms, vec![Term::Label("foo, bar".into())]);
    }

    #[test]
    fn matching_is_case_insensitive_in_keys_and_values() {
        let mut t = ticket("K-1", "Anything");
        t.labels = vec!["UX".into()];
        assert!(Query::parse("LABEL: ux").matches(&view(t.clone()), &[]));
        assert!(Query::parse("label:UX").matches(&view(t.clone()), &[]));

        t.labels = vec!["ux".into()];
        assert!(Query::parse("Label: UX").matches(&view(t), &[]));
    }

    #[test]
    fn free_text_spans_id_title_body_labels_and_branch() {
        let mut t = ticket("K-27", "Search bar");
        t.body = "Realtime results as you type".into();
        t.labels = vec!["UX".into()];
        t.external = Some(External { provider: "github".into(), kind: "issue".into(), number: 42 });
        t.column = Column::Review { branch: Some("k-27/search-bar".into()) };
        let v = view(t);

        let missed: Vec<&str> = ["k-27", "search bar", "realtime results", "ux", "search-bar", "github issue#42"]
            .into_iter()
            .filter(|q| !Query::parse(q).matches(&v, &[]))
            .collect();
        assert!(missed.is_empty(), "every field a bare phrase covers must match: {missed:?}");
        assert!(!Query::parse("nowhere in this ticket").matches(&v, &[]));
    }

    #[test]
    fn free_text_ignores_notes_but_the_note_key_finds_them() {
        let mut t = ticket("K-1", "Quiet title");
        t.notes = vec![Note { at: Utc::now(), author: Some("claude".into()), text: "worktree started".into() }];
        let v = view(t);
        assert!(!Query::parse("worktree").matches(&v, &[]), "notes stay out of free text");
        assert!(Query::parse("note: worktree").matches(&v, &[]), "the opt-in key finds them");
    }

    #[test]
    fn landed_is_done_and_not_discarded() {
        let mut landed = ticket("K-1", "Shipped");
        landed.column = done(false);
        let mut binned = ticket("K-2", "Retired");
        binned.column = done(true);
        let (landed, binned) = (view(landed), view(binned));

        assert!(Query::parse("landed: true").matches(&landed, &[]));
        assert!(!Query::parse("landed: true").matches(&binned, &[]), "a discarded ticket never landed");
        assert!(Query::parse("discarded:yes").matches(&binned, &[]));
        assert!(!Query::parse("discarded:yes").matches(&landed, &[]));
        // Both sit in the done column, whatever their fate.
        assert!(["col:done", "column: DONE"].into_iter().all(|q| Query::parse(q).matches(&binned, &[])));
    }

    #[test]
    fn bad_booleans_and_unknown_statuses_degrade_to_free_text() {
        assert_eq!(Query::parse("landed:maybe").terms, vec![Term::Text("landed:maybe".into())]);
        assert_eq!(Query::parse("status:shipped").terms, vec![Term::Text("status:shipped".into())]);
        assert_eq!(Query::parse("col:backlog").terms, vec![Term::Text("col:backlog".into())]);
        // A half-typed key is a phrase, not an error and not a match-everything term.
        assert_eq!(Query::parse("label:").terms, vec![Term::Text("label:".into())]);
    }

    #[test]
    fn status_accepts_a_prefix() {
        assert_eq!(Query::parse("status:re").terms, vec![Term::Status(Status::Review)]);
        assert_eq!(Query::parse("status: DRAFT").terms, vec![Term::Status(Status::Draft)]);
        assert_eq!(Query::parse("status:s").terms, vec![Term::Status(Status::Stub)]);
    }

    #[test]
    fn effort_accepts_a_prefix_resolving_in_level_order() {
        assert_eq!(Query::parse("effort:x").terms, vec![Term::Effort(Effort::Xhigh)]);
        assert_eq!(Query::parse("effort: MAX").terms, vec![Term::Effort(Effort::Max)]);
        assert_eq!(Query::parse("effort:h").terms, vec![Term::Effort(Effort::High)]);
        // `m` fits both medium and max; ALL order decides, and spelling it out always wins.
        assert_eq!(Query::parse("effort:m").terms, vec![Term::Effort(Effort::Medium)]);
        assert_eq!(Query::parse("effort:nonsense").terms, vec![Term::Text("effort:nonsense".into())]);
    }

    #[test]
    fn model_matches_as_a_substring_and_effort_exactly() {
        let mut t = ticket("K-9", "Hard one");
        t.model = Some("claude-opus-4-8".into());
        t.effort = Some(Effort::Xhigh);
        let v = view(t);

        assert!(Query::parse("model:opus").matches(&v, &[]), "an alias finds the full id it names");
        assert!(Query::parse("model: OPUS").matches(&v, &[]));
        assert!(!Query::parse("model:sonnet").matches(&v, &[]));
        assert!(Query::parse("effort:xhigh").matches(&v, &[]));
        assert!(!Query::parse("effort:high").matches(&v, &[]), "xhigh is not high — the enum compares exactly");

        // A ticket expressing no preference is matched by neither term, rather than by every one.
        let plain = view(ticket("K-10", "Ordinary"));
        assert!(!Query::parse("model:opus").matches(&plain, &[]));
        assert!(!Query::parse("effort:low").matches(&plain, &[]));
    }

    #[test]
    fn ticket_only_terms_hide_epic_cards() {
        let e = epic_view("EP-1", "Board UI");
        assert!(Query::parse("board").matches_epic(&e), "free text searches the epic's own words");
        assert!(Query::parse("the plan").matches_epic(&e), "including its body");
        assert!(Query::parse("epic: ep-1").matches_epic(&e));
        assert!(Query::parse("status: ready").matches_epic(&e));

        let ticket_only = [
            "label: ux", "col: todo", "landed: false", "discarded:no", "blocked:false", "id: EP-1", "note: anything", "model: opus",
            "effort: max",
        ];
        let leaked: Vec<&str> = ticket_only.into_iter().filter(|q| Query::parse(q).matches_epic(&e)).collect();
        assert!(leaked.is_empty(), "a ticket-only term cannot be satisfied by an epic, so it must hide the card: {leaked:?}");
    }

    #[test]
    fn the_worked_example_ands_three_terms() {
        let q = Query::parse("landed: true, label: ux, realtime results");
        assert_eq!(
            q.terms,
            vec![Term::Landed(true), Term::Label("ux".into()), Term::Text("realtime results".into())]
        );

        let mut t = ticket("K-27", "Search bar");
        t.body = "Realtime results as you type".into();
        t.labels = vec!["UX".into()];
        t.column = done(false);
        assert!(q.matches(&view(t.clone()), &[]));

        t.column = done(true);
        assert!(!q.matches(&view(t), &[]), "discarded work has not landed");
    }

    #[test]
    fn the_epic_key_matches_by_id_or_title() {
        let epics = [epic_view("EP-1", "Board UI")];
        let mut t = ticket("K-1", "Anything");
        t.epic = Some(EpicId("EP-1".into()));
        let v = view(t);
        assert!(Query::parse("epic: ep-1").matches(&v, &epics));
        assert!(Query::parse("epic: board").matches(&v, &epics));
        assert!(!Query::parse("epic: landing").matches(&v, &epics));
        assert!(!Query::parse("epic: board").matches(&view(ticket("K-2", "Epicless")), &epics));
    }

    #[test]
    fn epic_none_and_epic_null_both_find_the_unfiled() {
        let epics = [epic_view("EP-1", "Board UI")];
        let mut filed = ticket("K-1", "Filed");
        filed.epic = Some(EpicId("EP-1".into()));
        let (filed, loose) = (view(filed), view(ticket("K-2", "Loose")));

        let spellings = ["epic:none", "epic: NULL", "epic:Null", "epic: none"];
        let wrong: Vec<&str> = spellings
            .into_iter()
            .filter(|q| !Query::parse(q).matches(&loose, &epics) || Query::parse(q).matches(&filed, &epics))
            .collect();
        assert!(wrong.is_empty(), "every spelling must admit only the unfiled ticket: {wrong:?}");

        // Identical parses, not merely identical outcomes on this pair of tickets.
        assert_eq!(Query::parse("epic:none").terms, vec![Term::NoEpic]);
        assert_eq!(Query::parse("epic:null").terms, Query::parse("epic:none").terms);
    }

    /// The reserved words cost `epic:` two titles, and the escape hatch is the epic's id.
    #[test]
    fn epic_none_beats_an_epic_actually_called_none() {
        let epics = [epic_view("EP-9", "None")];
        let mut t = ticket("K-1", "Filed under None");
        t.epic = Some(EpicId("EP-9".into()));
        let v = view(t);
        assert!(!Query::parse("epic: none").matches(&v, &epics), "none is reserved, not a title lookup");
        assert!(Query::parse("epic: ep-9").matches(&v, &epics), "its id still reaches it");
    }

    #[test]
    fn epic_none_hides_epic_cards() {
        let e = epic_view("EP-1", "Board UI");
        assert!(!Query::parse("epic:none").matches_epic(&e), "an epic card is the opposite of unfiled");
        assert!(!Query::parse("epic:null").matches_epic(&e));
    }
}
