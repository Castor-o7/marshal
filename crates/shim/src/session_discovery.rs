//! Resolve Claude Code's canonical session_id for THIS shim invocation.
//!
//! The shim is launched as Claude Code's stdio MCP child. Claude does not
//! pass the session_id through env or stdin, so we have to learn it
//! out-of-band. Claude writes a per-session transcript at:
//!
//!     ~/.claude/projects/<encoded_cwd>/<session_id>.jsonl
//!
//! …where `<session_id>` IS the canonical id (a uuid) — the same id the
//! daemon's `/hook/*` endpoints receive in their stdin payload. If the shim
//! adopts that same id, hook and shim collapse to one Session row.
//!
//! ## Disambiguation — multi-agent same-cwd
//!
//! A shim invocation's PARENT process IS its Claude session: stdio MCP
//! children are spawned by the parent's `child_process.spawn`, and POSIX +
//! Windows both expose the parent PID directly. We resolve in three tiers
//! keyed off that parent:
//!
//! 0. **Claude's own per-PID session manifest.** Claude Code (≥ 2.1.172)
//!    writes `~/.claude/sessions/<pid>.json` for every active session
//!    containing `{ pid, sessionId, cwd, startedAt, ... }`. The shim reads
//!    `~/.claude/sessions/<parent_pid>.json` and takes `sessionId`
//!    verbatim. Authoritative, no heuristic involved. This is the
//!    happy path and covers fresh starts, resumes, self-upgrade
//!    restarts, forks, and `--continue` — all of which Claude tracks in
//!    the same per-PID record.
//!
//! 1. **Parent cmdline parse.** Fallback for older Claude versions or
//!    sandboxed environments where `sessions/<pid>.json` is unavailable.
//!    `--resume <path>.jsonl` or `--session-id <uuid>` name the id
//!    literally.
//!
//! 2. **Parent start time → .jsonl creation time correlation.** Last
//!    resort: each fresh session's `.jsonl` is created when claude
//!    starts, so file ctime ≈ parent.started_at. Sibling sessions in
//!    the same cwd have distinct start times so the match is
//!    unambiguous for fresh starts. Self-upgrade / resumed sessions
//!    don't create a new file and therefore can't be matched this way
//!    — tier 0 covers those.
//!
//! Tier 2 uses `Metadata::created()` (statx birthtime on Linux 4.11+,
//! NTFS creation time on Windows). It falls back to `modified()` if
//! birthtime is unavailable, which approximates ctime well for newly
//! created files (mtime ≈ ctime when the first write is near-immediate)
//! and badly for long-running sessions (mtime drifts away from ctime as
//! the file is appended to). The fallback is a degradation, not a
//! correctness guarantee — it's a best-effort branch for old kernels.
//!
//! Both tiers verify cwd-content match on the candidate .jsonl as a
//! consistency check — a parent could in principle have a stale
//! .jsonl path in argv pointing to a different cwd, and we'd rather
//! fail loudly than register under the wrong id.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use marshal_entities::SessionId;

const MAX_WAIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Claude Code exports the canonical session id into the environment of
/// every process it spawns — MCP servers (us) and hooks alike. It is
/// fixed at exec time, so it is present from the shim's very first
/// instant with no file, poll, or parent-PID heuristic involved. This is
/// the primary, race-free discovery path; the `~/.claude` file tiers in
/// `resolve_under` are fallback for older Claude builds that predate this
/// var or sandboxes that strip it. (Unlike the daemon-address value —
/// which we deliberately read from a config file because a stale parent's
/// env can shadow per-host config — this var carries no staleness risk:
/// each Claude process sets it to ITS OWN session id for ITS OWN
/// children, so sibling sessions in one cwd are disambiguated for free.)
const SESSION_ID_ENV: &str = "CLAUDE_CODE_SESSION_ID";
/// Cap lines read per candidate while looking for the cwd marker. The
/// `"cwd":` field shows up within the first ~dozen lines of every real
/// transcript; 200 is well above the worst case and caps I/O if we're
/// pointed at an unusually large unrelated file.
const MAX_HEADER_LINES: usize = 200;
/// How close a candidate .jsonl's creation time must be to its parent
/// claude's process start time to be considered "the file claude opened
/// when this session began". Generous because clock granularity varies
/// and the file's first write can lag the process spawn by a few
/// hundred ms; tight enough to exclude unrelated sibling sessions
/// which differ by minutes-to-hours in practice.
const PARENT_CORRELATION_WINDOW: Duration = Duration::from_secs(30);

pub fn resolve(cwd: &str) -> Option<SessionId> {
    // Primary path: the session id Claude handed us in the environment.
    // Race-free and exact — short-circuits the entire file/heuristic
    // stack below.
    if let Some(sid) = sid_from_env(std::env::var(SESSION_ID_ENV).ok()) {
        log::info!(
            "[marshal-shim] session discovery: {SESSION_ID_ENV} → session_id {}",
            sid.0
        );
        return Some(sid);
    }
    let home = home_dir()?;
    resolve_under(&real_liveness, &home, cwd, MAX_WAIT, real_parent_info)
}

/// This shim's parent process id (the `claude` that spawned it). `None` if the
/// platform can't surface it.
pub(crate) fn parent_pid() -> Option<u32> {
    real_parent_info().map(|p| p.pid)
}

