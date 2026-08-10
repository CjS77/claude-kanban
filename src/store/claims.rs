//! The live-claims sidecar, `.kanban/claims.json`.
//!
//! A claim records who is doing something to a ticket *right now* and since when — that's what surfaces "Claude is working on
//! this" on the card face. Claims are machine-local live facts (a worktree path on this machine means nothing on a
//! collaborator's), so the file is gitignored and carries no version counter; durable, shareable outcomes (`owner`, `branch`,
//! `completed_at`) nest under the ticket's `column` in `board.json` instead. Both files sit under the same advisory lock.
//!
//! Two kinds of record share the file, distinguished by [`ClaimKind`]: ownership of a ticket being *worked*, and the short
//! in-flight marker on a review ticket being *landed*. They live together because they answer the same question — who is busy
//! with this right now — and because [`crate::ops`] can write this file transactionally alongside the board, which is what
//! lets a landing record retire with the landing that ends it.
//!
//! Wire format: a bare JSON array of claims; a missing file is an empty one.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::TicketId;

/// What a live record means. Absent on the wire means [`ClaimKind::Work`], so every claims file written before landings
/// existed parses unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// Somebody owns this ticket and is working it. The card says so, and nobody else may claim it.
    #[default]
    Work,
    /// A review ticket is being rebased and fast-forwarded into the main branch right now. **Not** ownership: a review
    /// ticket has no owner, and this record never puts one on the card — it is an interlock, so a second work loop can
    /// see the landing is already in hand instead of racing it onto the same branch.
    Landing,
}

impl ClaimKind {
    // serde's skip_serializing_if demands fn(&T) — the reference is the contract, not a choice.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_work(&self) -> bool {
        matches!(self, ClaimKind::Work)
    }
}

/// One live record: `{ticket, agent, since, path, kind}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub ticket: TicketId,
    /// Who is working it — an agent name like `claude`, or a human.
    pub agent: String,
    pub since: DateTime<Utc>,
    /// The ticket's worktree on *this* machine, filled in by `worktree start`. `None` between claim and start, or after
    /// `worktree finish` while the ticket is still in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Ownership, or an in-flight landing. Absent means ownership — the only kind that existed before landings did.
    #[serde(default, skip_serializing_if = "ClaimKind::is_work")]
    pub kind: ClaimKind,
}

impl Claim {
    /// Whether this record is an in-flight landing rather than ownership of the work.
    #[must_use]
    pub fn is_landing(&self) -> bool {
        self.kind == ClaimKind::Landing
    }
}

/// How long an in-flight landing stays believable. A landing is seconds of git; anything still marked after this either
/// crashed or was killed, and the ticket has to become landable again or it is stuck looking busy forever. This is the
/// whole crash-recovery story, and it is deliberately the dumbest one that works: no pids, no heartbeats, no lock files
/// to leak. The cost of it being wrong is one duplicated rebase attempt, which `--ff-only` makes a no-op.
pub const LANDING_STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(15);

/// The live record on `ticket`, whatever kind it is.
#[must_use]
pub fn find<'a>(claims: &'a [Claim], ticket: &TicketId) -> Option<&'a Claim> {
    claims.iter().find(|c| &c.ticket == ticket)
}

/// Whether a landing on `ticket` is in flight *and* recent enough to believe — the interlock `kanban_next` consults
/// before handing the same branch to a second loop.
#[must_use]
pub fn landing_in_flight(claims: &[Claim], ticket: &TicketId, now: DateTime<Utc>) -> bool {
    find(claims, ticket).is_some_and(|c| c.is_landing() && now - c.since < LANDING_STALE_AFTER)
}

/// Drop the record on `ticket` only if it is an in-flight landing, leaving real ownership alone. The counterpart to the
/// flag-clearing rules: every transition that makes an approval stale makes an in-flight landing stale too.
pub fn remove_landing(claims: &mut Vec<Claim>, ticket: &TicketId) {
    if find(claims, ticket).is_some_and(Claim::is_landing) {
        remove(claims, ticket);
    }
}

/// Insert `claim`, replacing any existing claim on the same ticket.
pub fn upsert(claims: &mut Vec<Claim>, claim: Claim) {
    remove(claims, &claim.ticket);
    claims.push(claim);
}

/// Drop the claim on `ticket`, returning it if one was live.
pub fn remove(claims: &mut Vec<Claim>, ticket: &TicketId) -> Option<Claim> {
    claims.iter().position(|c| &c.ticket == ticket).map(|i| claims.remove(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str, agent: &str) -> Claim {
        Claim { ticket: TicketId(id.into()), agent: agent.into(), since: Utc::now(), path: None, kind: ClaimKind::Work }
    }

    /// The wire contract: a claims file written before landings existed parses, reads as ownership, and never grows the
    /// key by being read and rewritten.
    #[test]
    fn the_kind_stays_off_the_wire_for_ordinary_claims() {
        let bare = r#"[{"ticket":"K-1","agent":"claude","since":"2024-01-01T00:00:00Z"}]"#;
        let claims: Vec<Claim> = serde_json::from_str(bare).unwrap();
        assert_eq!(claims[0].kind, ClaimKind::Work, "absent means ownership");
        assert!(!claims[0].is_landing());

        let v = serde_json::to_value(&claims[0]).unwrap();
        assert!(v.get("kind").is_none(), "ownership stays off the wire");

        let landing = Claim { kind: ClaimKind::Landing, ..claims[0].clone() };
        let v = serde_json::to_value(&landing).unwrap();
        assert_eq!(v["kind"], "landing");
        assert_eq!(serde_json::from_value::<Claim>(v).unwrap(), landing);
    }

    #[test]
    fn a_landing_is_believed_until_it_goes_stale() {
        let id = TicketId("K-1".into());
        let now = Utc::now();
        let mut claims = vec![Claim { kind: ClaimKind::Landing, ..claim("K-1", "claude") }];
        assert!(landing_in_flight(&claims, &id, now), "a landing taken just now is in hand");

        // The crash case: nothing ever cleared it, so after the cutoff the ticket becomes landable again.
        claims[0].since = now - LANDING_STALE_AFTER - chrono::TimeDelta::seconds(1);
        assert!(!landing_in_flight(&claims, &id, now), "a stale landing must not wedge the ticket forever");

        // Ownership is never mistaken for a landing, however old it is.
        let owned = vec![claim("K-1", "claude")];
        assert!(!landing_in_flight(&owned, &id, now));
    }

    #[test]
    fn remove_landing_spares_real_ownership() {
        let id = TicketId("K-1".into());
        let mut owned = vec![claim("K-1", "claude")];
        remove_landing(&mut owned, &id);
        assert_eq!(owned.len(), 1, "a worker's claim is not a landing marker to clean up");

        let mut landing = vec![Claim { kind: ClaimKind::Landing, ..claim("K-1", "claude") }];
        remove_landing(&mut landing, &id);
        assert!(landing.is_empty());
    }

    #[test]
    fn upsert_replaces_a_claim_on_the_same_ticket() {
        let mut claims = vec![claim("K-1", "claude")];
        upsert(&mut claims, claim("K-1", "someone-else"));
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].agent, "someone-else");
    }

    #[test]
    fn remove_returns_the_dropped_claim() {
        let mut claims = vec![claim("K-1", "claude"), claim("K-2", "claude")];
        assert_eq!(remove(&mut claims, &TicketId("K-1".into())).unwrap().ticket.0, "K-1");
        assert!(remove(&mut claims, &TicketId("K-1".into())).is_none());
        assert_eq!(claims.len(), 1);
    }
}
