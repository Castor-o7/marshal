//! The operator's controls over the running daemon, read from files in its
//! state directory (MAR-46, from muster's MUS-105). Writing the files needs the
//! daemon host's filesystem, so a connected client can't trigger them.
//!
//! - `forget-sessions`: sessions the operator asked to remove, one
//!   `<session id, 8+ char id prefix or nickname>\t<ms>\t<token>` per line,
//!   which `marshal-daemon forget` appends. Each line is kept for
//!   [`forget_hold`].
//! - `forget-results`: the running daemon's answer to each request, one
//!   `<token>\t<ok|refused>\t<text>` per line, which `forget` waits for.
//!
//! The running daemon reads them every [`REFRESH`]. The state directory is
//! created `0700`, and a control file that's group- or world-writable, or
//! owned by another user, is ignored with a warning.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyphae::{Gettable as _, Materialize as _};
use marshal_entities::{Session, SessionNickname};
use myko::{core::item::Eventable as _, server::MykoServerContext, utils::downcast_item};

pub const FORGET_FILE: &str = "forget-sessions";
pub const RESULTS_FILE: &str = "forget-results";
/// How soon the running daemon sees a change.
pub const REFRESH: Duration = Duration::from_secs(2);
/// How long a forgotten session's hooks are refused by default, so a waiter
/// still polling under its id can't put the row straight back.
pub const DEFAULT_FORGET_HOLD: Duration = Duration::from_secs(10 * 60);

/// The hold window: `MARSHAL_FORGET_HOLD_SECS`, else [`DEFAULT_FORGET_HOLD`].
pub fn forget_hold() -> Duration {
    hold_from(std::env::var("MARSHAL_FORGET_HOLD_SECS").ok().as_deref())
}

fn hold_from(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_FORGET_HOLD)
}

/// The daemon's state directory: `MARSHAL_STATE_DIR`, else
/// `$XDG_STATE_HOME/marshal`, else `~/.local/state/marshal`. Not created here.
pub fn state_dir() -> Option<PathBuf> {
    let nonempty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(dir) = nonempty("MARSHAL_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = nonempty("XDG_STATE_HOME") {
        return Some(PathBuf::from(xdg).join("marshal"));
    }
    nonempty("HOME")
        .or_else(|| nonempty("USERPROFILE"))
        .map(|h| {
            PathBuf::from(h)
                .join(".local")
                .join("state")
                .join("marshal")
        })
}

/// Create the state directory, private to this user.
pub fn ensure_state_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Why a control file can't be trusted, or `None` if it can. Unix only: a file
/// another user can write could forget any session.
fn untrusted(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            return Some("it's a symlink".into());
        }
        // SAFETY: geteuid has no preconditions and can't fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Some(format!("it's owned by uid {}, not {me}", meta.uid()));
        }
        if meta.mode() & 0o022 != 0 {
            return Some(format!(
                "its mode {:o} lets others write it",
                meta.mode() & 0o777
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    None
}

fn read_control(path: &Path) -> String {
    if !path.exists() {
        return String::new();
    }
    if let Some(why) = untrusted(path) {
        log::warn!("[controls] ignoring {}: {why}", path.display());
        return String::new();
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

fn append_private(path: &Path, line: &str) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    writeln!(options.open(path)?, "{line}")
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if text.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    {
        use std::io::Write as _;
        options.open(&tmp)?.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, path)
}

/// Ask the running daemon to forget a session: `target` is its id, an id
/// prefix of at least 8 characters, or its nickname. Returns the request's
/// token, which its answer in `forget-results` names.
pub fn request_forget(dir: &Path, target: &str) -> io::Result<String> {
    ensure_state_dir(dir)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now().timestamp_millis();
    append_private(
        &dir.join(FORGET_FILE),
        &format!("{}\t{now}\t{token}", target.trim()),
    )?;
    Ok(token)
}

/// The running daemon's answer to request `token`: `(accepted, text)`.
pub fn forget_result(dir: &Path, token: &str) -> Option<(bool, String)> {
    read_control(&dir.join(RESULTS_FILE)).lines().find_map(|l| {
        let mut parts = l.splitn(3, '\t');
        (parts.next()? == token).then_some(())?;
        let ok = parts.next()? == "ok";
        Some((ok, parts.next().unwrap_or_default().to_string()))
    })
}

/// One line of `forget-sessions`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    target: String,
    at: i64,
    token: String,
}

fn requests(dir: &Path) -> Vec<Request> {
    read_control(&dir.join(FORGET_FILE))
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let target = parts.next()?.trim().to_string();
            let at = parts.next()?.trim().parse().ok()?;
            let token = parts.next().unwrap_or_default().trim().to_string();
            (!target.is_empty()).then_some(Request { target, at, token })
        })
        .collect()
}

