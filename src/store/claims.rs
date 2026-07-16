//! The live-claims sidecar, `.kanban/claims.json`.
//!
//! A claim records who is working a ticket *right now* and since when — that's what surfaces "Claude is working on this" on
//! the card face. Claims are machine-local live facts (a worktree path on this machine means nothing on a collaborator's), so
//! the file is gitignored and carries no version counter; durable, shareable outcomes (`owner`, `branch`, `completed_at`) nest
//! under the ticket's `column` in `board.json` instead. Both files sit under the same advisory lock.
//!
//! Wire format: a bare JSON array of claims; a missing file is an empty one.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::TicketId;

/// One live claim: `{ticket, agent, since, path}`.
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
}

/// The live claim on `ticket`, if any.
#[must_use] 
pub fn find<'a>(claims: &'a [Claim], ticket: &TicketId) -> Option<&'a Claim> {
    claims.iter().find(|c| &c.ticket == ticket)
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
        Claim { ticket: TicketId(id.into()), agent: agent.into(), since: Utc::now(), path: None }
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
