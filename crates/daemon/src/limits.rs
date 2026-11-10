//! Connection caps (MAR-88, from muster's MUS-9). Fork only, and off unless
//! configured.
//!
//! A TLS front counts the connections it holds open, in total and per peer
//! address, and refuses a new one past a cap by closing it before the
//! handshake, so one flooding peer is turned away while others keep
//! connecting. Plain mode has no fronts, so nothing here runs there.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// `MARSHAL_MAX_CONNECTIONS`: open connections per TLS listener, in total.
pub const TOTAL_ENV: &str = "MARSHAL_MAX_CONNECTIONS";
/// `MARSHAL_MAX_CONNECTIONS_PER_PEER`: open connections per peer address, per TLS listener.
pub const PER_PEER_ENV: &str = "MARSHAL_MAX_CONNECTIONS_PER_PEER";

/// The caps one TLS front applies, if any.
#[derive(Debug, Default)]
pub struct ConnLimits {
    total: Option<usize>,
    per_peer: Option<usize>,
    open: Mutex<HashMap<String, usize>>,
}

/// Holds one connection's places; dropping it frees them.
pub struct ConnGuard {
    limits: Arc<ConnLimits>,
    keys: Vec<String>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut open = self.limits.open.lock().unwrap_or_else(|e| e.into_inner());
        for key in &self.keys {
            if let Some(n) = open.get_mut(key) {
                *n -= 1;
                if *n == 0 {
                    open.remove(key);
                }
            }
        }
    }
}

impl ConnLimits {
    pub fn new(total: Option<usize>, per_peer: Option<usize>) -> Option<Arc<Self>> {
        if total.is_none() && per_peer.is_none() {
            return None;
        }
        Some(Arc::new(Self {
            total,
            per_peer,
            open: Mutex::default(),
        }))
    }

    /// The caps from the environment: a positive integer in each variable,
    /// else no cap of that kind. Neither set means no limits at all.
    pub fn from_env() -> Result<Option<Arc<Self>>, String> {
        let read = |var: &str| -> Result<Option<usize>, String> {
            match std::env::var(var) {
                Ok(v) if v.trim().is_empty() => Ok(None),
                Ok(v) => v
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .map(Some)
                    .ok_or_else(|| format!("{var} must be a positive integer, got {v:?}")),
                Err(_) => Ok(None),
            }
        };
        Ok(Self::new(read(TOTAL_ENV)?, read(PER_PEER_ENV)?))
    }

    /// A place for a connection from `peer`, or `None` when a cap is reached.
    pub fn try_acquire(self: &Arc<Self>, peer: IpAddr) -> Option<ConnGuard> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let total_key = "*".to_string();
        let peer_key = peer.to_string();
        let over = |key: &str, cap: Option<usize>| {
            cap.is_some_and(|cap| open.get(key).copied().unwrap_or(0) >= cap)
        };
        if over(&total_key, self.total) || over(&peer_key, self.per_peer) {
            return None;
        }
        let mut keys = Vec::with_capacity(2);
        if self.total.is_some() {
            *open.entry(total_key.clone()).or_insert(0) += 1;
            keys.push(total_key);
        }
        if self.per_peer.is_some() {
            *open.entry(peer_key.clone()).or_insert(0) += 1;
            keys.push(peer_key);
        }
        Some(ConnGuard {
            limits: self.clone(),
            keys,
        })
    }

    pub fn describe(&self) -> String {
        match (self.total, self.per_peer) {
            (Some(t), Some(p)) => format!("{t} in total, {p} per peer"),
            (Some(t), None) => format!("{t} in total"),
            (None, Some(p)) => format!("{p} per peer"),
            (None, None) => "none".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_hold_per_peer_and_in_total_and_free_on_drop() {
        let limits = ConnLimits::new(Some(3), Some(2)).unwrap();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let a1 = limits.try_acquire(a).unwrap();
        let _a2 = limits.try_acquire(a).unwrap();
        assert!(limits.try_acquire(a).is_none(), "a third from one peer");
        let _b1 = limits.try_acquire(b).unwrap();
        assert!(limits.try_acquire(b).is_none(), "a fourth in total");
        drop(a1);
        assert!(
            limits.try_acquire(b).is_some(),
            "a place freed is a place again"
        );
    }

    #[test]
    fn nothing_configured_means_no_limiter() {
        assert!(ConnLimits::new(None, None).is_none());
        assert_eq!(
            ConnLimits::new(Some(5), None).unwrap().describe(),
            "5 in total"
        );
    }
}