/// The session id Claude CURRENTLY considers canonical for process
/// `parent_pid`, read from its live per-PID manifest
/// (`~/.claude/sessions/<pid>.json`). Unlike the inherited
/// `CLAUDE_CODE_SESSION_ID` env — which is frozen at the shim's spawn and goes
/// STALE after a compact/clear re-mints the id WITHOUT re-spawning the shim —
/// Claude keeps this file current, so it is the authoritative source for
/// detecting canonical-id drift in a long-lived shim. `None` if the manifest
/// is absent (older Claude) or unreadable.
pub(crate) fn canonical_session_id(parent_pid: u32) -> Option<SessionId> {
    let home = home_dir()?;
    sid_from_claude_sessions_file(&real_liveness, &home, parent_pid)
}

/// Test seam: same logic as `resolve()` but takes HOME, max wait, and
/// the parent-info source explicitly. Lets unit tests exercise both
/// tiers without forking a child whose parent PID we control.
pub(crate) fn resolve_under<F>(
    liveness: &dyn Fn(u32) -> Liveness,
    home: &Path,
    cwd: &str,
    max_wait: Duration,
    parent_info: F,
) -> Option<SessionId>
where
    F: Fn() -> Option<ParentInfo>,
{
    let projects = home.join(".claude").join("projects");
    if !projects.is_dir() {
        log::warn!(
            "[marshal-shim] session discovery: {:?} does not exist",
            projects
        );
        return None;
    }
    let parent = parent_info()?;
    log::info!(
        "[marshal-shim] session discovery: parent pid={} started_at={:?} argv_len={}",
        parent.pid,
        parent.started_at,
        parent.cmdline.len()
    );

    // All three tiers are retried inside one poll loop. This is the crux
    // of fresh-launch robustness: on a brand-new session Claude writes
    // both the per-PID manifest (tier 0) and the transcript .jsonl
    // (tier 2) around the same instant it spawns this shim as an MCP
    // child — so a single up-front read of either can lose the race by a
    // few hundred ms. Previously tiers 0 and 1 were checked exactly once
    // before the loop; a near-simultaneous tier-0 manifest write was
    // missed, and the shim fell through to tier-2 ctime correlation,
    // which on a no-prompt-yet fresh session has no .jsonl to match and
    // expired → `resolve()` returned None → the shim bailed and the MCP
    // connection dropped. `/mcp reconnect` "fixed" it only because by
    // then the manifest was long on disk. Polling the authoritative
    // tiers converges the moment the manifest lands (sub-second), with
    // no dependency on the transcript existing.
    let cwd_norm = normalize_cwd(cwd);
    let deadline = Instant::now() + max_wait;
    loop {
        // Tier 0: Claude's own per-PID session manifest. Authoritative.
        if let Some(sid) = sid_from_claude_sessions_file(liveness, home, parent.pid) {
            log::info!(
                "[marshal-shim] session discovery: ~/.claude/sessions/{}.json → session_id {}",
                parent.pid,
                sid.0
            );
            return Some(sid);
        }

        // Tier 1: parent cmdline parse — authoritative. Immutable after
        // exec, so it can only ever succeed on the first pass, but it's
        // cheap to recheck and keeps the precedence order explicit.
        if let Some(stem) = sid_from_cmdline(&parent.cmdline) {
            log::info!(
                "[marshal-shim] session discovery: parent cmdline → session_id {}",
                stem
            );
            let _ = verify_cwd_for_session_id(&projects, &stem, cwd);
            return Some(SessionId(Arc::from(stem.as_str())));
        }

        // Tier 2: ctime correlation against the per-session transcript.
        if let Some(id) = scan_by_parent_correlation(&projects, &cwd_norm, &parent) {
            return Some(id);
        }

        if Instant::now() >= deadline {
            log::warn!(
                "[marshal-shim] session discovery: no manifest, cmdline id, or .jsonl matched \
                 parent (pid={}) within {:?}; giving up",
                parent.pid,
                max_wait
            );
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Parent process snapshot — pid, when it started, and its argv.
/// `started_at` is `None` if the platform can't surface a reliable
/// process start time (very old kernels, sandboxes).
#[derive(Debug)]
pub(crate) struct ParentInfo {
    pub pid: u32,
    pub started_at: Option<SystemTime>,
    pub cmdline: Vec<String>,
}

/// Read `<home>/.claude/sessions/<parent_pid>.json` and pull `sessionId`.
/// This is Claude Code's own per-PID session manifest (≥ 2.1.172) — the
/// canonical source. Returns None if the file doesn't exist, isn't
/// valid JSON, doesn't carry a non-empty `sessionId`, or isn't live (MAR-45, from muster MUS-107,
/// [`manifest_is_live`]): a manifest left by a process that has exited, whose
/// pid another process now holds, names a session that's gone.
pub(crate) fn sid_from_claude_sessions_file(
    liveness: &dyn Fn(u32) -> Liveness,
    home: &Path,
    parent_pid: u32,
) -> Option<SessionId> {
    let path = home
        .join(".claude")
        .join("sessions")
        .join(format!("{parent_pid}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&content).ok()?;
    let sid = manifest.get("sessionId")?.as_str()?;
    if sid.is_empty() {
        return None;
    }
    if !manifest_is_live(&manifest, parent_pid, &liveness(parent_pid)) {
        log::warn!(
            "[marshal-shim] session discovery: {path:?} names session {sid}, but isn't pid \
             {parent_pid}'s: that process isn't the one that wrote it, so it's ignored"
        );
        return None;
    }
    Some(SessionId(Arc::from(sid)))
}

/// What can be learned about a running process, to check a manifest is its own
/// (MAR-45, from muster MUS-107). Each field is `None` where the platform can't say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Liveness {
    /// Whether a process with the pid exists.
    pub running: Option<bool>,
    /// Linux: its start time in clock ticks since boot, field 22 of
    /// `/proc/<pid>/stat`, which Claude records as the manifest's `procStart`.
    pub start_ticks: Option<u64>,
    /// When it started, by the wall clock.
    pub started_at: Option<SystemTime>,
}

/// How much later than the manifest's `startedAt` a process may seem to have
/// started and still be the one that wrote it: wall-clock start times are read
/// to the second on macOS, and Linux's are derived from the uptime.
const STARTED_AT_SLACK: Duration = Duration::from_secs(2);

/// Whether the manifest `m`, read for `pid`, was written by the process now
/// holding that pid (MAR-45, from muster MUS-107). Pids are reused, and Claude leaves a manifest
/// behind when it dies without cleaning up, so a stale manifest can name a
/// session that's gone, for an unrelated process now holding its pid. It's
/// stale if it names another pid; if no process holds the pid; if its
/// `procStart` differs from the process's start ticks; or, where those can't be
/// compared, if the process started after the manifest's `startedAt`, so after
/// the session it describes. Whatever can't be read is taken on trust, so an
/// older Claude's manifest, or a platform that can't say, behaves as before.
pub(crate) fn manifest_is_live(m: &serde_json::Value, pid: u32, live: &Liveness) -> bool {
    if m.get("pid")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|named| named != u64::from(pid))
    {
        return false;
    }
    if live.running == Some(false) {
        return false;
    }
    let proc_start = m.get("procStart").and_then(|v| match v {
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        other => other.as_u64(),
    });
    if let (Some(written), Some(now)) = (proc_start, live.start_ticks) {
        return written == now;
    }
    let started_at = m
        .get("startedAt")
        .and_then(serde_json::Value::as_u64)
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
    if let (Some(session), Some(process)) = (started_at, live.started_at) {
        return process <= session + STARTED_AT_SLACK;
    }
    true
}

/// [`Liveness`] for `pid`, read from the system.
pub(crate) fn real_liveness(pid: u32) -> Liveness {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => Liveness {
                running: Some(true),
                start_ticks: linux_start_ticks(&stat),
                started_at: linux_process_start_time(pid),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness {
                running: Some(false),
                ..Liveness::default()
            },
            Err(_) => Liveness::default(),
        }
    }
    #[cfg(target_os = "macos")]
    {
        Liveness {
            running: unix_pid_running(pid),
            start_ticks: None,
            started_at: macos_process_start_time(pid),
        }
    }
    #[cfg(windows)]
    {
        Liveness {
            running: None,
            start_ticks: None,
            started_at: windows_process_start_time(pid),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        Liveness::default()
    }
}

/// Whether a process holds `pid`, by `kill(pid, 0)`: `ESRCH` means none does,
/// and `EPERM` that one does under another user. `None` for anything else.
#[cfg(target_os = "macos")]
fn unix_pid_running(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(false),
        Some(libc::EPERM) => Some(true),
        _ => None,
    }
}

/// Field 22 of a `/proc/<pid>/stat` line, the process's start time in clock
/// ticks since boot. The comm field is in parentheses and may hold spaces, so
/// the count starts after the last `)`.
#[cfg(any(target_os = "linux", test))]
fn linux_start_ticks(stat: &str) -> Option<u64> {
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(19)?.parse().ok()
}

/// The live fields marshal scrapes from Claude's per-PID manifest
/// (`<home>/.claude/sessions/<parent_pid>.json`). Best-effort: any absent
/// field is `None`, and the whole struct is `None` only when the manifest is
/// missing (older Claude / Codex, which has none) or unparseable.
pub(crate) struct ManifestFields {
    /// `name` — the operator's `/rename` value, or Claude's auto-title.
    pub name: Option<String>,
    /// `status` — live turn-state: `busy` / `idle` / `shell`.
    pub activity: Option<String>,
    /// `kind` — `interactive` / `bg`. Static over a session's life.
    pub kind: Option<String>,
    /// `statusUpdatedAt` — epoch millis when Claude last set `status`.
    pub status_updated_at: Option<i64>,
}

/// `Session::activity_at` (MAR-58) for a session whose activity is `activity`:
/// the manifest's `statusUpdatedAt` when it has one, so a shim that starts (or
/// re-registers) after the change still reports when it happened; else `now_ms`,
/// the moment the shim saw it. `None` when there's no activity to date.
pub(crate) fn activity_at(
    activity: Option<&str>,
    status_updated_at: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    activity?;
    Some(status_updated_at.unwrap_or(now_ms))
}

/// Parse the live fields from the per-PID manifest. Full JSON parse (not the
/// `sessionId` cheap-scan above): `name` is arbitrary human text that can
/// contain quotes, escapes, or `}` — a substring scan would mis-slice it. The
/// file is ~300 bytes so the cost is trivial. Read on the publisher tick AND
/// on an fs-watch wake, so a mid-session `/rename` or busy↔idle flip
/// propagates to the roster in near real time.
pub(crate) fn manifest_fields(parent_pid: u32) -> Option<ManifestFields> {
    let content = std::fs::read_to_string(session_manifest_path(parent_pid)?).ok()?;
    parse_manifest_fields(&content)
}

/// The parsing half of [`manifest_fields`], separate so tests needn't write a
/// manifest under a fake home.
fn parse_manifest_fields(content: &str) -> Option<ManifestFields> {
    let m: serde_json::Value = serde_json::from_str(content).ok()?;
    let str_field = |k: &str| {
        m.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(ManifestFields {
        name: str_field("name"),
        activity: str_field("status"),
        kind: str_field("kind"),
        status_updated_at: m
            .get("statusUpdatedAt")
            .and_then(|v| v.as_i64())
            .filter(|t| *t > 0),
    })
}

/// Path to Claude's per-PID manifest for `parent_pid`.
fn session_manifest_path(parent_pid: u32) -> Option<PathBuf> {
    Some(
        home_dir()?
            .join(".claude")
            .join("sessions")
            .join(format!("{parent_pid}.json")),
    )
}

/// The directory holding Claude's per-PID manifests — the fs-watch target for
/// event-driven turn-state / name updates. `None` if the home dir is unknown.
pub(crate) fn sessions_dir() -> Option<PathBuf> {
    Some(home_dir()?.join(".claude").join("sessions"))
}

/// Extract a `SessionId` from the raw `CLAUDE_CODE_SESSION_ID` value.
/// Returns None for an absent or whitespace-only value so we fall
/// through to the file-based tiers. Pure (takes the value rather than
/// reading the env) so it's testable without mutating process env —
/// which matters because the test runner is itself a Claude child and
/// has this var set.
fn sid_from_env(val: Option<String>) -> Option<SessionId> {
    let v = val?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    Some(SessionId(Arc::from(v)))
}

fn sid_from_cmdline(args: &[String]) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        // `--session-id <uuid>` or `--session-id=<uuid>`
        if a == "--session-id"
            && let Some(v) = iter.peek()
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
        if let Some(v) = a.strip_prefix("--session-id=")
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
        // `--resume <path>` or `--resume=<path>`. The session id is the
        // .jsonl filename stem. Claude also accepts a bare session id
        // (no path) — handle both: if it parses as a uuid-shaped token,
        // use as-is; otherwise treat as a path and take the stem.
        if a == "--resume"
            && let Some(v) = iter.peek()
            && !v.is_empty()
        {
            return Some(extract_session_id_from_resume_arg(v));
        }
        if let Some(v) = a.strip_prefix("--resume=")
            && !v.is_empty()
        {
            return Some(extract_session_id_from_resume_arg(v));
        }
    }
    None
}

fn extract_session_id_from_resume_arg(v: &str) -> String {
    // If it contains a path separator OR ends with .jsonl, treat as path.
    if v.contains(['/', '\\']) || v.ends_with(".jsonl") {
        Path::new(v)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(v)
            .to_string()
    } else {
        v.to_string()
    }
}

fn verify_cwd_for_session_id(projects: &Path, session_id: &str, cwd: &str) -> bool {
    let target = format!("{session_id}.jsonl");
    let cwd_norm = normalize_cwd(cwd);
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return false;
    };
    for dir in dirs.flatten() {
        if !dir.path().is_dir() {
            continue;
        }
        let candidate = dir.path().join(&target);
        if candidate.exists() && jsonl_matches_cwd(&candidate, &cwd_norm) {
            return true;
        }
    }
    false
}