/// Session ids whose hooks are refused while their forget holds.
static HELD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Whether the operator forgot this session within the hold window.
pub fn is_forgotten(session_id: &str) -> bool {
    HELD.lock()
        .map(|h| h.iter().any(|id| id == session_id))
        .unwrap_or(false)
}

/// Which session a forget names: an exact id, else the one session on the
/// roster with that nickname, else the one id that starts with it (at least 8
/// characters). `Err` holds the matches when it names several, which removes
/// nothing: a guess could remove a live session that shares a ghost's name.
pub fn resolve_target(
    target: &str,
    ids: &[String],
    nicknames: &[(String, String)],
) -> Result<Option<String>, Vec<String>> {
    if ids.iter().any(|id| id == target) {
        return Ok(Some(target.to_string()));
    }
    let one = |mut found: Vec<String>| match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(found),
    };
    let named: Vec<String> = nicknames
        .iter()
        .filter(|(id, nick)| nick == target && ids.contains(id))
        .map(|(id, _)| id.clone())
        .collect();
    if !named.is_empty() || target.len() < 8 {
        return one(named);
    }
    one(ids
        .iter()
        .filter(|id| id.starts_with(target))
        .cloned()
        .collect())
}

/// Marks a control-file line already resolved to a full session id.
const RESOLVED: &str = "id:";

/// Whether `s` could be a full session id this daemon stores, so a forget of
/// one that's already off the roster is still held.
fn plausible_full_id(s: &str) -> bool {
    s.len() >= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What one refresh does with the requests: the lines to keep, the ids to
/// hold, and an answer for each request answered this time.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    kept: Vec<Request>,
    held: Vec<String>,
    answers: Vec<(String, bool, String)>,
}

/// Resolve each request once. A line naming a session by nickname or prefix is
/// rewritten to `id:<full id>` at its first refresh, and from then on held and
/// deleted only by that id: resolving it again after the row is gone would lose
/// the hold, and could pick a new session given the freed nickname. A target
/// naming nothing, or several sessions, is dropped and answered as refused.
fn plan(lines: &[Request], ids: &[String], nicknames: &[(String, String)], hold: Duration) -> Plan {
    let mut out = Plan::default();
    for r in lines {
        if let Some(id) = r.target.strip_prefix(RESOLVED) {
            out.held.push(id.to_string());
            out.kept.push(r.clone());
            continue;
        }
        let id = match resolve_target(&r.target, ids, nicknames) {
            Ok(Some(id)) => id,
            Ok(None) if plausible_full_id(&r.target) => r.target.clone(),
            Ok(None) => {
                out.answers.push((
                    r.token.clone(),
                    false,
                    format!(
                        "not forgetting {}: it names no session on the roster",
                        r.target
                    ),
                ));
                continue;
            }
            Err(matches) => {
                out.answers.push((
                    r.token.clone(),
                    false,
                    format!(
                        "not forgetting {}: it names {} sessions ({}); forget one by its id",
                        r.target,
                        matches.len(),
                        matches.join(", ")
                    ),
                ));
                continue;
            }
        };
        out.answers.push((
            r.token.clone(),
            true,
            format!(
                "forgot session {id}: its roster row is removed, and its hooks are refused for {} s",
                hold.as_secs()
            ),
        ));
        out.kept.push(Request {
            target: format!("{RESOLVED}{id}"),
            at: r.at,
            token: r.token.clone(),
        });
        out.held.push(id);
    }
    out
}

