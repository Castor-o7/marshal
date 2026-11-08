//! The identity key a session's nickname was assigned under (MUS-3). See
//! `succession.rs` for how a restarted session is matched to it.

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

/// The identity key a session's nickname was assigned under. The entity `id`
/// is the session id. Kept after the session's row goes, so a later session
/// can find it as a predecessor; removed when a successor takes it over.
#[myko_item]
pub struct SessionIdentity {
    /// `cwd:<harness>|<cwd>|<operator>|<host>`, or `role:<name>|<operator>|<host>`.
    pub key: String,
    /// Whether the key is a declared role (`MARSHAL_IDENTITY=<name>`) rather
    /// than inferred from the working directory. A role is a claim: any
    /// session on the machine can make it.
    pub role: bool,
    /// When the nickname was assigned, in ms since the epoch.
    pub assigned_at: i64,
}