fn scan_by_parent_correlation(
    projects: &Path,
    cwd: &str,
    parent: &ParentInfo,
) -> Option<SessionId> {
    let parent_started = parent.started_at?;
    let mut best: Option<(Duration, PathBuf)> = None;

    let project_dirs = std::fs::read_dir(projects).ok()?;
    for project_dir in project_dirs.flatten() {
        if !project_dir.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project_dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            // Prefer created() (statx birthtime on Linux, NTFS creation
            // time on Windows). Fall back to modified() if unavailable
            // — degradation, not correctness.
            let file_t = meta.created().or_else(|_| meta.modified()).ok()?;

            let delta = abs_delta(file_t, parent_started);
            if delta > PARENT_CORRELATION_WINDOW {
                continue;
            }
            if !jsonl_matches_cwd(&p, cwd) {
                continue;
            }

            match &best {
                None => best = Some((delta, p)),
                Some((best_delta, _)) if delta < *best_delta => best = Some((delta, p)),
                _ => {}
            }
        }
    }

    let (delta, path) = best?;
    let stem = path.file_stem()?.to_str()?;
    log::info!(
        "[marshal-shim] session discovery: parent-correlation matched {} (Δ {:?}) → session_id {}",
        path.display(),
        delta,
        stem
    );
    Some(SessionId(Arc::from(stem)))
}