fn roster(ctx: &MykoServerContext) -> (Vec<String>, Vec<(String, String)>) {
    let ids = ctx
        .registry
        .get(Session::ENTITY_NAME_STATIC)
        .map(|s| {
            s.entries()
                .materialize()
                .get()
                .into_iter()
                .map(|(id, _)| id.to_string())
                .collect()
        })
        .unwrap_or_default();
    let nicknames = ctx
        .registry
        .get(SessionNickname::ENTITY_NAME_STATIC)
        .map(|s| {
            s.entries()
                .materialize()
                .get()
                .into_iter()
                .filter_map(|(id, item)| {
                    downcast_item::<SessionNickname>(&item)
                        .map(|n| (id.to_string(), n.nickname.clone()))
                })
                .collect()
        })
        .unwrap_or_default();
    (ids, nicknames)
}

/// Apply the control files once: remove the sessions the operator forgot, hold
/// their ids off, and answer each new request.
pub fn apply(dir: &Path, ctx: &MykoServerContext) {
    let hold = forget_hold();
    let now = chrono::Utc::now().timestamp_millis();
    let all = requests(dir);
    let live: Vec<Request> = all
        .iter()
        .filter(|r| now - r.at < hold.as_millis() as i64)
        .cloned()
        .collect();
    let (ids, nicknames) = if live.is_empty() {
        Default::default()
    } else {
        roster(ctx)
    };
    let plan = plan(&live, &ids, &nicknames, hold);
    // Deleted again on every refresh while the forget holds, should anything
    // have put the row back; only ever by the id resolved at the first refresh.
    for id in &plan.held {
        if ids.contains(id) {
            match ctx.del_by_id(Session::ENTITY_NAME_STATIC, id) {
                Ok(()) => {
                    log::warn!("[controls] forgot session {id} at the operator's request");
                    crate::auth::session_binding::release(id);
                }
                Err(e) => log::warn!("[controls] couldn't forget session {id}: {e}"),
            }
        }
    }
    for (_, ok, text) in &plan.answers {
        if *ok {
            log::info!("[controls] {text}");
        } else {
            log::warn!("[controls] {text}");
        }
    }
    if plan.kept != all {
        let text: String = plan
            .kept
            .iter()
            .map(|r| format!("{}\t{}\t{}\n", r.target, r.at, r.token))
            .collect();
        if let Err(e) = write_private(&dir.join(FORGET_FILE), &text) {
            log::warn!("[controls] couldn't rewrite {FORGET_FILE}: {e}");
        }
    }
    let answers: Vec<_> = plan
        .answers
        .iter()
        .filter(|(token, _, _)| !token.is_empty())
        .collect();
    if !answers.is_empty() {
        // Keep only the answers of requests still in the file, plus these.
        let keep: std::collections::HashSet<&str> =
            plan.kept.iter().map(|r| r.token.as_str()).collect();
        let mut text: String = read_control(&dir.join(RESULTS_FILE))
            .lines()
            .filter(|l| l.split('\t').next().is_some_and(|t| keep.contains(t)))
            .map(|l| format!("{l}\n"))
            .collect();
        for (token, ok, msg) in answers {
            text.push_str(&format!(
                "{token}\t{}\t{msg}\n",
                if *ok { "ok" } else { "refused" }
            ));
        }
        if let Err(e) = write_private(&dir.join(RESULTS_FILE), &text) {
            log::warn!("[controls] couldn't write {RESULTS_FILE}: {e}");
        }
    }
    if let Ok(mut h) = HELD.lock() {
        *h = plan.held;
    }
}

