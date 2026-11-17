//! Send allowances (MAR-88, from muster's MUS-9 and MUS-60). Fork only, and
//! off unless the daemon configures them.
//!
//! Each sending session may have a token bucket: a burst of messages, refilled
//! at a steady rate. Each machine may have one too, shared by all its sessions,
//! so a machine can't multiply its rate by registering more sessions. A message
//! needs room in both. A session that floods gets a clear error and other
//! machines keep their own allowance. A broadcast counts as one message,
//! however many members it reaches. With nothing installed, `take` always
//! allows, which is upstream's behaviour.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A bucket's shape: how many at once, and how many a minute come back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Allowance {
    pub burst: u32,
    pub per_minute: u32,
}

impl Allowance {
    /// Parse `"<burst>/<per minute>"`, both positive integers.
    pub fn parse(text: &str) -> Option<Self> {
        let (burst, per_minute) = text.trim().split_once('/')?;
        let burst = burst.trim().parse().ok().filter(|b| *b > 0)?;
        let per_minute = per_minute.trim().parse().ok().filter(|p| *p > 0)?;
        Some(Self { burst, per_minute })
    }

    fn refill(self) -> Duration {
        Duration::from_secs_f64(60.0 / f64::from(self.per_minute))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Per sending session.
    pub session: Option<Allowance>,
    /// Per machine, shared by its sessions.
    pub machine: Option<Allowance>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Install the allowances. Once; a later call changes nothing.
pub fn install(config: Config) {
    let _ = CONFIG.set(config);
}

/// Buckets untouched this long are full again, so they're dropped.
const IDLE: Duration = Duration::from_secs(10 * 60);

type Buckets = HashMap<String, (f64, Instant)>;
static BUCKETS: Mutex<Option<Buckets>> = Mutex::new(None);

/// Take one message from `sender`'s allowance and from `machine`'s, or say how
/// long until both have one. Nothing installed means nothing taken.
pub fn take(sender: &str, machine: Option<&str>) -> Result<(), String> {
    match CONFIG.get() {
        Some(config) => take_at(*config, sender, machine, Instant::now()),
        None => Ok(()),
    }
}

/// A bucket's tokens after refilling it to `now`.
fn level(buckets: &mut Buckets, key: &str, allowance: Allowance, now: Instant) -> f64 {
    let burst = f64::from(allowance.burst);
    let (tokens, at) = buckets.entry(key.to_string()).or_insert((burst, now));
    let refilled = now.duration_since(*at).as_secs_f64() / allowance.refill().as_secs_f64();
    *tokens = (*tokens + refilled).min(burst);
    *at = now;
    *tokens
}

fn take_at(
    config: Config,
    sender: &str,
    machine: Option<&str>,
    now: Instant,
) -> Result<(), String> {
    let mut guard = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
    let buckets = guard.get_or_insert_with(HashMap::new);
    if buckets.len() > 1024 {
        buckets.retain(|_, (_, at)| now.duration_since(*at) < IDLE);
    }
    let session_key = format!("session:{sender}");
    let machine_key = machine.map(|m| format!("machine:{m}"));
    let wait_for = |tokens: f64, allowance: Allowance| {
        ((1.0 - tokens) * allowance.refill().as_secs_f64()).ceil()
    };
    if let Some(allowance) = config.session {
        let tokens = level(buckets, &session_key, allowance, now);
        if tokens < 1.0 {
            return Err(format!(
                "too many messages from this session: the limit is {} at once and {} a minute; \
                 try again in {:.0} s",
                allowance.burst,
                allowance.per_minute,
                wait_for(tokens, allowance)
            ));
        }
    }
    if let (Some(allowance), Some(key)) = (config.machine, machine_key.as_deref()) {
        let tokens = level(buckets, key, allowance, now);
        if tokens < 1.0 {
            return Err(format!(
                "too many messages from this machine's sessions together: the limit is {} at \
                 once and {} a minute; try again in {:.0} s",
                allowance.burst,
                allowance.per_minute,
                wait_for(tokens, allowance)
            ));
        }
    }
    if config.session.is_some()
        && let Some((tokens, _)) = buckets.get_mut(&session_key)
    {
        *tokens -= 1.0;
    }
    if config.machine.is_some()
        && let Some(key) = machine_key
        && let Some((tokens, _)) = buckets.get_mut(&key)
    {
        *tokens -= 1.0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            session: Some(Allowance {
                burst: 3,
                per_minute: 30,
            }),
            machine: Some(Allowance {
                burst: 4,
                per_minute: 60,
            }),
        }
    }

    #[test]
    fn an_allowance_parses_as_burst_over_per_minute() {
        assert_eq!(
            Allowance::parse(" 20/30 "),
            Some(Allowance {
                burst: 20,
                per_minute: 30
            })
        );
        for bad in ["20", "0/30", "20/0", "a/b", "20/30/40"] {
            assert_eq!(Allowance::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_flooding_session_is_refused_and_others_are_not() {
        let t0 = Instant::now();
        let c = config();
        for _ in 0..3 {
            take_at(c, "rate-flood", None, t0).unwrap();
        }
        let refused = take_at(c, "rate-flood", None, t0).unwrap_err();
        assert!(
            refused.contains("too many messages from this session"),
            "{refused}"
        );
        take_at(c, "rate-other", None, t0).expect("another sender keeps its allowance");
        // 30 a minute: one comes back every 2 s.
        take_at(c, "rate-flood", None, t0 + Duration::from_secs(2)).expect("one back");
        assert!(take_at(c, "rate-flood", None, t0 + Duration::from_secs(2)).is_err());
    }

    #[test]
    fn a_machines_sessions_share_its_allowance() {
        let t0 = Instant::now();
        let c = config();
        // Four sessions, one message each: the machine's burst of 4 is spent.
        for n in 0..4 {
            take_at(c, &format!("rate-m-{n}"), Some("rate-laptop"), t0).unwrap();
        }
        let refused = take_at(c, "rate-m-5", Some("rate-laptop"), t0).unwrap_err();
        assert!(
            refused.contains("this machine's sessions together"),
            "{refused}"
        );
        take_at(c, "rate-m-5", Some("rate-desk"), t0).expect("another machine is fine");
    }

    #[test]
    fn with_no_machine_only_the_session_counts() {
        let t0 = Instant::now();
        let c = Config {
            session: None,
            machine: Some(Allowance {
                burst: 1,
                per_minute: 1,
            }),
        };
        for _ in 0..5 {
            take_at(c, "rate-free", None, t0).expect("no machine, no machine limit");
        }
    }
}
