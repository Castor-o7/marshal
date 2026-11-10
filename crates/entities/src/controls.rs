//! The operator's sender allow-list (MAR-89, from muster's MUS-9). Fork only,
//! and off unless the daemon sets a list.
//!
//! With a list set, only sessions on a named machine may send messages. The
//! daemon reads the list from `allowed-senders` in its state directory and
//! sets it here; no list, or an empty one, means everyone may send, which is
//! upstream's behaviour.

use std::sync::RwLock;

static ALLOWED_SENDERS: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// Set the machines that may send, or `None` for everyone.
pub fn set_allowed_senders(machines: Option<Vec<String>>) {
    *ALLOWED_SENDERS.write().unwrap_or_else(|e| e.into_inner()) =
        machines.filter(|list| !list.is_empty());
}

/// Whether a session on `machine` may send. A session with no machine is
/// refused whenever a list is set, since it can't be matched against it.
pub fn sender_allowed(machine: Option<&str>) -> Result<(), String> {
    let allowed = ALLOWED_SENDERS.read().unwrap_or_else(|e| e.into_inner());
    let Some(list) = allowed.as_ref() else {
        return Ok(());
    };
    match machine {
        Some(m) if list.iter().any(|a| a.eq_ignore_ascii_case(m)) => Ok(()),
        Some(m) => Err(format!(
            "the operator allows messages only from {}; this session is on {m}",
            list.join(", ")
        )),
        None => Err(format!(
            "the operator allows messages only from {}; this session names no machine",
            list.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is one per process, so these tests take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn no_list_or_an_empty_one_lets_everyone_send() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        set_allowed_senders(None);
        assert!(sender_allowed(Some("anyone")).is_ok());
        assert!(sender_allowed(None).is_ok());
        set_allowed_senders(Some(vec![]));
        assert!(sender_allowed(None).is_ok(), "an empty list is no list");
    }

    #[test]
    fn a_list_admits_its_machines_in_any_case_and_refuses_the_rest() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        set_allowed_senders(Some(vec!["Laptop".into(), "murph".into()]));
        assert!(sender_allowed(Some("laptop")).is_ok());
        assert!(sender_allowed(Some("MURPH")).is_ok());
        let refused = sender_allowed(Some("desk")).unwrap_err();
        assert!(
            refused.contains("only from Laptop, murph") && refused.contains("on desk"),
            "{refused}"
        );
        assert!(
            sender_allowed(None)
                .unwrap_err()
                .contains("names no machine")
        );
        set_allowed_senders(None);
    }
}