fn abs_delta(a: SystemTime, b: SystemTime) -> Duration {
    if a >= b {
        a.duration_since(b).unwrap_or(Duration::ZERO)
    } else {
        b.duration_since(a).unwrap_or(Duration::ZERO)
    }
}

fn jsonl_matches_cwd(path: &Path, cwd: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // Claude Code writes cwd as a JSON-string value. JSON-escape
    // backslashes (Windows) and double-quotes so the substring match is
    // exact.
    let needle = format!("\"cwd\":\"{}\"", json_escape(cwd));
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        if i >= MAX_HEADER_LINES {
            break;
        }
        let Ok(line) = line else { continue };
        if line.contains(&needle) {
            return true;
        }
    }
    false
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

fn normalize_cwd(cwd: &str) -> String {
    cwd.to_string()
}

// ── Platform: real ParentInfo lookup ──────────────────────────────────

#[cfg(target_os = "linux")]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = unsafe { libc::getppid() } as u32;
    Some(ParentInfo {
        pid: ppid,
        started_at: linux_process_start_time(ppid),
        cmdline: linux_process_cmdline(ppid),
    })
}

#[cfg(target_os = "linux")]
fn linux_process_cmdline(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_process_start_time(pid: u32) -> Option<SystemTime> {
    // /proc/<pid>/stat is space-separated except for the comm field which
    // is wrapped in parens and can contain spaces. Skip past the last
    // ')' before splitting; starttime is field 22 overall = index 19
    // in the post-comm tail (state, ppid, pgrp, … starttime).
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let starttime_ticks = linux_start_ticks(&stat)?;

    // Boot time = now - uptime. /proc/uptime is "<seconds_up> <idle>".
    let uptime_raw = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_raw.split_whitespace().next()?.parse().ok()?;
    let boot = SystemTime::now().checked_sub(Duration::from_secs_f64(uptime_secs))?;

    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    if ticks_per_sec == 0 {
        return None;
    }
    let starttime_secs = starttime_ticks / ticks_per_sec;
    let starttime_frac_ticks = starttime_ticks % ticks_per_sec;
    let starttime_nanos =
        (starttime_frac_ticks as u128 * 1_000_000_000u128 / ticks_per_sec as u128) as u32;
    boot.checked_add(Duration::new(starttime_secs, starttime_nanos))
}

#[cfg(target_os = "macos")]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = unsafe { libc::getppid() } as u32;
    // macOS doesn't expose start_time via /proc. `ps -o lstart -p <pid>`
    // and sysctl KERN_PROC are both available; sysctl is preferred but
    // requires more FFI. Use `ps` for parity with statefile's macOS
    // helper — pulse-deploy doesn't run shims on macOS today, so this
    // path is a best-effort placeholder.
    let started_at = macos_process_start_time(ppid);
    let cmdline = macos_process_cmdline(ppid);
    Some(ParentInfo {
        pid: ppid,
        started_at,
        cmdline,
    })
}

