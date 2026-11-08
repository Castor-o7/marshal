//! Stable identity across restarts (MUS-3).
//!
//! A session's nickname is keyed by its session id, and a restarted Claude
//! session or a new Codex thread gets a new id. So the daemon matches a new
//! session to a dead predecessor by an identity key (the harness and working
//! directory, or a declared role, on the same machine for the same operator)
//! and hands it the predecessor's nickname.
//!
//! This and `SessionIdentity` are written by the daemon only. The gate refuses a
//! client's events on anything but its own `Session` row, so a client can
//! declare an identity on that row but can't write who succeeded whom.

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

/// An observed record that a session took over a dead predecessor's nickname.
/// The entity `id` is the predecessor's session id, so mail addressed to that
/// old id can follow it.
#[myko_item]
pub struct Succession {
    /// The session that took the nickname over.
    pub to_session_id: String,
    /// The nickname it took over.
    pub nickname: String,
    /// The identity key both sessions shared.
    pub key: String,
    /// Why the daemon counted the predecessor as dead, so a wrong succession
    /// can be traced.
    pub reason: String,
    /// When, in ms since the epoch.
    pub at: i64,
    /// The nickname the successor had before it took this one over, when the
    /// takeover came after it registered (its predecessor hadn't counted as
    /// dead yet). Mail to it still reaches the successor until `alias_until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_until: Option<i64>,
    /// Whether the successor has been told its nickname changed. Only a late
    /// takeover needs telling: one at registration shows in its session block.
    #[serde(default)]
    pub told: bool,
}

/// Follow a session id through successions to the session that holds its
/// identity now. `None` when no session took it over.
pub fn successor_of(successions: &[std::sync::Arc<Succession>], id: &str) -> Option<String> {
    let mut at = id.to_string();
    let mut moved = false;
    // A cycle can't be written (a successor is always newer), but bound it anyway.
    for _ in 0..16 {
        match successions.iter().find(|s| s.id.0.as_ref() == at) {
            Some(s) => {
                at = s.to_session_id.clone();
                moved = true;
            }
            None => break,
        }
    }
    moved.then_some(at)
}