/// Keep applying the control files, every [`REFRESH`]. Spawn on a tokio task.
pub async fn run(dir: PathBuf, ctx: MykoServerContext) {
    let mut interval = tokio::time::interval(REFRESH);
    loop {
        interval.tick().await;
        apply(&dir, &ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: &str, token: &str) -> Request {
        Request {
            target: target.into(),
            at: 0,
            token: token.into(),
        }
    }

    const HOLD: Duration = Duration::from_secs(600);

    #[test]
    fn a_forget_names_one_session_or_removes_nothing() {
        let ids = vec![
            "aaaaaaaa-1111".to_string(),
            "aaaaaaaa-2222".to_string(),
            "bbbbbbbb-3333".to_string(),
        ];
        let nicks = vec![
            ("aaaaaaaa-1111".to_string(), "ghost".to_string()),
            ("aaaaaaaa-2222".to_string(), "twin".to_string()),
            ("bbbbbbbb-3333".to_string(), "twin".to_string()),
        ];
        assert_eq!(
            resolve_target("aaaaaaaa-1111", &ids, &nicks),
            Ok(Some("aaaaaaaa-1111".into()))
        );
        assert_eq!(
            resolve_target("ghost", &ids, &nicks),
            Ok(Some("aaaaaaaa-1111".into()))
        );
        assert_eq!(
            resolve_target("bbbbbbbb", &ids, &nicks),
            Ok(Some("bbbbbbbb-3333".into()))
        );
        // Ambiguous: a shared nickname, or a prefix two ids share.
        assert!(matches!(resolve_target("twin", &ids, &nicks), Err(m) if m.len() == 2));
        assert!(matches!(resolve_target("aaaaaaaa", &ids, &nicks), Err(m) if m.len() == 2));
        // Too short to be a prefix, and no such nickname.
        assert_eq!(resolve_target("aaaa", &ids, &nicks), Ok(None));
        assert_eq!(resolve_target("nobody", &ids, &nicks), Ok(None));
    }

    #[test]
    fn a_forget_is_resolved_once_then_held_by_id() {
        let ids = vec!["aaaaaaaa-1111".to_string()];
        let nicks = vec![("aaaaaaaa-1111".to_string(), "ghost".to_string())];
        let first = plan(&[req("ghost", "t1")], &ids, &nicks, HOLD);
        assert_eq!(first.held, ["aaaaaaaa-1111"]);
        assert_eq!(
            first.kept,
            [Request {
                target: "id:aaaaaaaa-1111".into(),
                at: 0,
                token: "t1".into()
            }]
        );
        assert!(first.answers[0].1, "{:?}", first.answers);
        // Next refresh: the row is gone, and the freed nickname went to a new
        // session. The hold stays on the old id, and the new one is untouched.
        let ids = vec!["cccccccc-9999".to_string()];
        let nicks = vec![("cccccccc-9999".to_string(), "ghost".to_string())];
        let next = plan(&first.kept, &ids, &nicks, HOLD);
        assert_eq!(next.held, ["aaaaaaaa-1111"]);
        assert!(next.answers.is_empty(), "answered once");
    }

    #[test]
    fn an_ambiguous_or_unknown_forget_is_refused_and_names_the_matches() {
        let ids = vec!["aaaaaaaa-1111".to_string(), "bbbbbbbb-2222".to_string()];
        let nicks = vec![
            ("aaaaaaaa-1111".to_string(), "twin".to_string()),
            ("bbbbbbbb-2222".to_string(), "twin".to_string()),
        ];
        let p = plan(
            &[req("twin", "t1"), req("nobody", "t2")],
            &ids,
            &nicks,
            HOLD,
        );
        assert!(p.held.is_empty() && p.kept.is_empty());
        let (_, ok, text) = &p.answers[0];
        assert!(!ok);
        assert!(
            text.contains("2 sessions")
                && text.contains("aaaaaaaa-1111")
                && text.contains("bbbbbbbb-2222"),
            "{text}"
        );
        assert!(!p.answers[1].1 && p.answers[1].2.contains("names no session"));
    }

    #[test]
    fn the_hold_window_is_a_setting_with_a_ten_minute_default() {
        assert_eq!(hold_from(None), Duration::from_secs(600));
        assert_eq!(hold_from(Some("30")), Duration::from_secs(30));
        assert_eq!(hold_from(Some("0")), Duration::from_secs(600));
        assert_eq!(hold_from(Some("soon")), Duration::from_secs(600));
    }

    #[cfg(unix)]
    #[test]
    fn the_state_dir_is_private_and_a_writable_control_file_is_ignored() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let token = request_forget(&dir, "ghost").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(FORGET_FILE)), 0o600);
        assert_eq!(requests(&dir).len(), 1);
        assert_eq!(requests(&dir)[0].token, token);
        // Made group-writable: it's ignored, so nothing it names is forgotten.
        std::fs::set_permissions(
            dir.join(FORGET_FILE),
            std::fs::Permissions::from_mode(0o620),
        )
        .unwrap();
        assert!(requests(&dir).is_empty());
    }
}