#[cfg(target_os = "macos")]
fn macos_process_start_time(pid: u32) -> Option<SystemTime> {
    // `lstart` is printed in the local time zone, with no offset, so `ps` is
    // run in UTC to print the time [`parse_lstart_utc`] reads. Parsed as UTC
    // while printed in local time, a Mac east of UTC saw a live Claude as
    // starting hours after its manifest's `startedAt`, and refused it (MAR-45, from muster MUS-107).
    // The day and month names follow the locale, so it's the C locale's.
    let out = std::process::Command::new("ps")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_lstart_utc(&String::from_utf8(out.stdout).ok()?)
}

/// A `ps -o lstart` time, such as "Tue Jun 10 17:31:30 2026", printed in UTC.
#[cfg(any(target_os = "macos", test))]
fn parse_lstart_utc(s: &str) -> Option<SystemTime> {
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%a %b %e %H:%M:%S %Y")
        .ok()
        .and_then(|dt| dt.and_utc().timestamp_millis().try_into().ok())
        .and_then(|millis: u64| SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis)))
}

#[cfg(target_os = "macos")]
fn macos_process_cmdline(pid: u32) -> Vec<String> {
    let out = match std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // ps command= returns a single space-separated line; argv reconstruction
    // is imprecise but adequate for `--session-id <uuid>` / `--resume <path>`
    // detection (neither flag's value should contain a literal space).
    s.trim().split_whitespace().map(str::to_string).collect()
}

#[cfg(windows)]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = windows_parent_pid()?;
    Some(ParentInfo {
        pid: ppid,
        started_at: windows_process_start_time(ppid),
        cmdline: windows_process_cmdline(ppid),
    })
}

#[cfg(windows)]
pub(crate) fn windows_parent_pid() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let my_pid = unsafe { GetCurrentProcessId() };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut found = None;
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == my_pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(windows)]
fn windows_process_start_time(pid: u32) -> Option<SystemTime> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        // FILETIME is 100-ns intervals since 1601-01-01 UTC. Convert to
        // Unix epoch (1970-01-01 UTC).
        let raw = (creation.dwHighDateTime as u64) << 32 | (creation.dwLowDateTime as u64);
        // Filetime epoch to Unix epoch in 100-ns intervals.
        const FT_TO_UNIX: u64 = 11_644_473_600 * 10_000_000;
        if raw < FT_TO_UNIX {
            return None;
        }
        let unix_100ns = raw - FT_TO_UNIX;
        let secs = unix_100ns / 10_000_000;
        let nanos = (unix_100ns % 10_000_000) as u32 * 100;
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
    }
}

#[cfg(windows)]
fn windows_process_cmdline(_pid: u32) -> Vec<String> {
    // Reading another process's command line on Windows requires either
    // NtQueryInformationProcess (ProcessBasicInformation +
    // ReadProcessMemory of the PEB), or WMI (Win32_Process.CommandLine),
    // or PSAPI. Neither is in windows-sys's surface area without
    // additional features. We omit cmdline parsing on Windows for now;
    // the start-time correlation tier covers the dominant case (fresh
    // sessions). `--resume` / `--session-id` flags on Windows fall
    // through to ctime correlation, which is still correct as long as
    // the resumed .jsonl's creation time matches the parent's start
    // time within `PARENT_CORRELATION_WINDOW` — which it does, because
    // claude.exe's start time is what we're correlating against, not
    // the .jsonl's original creation time.
    //
    // TODO: add NtQueryInformationProcess-based reader when we hit a
    // case where the correlation fails on Windows.
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn real_parent_info() -> Option<ParentInfo> {
    None
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    let mut full = PathBuf::from(drive);
    full.push(path);
    Some(full)
}

#[cfg(not(any(unix, windows)))]
fn home_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe that can say nothing, so the manifest is taken on trust, as before
    /// MAR-45, from muster MUS-107: these tests' parent pids are made up.
    fn unprobed(_pid: u32) -> Liveness {
        Liveness::default()
    }

    fn write_manifest(home: &Path, pid: u32, body: &str) {
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join(format!("{pid}.json")), body).unwrap();
    }

    /// MAR-45, from muster MUS-107: a manifest whose `procStart` matches the process holding its pid
    /// is trusted; one left by an exited process, whose pid another now holds,
    /// isn't; and a missing one gives nothing, as before.
    #[test]
    fn only_a_live_manifest_names_the_session() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let body = r#"{"pid":4017,"sessionId":"live-id","startedAt":1794011880448,"procStart":"192149093"}"#;
        write_manifest(&home, 4017, body);
        let id = |probe: &dyn Fn(u32) -> Liveness| {
            sid_from_claude_sessions_file(probe, &home, 4017).map(|s| s.0.to_string())
        };

        let live = |_| Liveness {
            running: Some(true),
            start_ticks: Some(192_149_093),
            started_at: None,
        };
        assert_eq!(id(&live).as_deref(), Some("live-id"));

        // The pid was reused: another process, started later, holds it now.
        let reused = |_| Liveness {
            running: Some(true),
            start_ticks: Some(192_990_001),
            started_at: None,
        };
        assert_eq!(id(&reused), None, "a reused pid passed as the session's");

        let gone = |_| Liveness {
            running: Some(false),
            ..Liveness::default()
        };
        assert_eq!(id(&gone), None, "no process holds the pid");

        // Can't read the start time: trusted, as before.
        let unreadable = |_| Liveness {
            running: Some(true),
            ..Liveness::default()
        };
        assert_eq!(id(&unreadable).as_deref(), Some("live-id"));

        assert_eq!(
            sid_from_claude_sessions_file(&live, &home, 4018),
            None,
            "no manifest"
        );
    }

    /// Without `procStart` (or start ticks, off Linux), the wall-clock start
    /// decides: a process that started after the session began isn't its own.
    #[test]
    fn without_start_ticks_a_process_younger_than_the_session_is_stale() {
        let started = 1_794_011_880_448u64;
        let m = serde_json::json!({"pid": 7, "sessionId": "s", "startedAt": started});
        let at = |ms: u64| Liveness {
            running: Some(true),
            start_ticks: Some(5),
            started_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
        };
        assert!(
            manifest_is_live(&m, 7, &at(started - 1_500)),
            "Claude starts, then writes"
        );
        assert!(
            manifest_is_live(&m, 7, &at(started + 1_000)),
            "within the slack"
        );
        assert!(
            !manifest_is_live(&m, 7, &at(started + 60_000)),
            "a later process"
        );
        assert!(
            !manifest_is_live(&m, 8, &at(started)),
            "another pid's manifest"
        );
        assert!(
            manifest_is_live(
                &serde_json::json!({"sessionId": "s"}),
                7,
                &at(started + 60_000)
            ),
            "an older Claude's manifest, with neither, is trusted"
        );
    }

    #[test]
    fn lstart_is_read_as_utc() {
        // 2026-11-07T02:55:15Z, as `TZ=UTC ps -o lstart=` prints it.
        let at = parse_lstart_utc("Sat Nov  7 02:55:15 2026\n").expect("parses");
        assert_eq!(
            at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
            1_794_020_115
        );
        assert_eq!(parse_lstart_utc("not a time"), None);
    }

    /// On the platform that runs `ps` for it, the start time read for this
    /// process is close to the truth, whatever the local time zone.
    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_start_time_is_recent_in_any_zone() {
        let started = macos_process_start_time(std::process::id()).expect("ps lstart");
        let age = SystemTime::now()
            .duration_since(started)
            .unwrap_or_default();
        assert!(
            age < Duration::from_secs(3600),
            "{age:?}: lstart was read in the wrong zone"
        );
        assert!(
            started <= SystemTime::now() + Duration::from_secs(2),
            "lstart read as later than now: the wrong zone"
        );
    }

    #[test]
    fn start_ticks_are_field_22_after_the_comm() {
        let stat =
            "4017 (claude (2) x) S 1 4017 4017 0 -1 4194560 1 0 0 0 1 2 0 0 20 0 9 0 192149093 1 2";
        assert_eq!(linux_start_ticks(stat), Some(192_149_093));
        assert_eq!(linux_start_ticks("garbage"), None);
    }

    /// This process is live, and its own probe says so, with start ticks on Linux.
    #[test]
    fn the_real_probe_sees_this_process() {
        let me = std::process::id();
        let live = real_liveness(me);
        assert_ne!(live.running, Some(false));
        #[cfg(target_os = "linux")]
        {
            let ticks = live.start_ticks.expect("start ticks on Linux");
            let m = serde_json::json!({"pid": me, "procStart": ticks.to_string()});
            assert!(manifest_is_live(&m, me, &live));
            let m = serde_json::json!({"pid": me, "procStart": (ticks + 1).to_string()});
            assert!(!manifest_is_live(&m, me, &live));
        }
    }
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // ── MAR-58: when a session's activity last changed ────────────────

    #[test]
    fn manifest_fields_read_status_updated_at() {
        let m = parse_manifest_fields(
            r#"{"pid":1,"sessionId":"s","status":"idle","statusUpdatedAt":1794186070745}"#,
        )
        .unwrap();
        assert_eq!(m.activity.as_deref(), Some("idle"));
        assert_eq!(m.status_updated_at, Some(1_794_186_070_745));
    }

    #[test]
    fn manifest_fields_without_a_usable_status_updated_at() {
        for json in [
            r#"{"status":"busy"}"#,
            r#"{"status":"busy","statusUpdatedAt":null}"#,
            r#"{"status":"busy","statusUpdatedAt":"1794186070745"}"#,
            r#"{"status":"busy","statusUpdatedAt":0}"#,
        ] {
            let m = parse_manifest_fields(json).unwrap();
            assert_eq!(m.activity.as_deref(), Some("busy"), "{json}");
            assert_eq!(m.status_updated_at, None, "{json}");
        }
    }

    #[test]
    fn activity_at_takes_the_manifest_time_over_the_shim_clock() {
        // A shim that starts, or re-registers, long after the change keeps
        // the manifest's time rather than resetting it to now.
        assert_eq!(activity_at(Some("idle"), Some(1_000), 9_000), Some(1_000));
    }

    #[test]
    fn activity_at_falls_back_to_the_shim_clock() {
        assert_eq!(activity_at(Some("busy"), None, 9_000), Some(9_000));
    }

    #[test]
    fn no_activity_has_no_activity_at() {
        assert_eq!(activity_at(None, Some(1_000), 9_000), None);
        assert_eq!(activity_at(None, None, 9_000), None);
    }

    // ── Tier 0: Claude's per-PID sessions manifest ────────────────────

    #[test]
    fn tier0_sessions_file_wins() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
        std::fs::create_dir_all(home.join(".claude").join("projects").join("-tmp-a")).unwrap();
        std::fs::write(
            home.join(".claude").join("sessions").join("12345.json"),
            r#"{"pid":12345,"sessionId":"14b684ba-618f-48f4-91d7-422ef99c9e38","cwd":"/tmp/x"}"#,
        )
        .unwrap();
        let parent = ParentInfo {
            pid: 12345,
            started_at: None,
            cmdline: Vec::new(),
        };
        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/x",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(
            got.as_ref().map(|s| s.0.as_ref()),
            Some("14b684ba-618f-48f4-91d7-422ef99c9e38")
        );
    }

    #[test]
    fn canonical_id_drift_is_observed_when_manifest_rewritten() {
        // A long-lived shim's parent keeps the SAME pid across a compact/clear,
        // but Claude re-mints the session id and rewrites the per-PID manifest
        // in place. Re-reading the manifest must observe the NEW id — the env
        // var can't (it's frozen at spawn). This is the exact source the
        // reconcile loop polls to self-heal canonical-id drift.
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let manifest = sessions.join("4017.json");

        std::fs::write(
            &manifest,
            r#"{"pid":4017,"sessionId":"d77d5f3a-old","cwd":"/x"}"#,
        )
        .unwrap();
        assert_eq!(
            sid_from_claude_sessions_file(&unprobed, &home, 4017)
                .as_ref()
                .map(|s| s.0.as_ref()),
            Some("d77d5f3a-old")
        );

        // compact re-mints the id; same pid, manifest updated in place.
        std::fs::write(
            &manifest,
            r#"{"pid":4017,"sessionId":"c80daf7d-new","cwd":"/x"}"#,
        )
        .unwrap();
        assert_eq!(
            sid_from_claude_sessions_file(&unprobed, &home, 4017)
                .as_ref()
                .map(|s| s.0.as_ref()),
            Some("c80daf7d-new")
        );
    }

    #[test]
    fn tier0_missing_sessions_file_falls_through_to_tier1() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-fallback");
        std::fs::create_dir_all(&proj).unwrap();
        // Tier 0 absent.
        // Tier 1: cmdline names the id directly.
        let parent = ParentInfo {
            pid: 99999,
            started_at: None,
            cmdline: vec![
                "claude".into(),
                "--session-id".into(),
                "from-cmdline".into(),
            ],
        };
        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/fallback",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("from-cmdline"));
    }

    #[test]
    fn tier0_manifest_written_after_start_is_polled() {
        // Fresh-launch race: Claude writes ~/.claude/sessions/<pid>.json a
        // beat AFTER it spawns this shim. The single up-front read that
        // used to gate tiers 0/1 missed it and fell through to tier-2
        // ctime correlation, which on a no-prompt-yet session has no
        // .jsonl to match → None → shim bails → MCP disconnect. With the
        // authoritative tiers folded into the poll loop, discovery must
        // converge once the manifest lands. No .jsonl is ever created
        // here, so this only passes if tier 0 is genuinely polled.
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let sessions = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(home.join(".claude").join("projects").join("-tmp-race")).unwrap();

        let manifest = sessions.join("4242.json");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            std::fs::write(
                &manifest,
                r#"{"pid":4242,"sessionId":"raced-into-existence","cwd":"/tmp/race"}"#,
            )
            .unwrap();
        });

        let parent = ParentInfo {
            pid: 4242,
            started_at: None,
            cmdline: Vec::new(),
        };
        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/race",
            Duration::from_secs(5),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(
            got.as_ref().map(|s| s.0.as_ref()),
            Some("raced-into-existence")
        );
    }

    // ── Primary: CLAUDE_CODE_SESSION_ID env var ───────────────────────

    #[test]
    fn env_var_value_is_used_verbatim_and_trimmed() {
        assert_eq!(
            sid_from_env(Some("  abc-123  ".to_string()))
                .as_ref()
                .map(|s| s.0.as_ref()),
            Some("abc-123")
        );
        assert_eq!(sid_from_env(None), None);
        assert_eq!(sid_from_env(Some("   ".to_string())), None);
        assert_eq!(sid_from_env(Some(String::new())), None);
    }

    // ── Tier 1: cmdline parse ────────────────────────────────────────

    #[test]
    fn cmdline_explicit_session_id_flag_wins() {
        let args = vec![
            "claude".to_string(),
            "--session-id".to_string(),
            "8c1175c7-40ea-44c7-8c1d-784f6e3d3dc1".to_string(),
            "--dangerously-load-development-channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("8c1175c7-40ea-44c7-8c1d-784f6e3d3dc1")
        );
    }

    #[test]
    fn cmdline_equals_form_session_id() {
        let args = vec!["claude".to_string(), "--session-id=abc".to_string()];
        assert_eq!(sid_from_cmdline(&args).as_deref(), Some("abc"));
    }

    #[test]
    fn cmdline_resume_with_path_extracts_stem() {
        let args = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "/root/.claude/projects/-root-pulse-deploy/14b684ba-618f-48f4-91d7-422ef99c9e38.jsonl"
                .to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("14b684ba-618f-48f4-91d7-422ef99c9e38")
        );
    }

    #[test]
    fn cmdline_resume_with_bare_uuid_passthrough() {
        let args = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "14b684ba-618f-48f4-91d7-422ef99c9e38".to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("14b684ba-618f-48f4-91d7-422ef99c9e38")
        );
    }

    #[test]
    fn cmdline_session_id_takes_priority_over_resume() {
        // If both flags appear, the first one parsed wins. We iterate
        // left-to-right; document that semantics by testing the order
        // observed in real Claude args.
        let args = vec![
            "claude".to_string(),
            "--session-id".to_string(),
            "explicit-id".to_string(),
            "--resume".to_string(),
            "/path/some-other-id.jsonl".to_string(),
        ];
        assert_eq!(sid_from_cmdline(&args).as_deref(), Some("explicit-id"));
    }

    #[test]
    fn cmdline_no_relevant_flags_returns_none() {
        let args = vec![
            "claude".to_string(),
            "--dangerously-load-development-channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert_eq!(sid_from_cmdline(&args), None);
    }

    // ── Tier 2: ctime correlation ────────────────────────────────────

    #[test]
    fn parent_correlation_picks_jsonl_with_closest_creation_time() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-multi-cwd");
        std::fs::create_dir_all(&proj).unwrap();

        // Three sibling sessions in the same cwd. Each .jsonl has a
        // slightly different creation time. The shim's parent was
        // spawned at the second one's time; that one must win.
        write_transcript(&proj.join("sess-old.jsonl"), "/tmp/shared-cwd");
        std::thread::sleep(Duration::from_millis(200));
        write_transcript(&proj.join("sess-target.jsonl"), "/tmp/shared-cwd");
        std::thread::sleep(Duration::from_millis(200));
        write_transcript(&proj.join("sess-newer.jsonl"), "/tmp/shared-cwd");

        // Parent's started_at = the middle file's actual created() time.
        let target_meta = std::fs::metadata(proj.join("sess-target.jsonl")).unwrap();
        let parent_t = target_meta
            .created()
            .or_else(|_| target_meta.modified())
            .unwrap();

        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/shared-cwd",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("sess-target"));
    }

    #[test]
    fn parent_correlation_outside_window_excludes_match() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home
            .join(".claude")
            .join("projects")
            .join("-tmp-far-from-now");
        std::fs::create_dir_all(&proj).unwrap();
        write_transcript(&proj.join("only.jsonl"), "/tmp/wanted-cwd");

        // Parent claims to have started a year before this file existed.
        let parent_t = std::time::SystemTime::UNIX_EPOCH;
        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/wanted-cwd",
            Duration::from_millis(50),
            move || Some(clone_parent(&parent)),
        );
        assert!(got.is_none());
    }

    #[test]
    fn cmdline_short_circuits_correlation() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-cmd");
        std::fs::create_dir_all(&proj).unwrap();
        write_transcript(&proj.join("forced-sid.jsonl"), "/tmp/cmdline-cwd");

        let parent = ParentInfo {
            pid: 99999,
            // started_at is None — would force tier 2 to bail. tier 1
            // must still resolve.
            started_at: None,
            cmdline: vec![
                "claude".to_string(),
                "--session-id".to_string(),
                "forced-sid".to_string(),
            ],
        };

        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/cmdline-cwd",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("forced-sid"));
    }

    #[test]
    fn missing_projects_dir_returns_none_fast() {
        let tmp = make_tempdir();
        let home = tmp.join("nonexistent");
        let started = Instant::now();
        let got = resolve_under(
            &unprobed,
            &home,
            "/tmp/whatever",
            Duration::from_secs(2),
            || {
                Some(ParentInfo {
                    pid: 1,
                    started_at: Some(SystemTime::UNIX_EPOCH),
                    cmdline: Vec::new(),
                })
            },
        );
        assert!(got.is_none());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn windows_style_cwd_with_backslashes_matches_via_correlation() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("C--Users-admin");
        std::fs::create_dir_all(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("win-sid.jsonl")).unwrap();
        writeln!(
            f,
            r#"{{"type":"system","cwd":"C:\\Users\\admin","timestamp":1}}"#
        )
        .unwrap();
        drop(f);

        let target_meta = std::fs::metadata(proj.join("win-sid.jsonl")).unwrap();
        let parent_t = target_meta
            .created()
            .or_else(|_| target_meta.modified())
            .unwrap();
        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &unprobed,
            &home,
            r"C:\Users\admin",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("win-sid"));
    }

    // ── helpers ────────────────────────────────────────────────────────

    fn write_transcript(path: &Path, cwd: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, r#"{{"type":"last-prompt","leafUuid":"x"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"permission-mode","permissionMode":"default"}}"#
        )
        .unwrap();
        let escaped = cwd.replace('\\', r"\\").replace('"', r#"\""#);
        writeln!(f, r#"{{"type":"system","cwd":"{escaped}","timestamp":1}}"#).unwrap();
    }

    fn make_tempdir() -> PathBuf {
        // Cheap uniqueness without bringing in tempfile.
        static COUNTER: Mutex<u64> = Mutex::new(0);
        let n = {
            let mut g = COUNTER.lock().unwrap();
            *g += 1;
            *g
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "marshal-shim-discover-{}-{n}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn clone_parent(p: &ParentInfo) -> ParentInfo {
        ParentInfo {
            pid: p.pid,
            started_at: p.started_at,
            cmdline: p.cmdline.clone(),
        }
    }
}
