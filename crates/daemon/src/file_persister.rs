//! Keeping the store in a file when there's no Postgres (MAR-53, ported from muster's MUS-17).
//!
//! Opt-in with `MARSHAL_PERSIST=file` (Josh, 2026-11-05). Events go one JSON line
//! each to `<state dir>/store.jsonl`. At boot the file is folded to the last SET
//! per entity, those are loaded straight into myko's stores (as myko's own
//! Postgres consumer does: no cascades, nothing written back), and the fold is
//! written out as the new file. While running, the file is compacted the same way
//! once it has grown past four times its last compacted size and 1 MB, and once a
//! day, so mail the 14-day sweep deleted doesn't sit on disk for long.
//!
//! Compaction never stops appends for longer than the tail copy (muster's design review
//! rule, muster's docs/local-persistence-design.md): it notes the file's length `L` under
//! the append lock, folds `[0, L)` without the lock, then under the lock copies
//! `[L, end)` of the live file unchanged onto the folded one and renames it over
//! the live file. Every event is either folded or copied in order.
//!
//! Which entities reach this persister is decided where it's installed: it takes
//! the default persister's slot, so the blackholed connection bookkeeping and
//! the Session liveness filter stay as they are with Postgres. It writes only the
//! durable kinds listed in [`DURABLE_KINDS`], and refuses a file naming any other.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use myko::server::{PersistError, PersistHealth, Persister};
use myko::wire::{MEvent, MEventType};

/// The store file in the state directory.
pub const STORE_FILE: &str = "store.jsonl";
/// The setting that turns the file on.
pub const ENV: &str = "MARSHAL_PERSIST";

/// The entities Marshal keeps across a restart, as with Postgres. Anything else
/// that reaches the persister is not written, and a file naming anything else
/// stops startup.
pub const DURABLE_KINDS: &[&str] = &[
    "Message",
    "MessageRead",
    "Room",
    "RoomMember",
    "SandboxRun",
    "Session",
    "SessionIdentity",
    "SessionNickname",
    "Succession",
];

/// Compact once the file is this many times its last compacted size...
const GROWTH_FACTOR: u64 = 4;
/// ...and at least this big, so a small store isn't rewritten all the time.
const MIN_COMPACT_BYTES: u64 = 1024 * 1024;
/// Compact at least this often, so deleted mail leaves the disk within a day.
const COMPACT_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Writes other than a new message are synced to disk at most this late.
const SYNC_EVERY: Duration = Duration::from_millis(100);
/// Past this size after compaction, warn every hour rather than lose mail.
const WARN_BYTES: u64 = 64 * 1024 * 1024;

/// The daemon's state directory, where the store file goes:
/// `MARSHAL_STATE_DIR`, else `$XDG_STATE_HOME/marshal`, else
/// `~/.local/state/marshal`. Read only with `MARSHAL_PERSIST=file`.
pub fn state_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MARSHAL_STATE_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("marshal"));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set; set MARSHAL_STATE_DIR"))?;
    Ok(PathBuf::from(home).join(".local/state/marshal"))
}

/// How the daemon keeps its store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistMode {
    Postgres,
    File,
    Ephemeral,
}

/// The mode, from whether Postgres is configured and `MARSHAL_PERSIST`. A value
/// other than `file` stops startup, so a typo can't silently lose mail. The
/// string is a note for the startup line, when there is one.
pub fn resolve_mode(
    postgres: bool,
    setting: Option<&str>,
) -> Result<(PersistMode, Option<String>), String> {
    let wants_file = match setting.map(str::trim) {
        None | Some("") => false,
        Some("file") => true,
        Some(other) => {
            return Err(format!(
                "{ENV}={other:?} isn't a persistence mode: use \"file\", or leave it unset"
            ));
        }
    };
    Ok(match (postgres, wants_file) {
        (true, true) => (
            PersistMode::Postgres,
            Some(format!("{ENV}=file ignored: Postgres is configured")),
        ),
        (true, false) => (PersistMode::Postgres, None),
        (false, true) => (PersistMode::File, None),
        (false, false) => (PersistMode::Ephemeral, None),
    })
}

/// What a fold of the file found.
#[derive(Debug, Default)]
pub struct Folded {
    /// The last SET of every entity not since deleted, in the order of those SETs.
    pub events: Vec<MEvent>,
    /// How many of `events` there are of each kind.
    pub counts: BTreeMap<String, usize>,
    /// A final line cut off by a crash, skipped.
    pub torn_tail: bool,
}

/// Fold JSONL events to the latest state. A line that doesn't parse stops the
/// fold with its number, except a final line with no newline, which a crash
/// can leave half-written and is skipped. So is a kind outside [`DURABLE_KINDS`].
pub fn fold(reader: impl Read) -> io::Result<Folded> {
    let mut latest: HashMap<(String, String), (u64, MEvent)> = HashMap::new();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut number = 0u64;
    let mut torn_tail = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        number += 1;
        let complete = line.ends_with('\n');
        let text = line.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        let event: MEvent = match serde_json::from_str(text) {
            Ok(event) => event,
            Err(_) if !complete => {
                torn_tail = true;
                break;
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {number} isn't an event: {e}"),
                ));
            }
        };
        if !DURABLE_KINDS.contains(&&*event.item_type) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "line {number} names {:?}, which Marshal doesn't keep",
                    event.item_type
                ),
            ));
        }
        let Some(id) = event
            .item
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {number} has no id"),
            ));
        };
        let key = (event.item_type.to_string(), id);
        match event.change_type {
            MEventType::SET => {
                latest.insert(key, (number, event));
            }
            MEventType::DEL => {
                latest.remove(&key);
            }
        }
    }
    let mut kept: Vec<(u64, MEvent)> = latest.into_values().collect();
    kept.sort_by_key(|(n, _)| *n);
    let mut counts = BTreeMap::new();
    for (_, event) in &kept {
        *counts.entry(event.item_type.to_string()).or_insert(0) += 1;
    }
    Ok(Folded {
        events: kept.into_iter().map(|(_, e)| e).collect(),
        counts,
        torn_tail,
    })
}

fn event_line(event: &MEvent) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(event).map_err(io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

/// Whether the store file was excluded from Time Machine, for the startup line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backup {
    /// Not macOS.
    NotApplicable,
    Excluded,
    Failed(String),
}

impl std::fmt::Display for Backup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplicable => Ok(()),
            Self::Excluded => write!(f, ", excluded from Time Machine"),
            Self::Failed(why) => write!(f, ", NOT excluded from Time Machine ({why})"),
        }
    }
}

/// Exclude a freshly written store file from Time Machine (Josh, 2026-11-05).
/// The sticky form (no `-p`) marks the file itself, so each compacted file is
/// marked before it's renamed into place.
fn exclude_from_backup(path: &Path) -> Backup {
    if !cfg!(target_os = "macos") {
        return Backup::NotApplicable;
    }
    match std::process::Command::new("tmutil")
        .arg("addexclusion")
        .arg(path)
        .output()
    {
        Ok(out) if out.status.success() => Backup::Excluded,
        Ok(out) => Backup::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Backup::Failed(e.to_string()),
    }
}

/// Write events to `path` as a private file, synced, and exclude it from backup.
fn write_snapshot(path: &Path, events: &[MEvent]) -> io::Result<(File, u64, Backup)> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut len = 0u64;
    {
        let mut out = io::BufWriter::new(&mut file);
        for event in events {
            let line = event_line(event)?;
            out.write_all(&line)?;
            len += line.len() as u64;
        }
        out.flush()?;
    }
    file.sync_all()?;
    let backup = exclude_from_backup(path);
    Ok((file, len, backup))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// The store file must be ours and private, like the token files.
fn check_private(path: &Path, owner_probe: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(());
        };
        let me = owner_probe.metadata()?.uid();
        if meta.uid() != me {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} belongs to another user (uid {}); Marshal runs as uid {me}",
                    path.display(),
                    meta.uid()
                ),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} can be read by other users (mode {:o}); run `chmod 600 {}`",
                    path.display(),
                    meta.mode() & 0o777,
                    path.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, owner_probe);
    Ok(())
}

struct Appender {
    file: File,
    /// The file's length after the last whole line: every byte before it is
    /// complete lines, and nothing after it is kept (see `append`).
    len: u64,
    /// Set when a failed write couldn't be cut back off the file. Appends are
    /// then refused with this reason, rather than landing after a broken line.
    broken: Option<String>,
    /// Tests: the next write puts only this many bytes in, then fails.
    #[cfg(test)]
    fail_after: Option<usize>,
    /// Tests: the next reopen after a compaction fails.
    #[cfg(test)]
    fail_reopen: bool,
}

struct Shared {
    dir: PathBuf,
    path: PathBuf,
    appender: Mutex<Appender>,
    dirty: AtomicBool,
    compacting: AtomicBool,
    compacted_len: AtomicU64,
    last_compaction: Mutex<Instant>,
    last_warned: Mutex<Option<Instant>>,
    health: Arc<PersistHealth>,
    unkept: Mutex<std::collections::HashSet<String>>,
    /// Held for the persister's life: a second daemon on this directory can't take it.
    _lock: File,
}

/// What opening the store loaded, for replay and the startup line.
pub struct Opened {
    pub persister: Arc<FilePersister>,
    pub folded: Folded,
    pub path: PathBuf,
    pub backup: Backup,
}

/// The file-backed persister. Cheap to clone; all clones share one file.
#[derive(Clone)]
pub struct FilePersister {
    shared: Arc<Shared>,
}

impl FilePersister {
    /// Open the store in `dir`: take its lock, check the file is private, fold
    /// it, and write the fold as the new file. The folded events are returned
    /// for the caller to load into myko's stores.
    pub fn open(dir: &Path) -> io::Result<Opened> {
        create_private_dir(dir)?;
        let lock_path = dir.join(format!("{STORE_FILE}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        if let Err(e) = lock.try_lock() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another marshal-daemon is using {} ({e}); stop it, or give this one its own MARSHAL_STATE_DIR",
                    dir.display()
                ),
            ));
        }
        let path = dir.join(STORE_FILE);
        let tmp = dir.join(format!("{STORE_FILE}.tmp"));
        check_private(&path, &lock)?;
        // A compaction a crash cut short leaves the old file whole and this behind.
        let _ = std::fs::remove_file(&tmp);

        let folded = match File::open(&path) {
            Ok(file) => fold(file)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Folded::default(),
            Err(e) => return Err(e),
        };
        let (_, len, backup) = write_snapshot(&tmp, &folded.events)?;
        std::fs::rename(&tmp, &path)?;
        sync_dir(dir)?;

        let shared = Arc::new(Shared {
            dir: dir.to_path_buf(),
            path: path.clone(),
            appender: Mutex::new(Appender {
                file: open_append(&path)?,
                len,
                broken: None,
                #[cfg(test)]
                fail_after: None,
                #[cfg(test)]
                fail_reopen: false,
            }),
            dirty: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
            compacted_len: AtomicU64::new(len),
            last_compaction: Mutex::new(Instant::now()),
            last_warned: Mutex::new(None),
            health: Arc::new(PersistHealth::default()),
            unkept: Mutex::new(Default::default()),
            _lock: lock,
        });
        start_background(Arc::downgrade(&shared));
        Ok(Opened {
            persister: Arc::new(Self { shared }),
            folded,
            path,
            backup,
        })
    }

    /// The file's current length.
    pub fn len(&self) -> u64 {
        self.shared
            .appender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Compact now, as the background task does.
    pub fn compact(&self) -> io::Result<()> {
        self.compact_with(|| {})
    }

    /// Compact, calling `between` after the fold and before the tail copy: the
    /// window in which appends go on into the live file. For tests.
    pub fn compact_with(&self, between: impl FnOnce()) -> io::Result<()> {
        if self.shared.compacting.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = compact(&self.shared, between);
        self.shared.compacting.store(false, Ordering::SeqCst);
        result
    }

    fn append(&self, event: &MEvent) -> io::Result<()> {
        let line = event_line(event)?;
        let len = {
            let mut appender = self
                .shared
                .appender
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(why) = &appender.broken {
                return Err(io::Error::other(format!(
                    "the store file is broken and takes no more writes until the daemon restarts: {why}"
                )));
            }
            // A new message is the thing this file exists to keep: on disk before
            // the send answers. Everything else is synced within SYNC_EVERY.
            let sync_now = &*event.item_type == "Message";
            if let Err(e) = write_line(&mut appender, &line, sync_now) {
                // Part of the line may be in the file (ENOSPC, EIO). Cut it off, so the
                // next line starts on a boundary and the file still folds at boot.
                let good = appender.len;
                if let Err(cut) = appender.file.set_len(good) {
                    let why = format!(
                        "a write failed ({e}) and its partial line couldn't be cut off ({cut})"
                    );
                    log::error!("[store] {}: {why}", self.shared.path.display());
                    appender.broken = Some(why);
                }
                return Err(e);
            }
            appender.len += line.len() as u64;
            if !sync_now {
                self.shared.dirty.store(true, Ordering::Release);
            }
            appender.len
        };
        let compacted = self.shared.compacted_len.load(Ordering::Relaxed);
        if len > MIN_COMPACT_BYTES
            && len > compacted.saturating_mul(GROWTH_FACTOR)
            && !self.shared.compacting.load(Ordering::Relaxed)
        {
            let me = self.clone();
            std::thread::spawn(move || {
                if let Err(e) = me.compact() {
                    log::warn!("[store] compaction failed, the file is unchanged: {e}");
                }
            });
        }
        Ok(())
    }
}

/// Open the freshly compacted file for appends.
fn reopen(appender: &mut Appender, path: &Path) -> io::Result<File> {
    #[cfg(test)]
    if std::mem::take(&mut appender.fail_reopen) {
        return Err(io::Error::other("injected reopen failure"));
    }
    #[cfg(not(test))]
    let _ = appender;
    open_append(path)
}

/// Write one whole line, and sync it if asked. Fails rather than leave it half
/// written, though the caller still has to cut off what a failure left behind.
fn write_line(appender: &mut Appender, line: &[u8], sync_now: bool) -> io::Result<()> {
    #[cfg(test)]
    if let Some(n) = appender.fail_after.take() {
        appender.file.write_all(&line[..n.min(line.len())])?;
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected short write",
        ));
    }
    appender.file.write_all(line)?;
    if sync_now {
        appender.file.sync_data()?;
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                log::warn!(
                    "[store] {} can be listed by other users (mode {mode:o}); the store file \
                     itself is private, but `chmod 700 {}` keeps its name and size private too",
                    dir.display(),
                    dir.display()
                );
            }
        }
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn compact(shared: &Shared, between: impl FnOnce()) -> io::Result<()> {
    // 1. Where the fold stops: a line boundary, since appends are whole lines under this lock.
    let cut = shared
        .appender
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len;
    // 2. Fold [0, cut) while appends carry on into the live file.
    let folded = fold(File::open(&shared.path)?.take(cut))?;
    let tmp = shared.dir.join(format!("{STORE_FILE}.tmp"));
    let (mut out, folded_len, backup) = write_snapshot(&tmp, &folded.events)?;
    if let Backup::Failed(why) = &backup {
        log::warn!("[store] the compacted file isn't excluded from Time Machine: {why}");
    }
    between();
    // 3 and 4, under the lock: copy everything from exactly `cut` on, then swap.
    let mut appender = shared.appender.lock().unwrap_or_else(|p| p.into_inner());
    let mut live = File::open(&shared.path)?;
    live.seek(SeekFrom::Start(cut))?;
    out.seek(SeekFrom::End(0))?;
    let tail = io::copy(&mut live, &mut out)?;
    out.sync_all()?;
    std::fs::rename(&tmp, &shared.path)?;
    // From here the appender's file is the old, now unlinked one: it must be
    // repointed, or appends must be refused. Never left writing into a deleted file.
    let reopened = sync_dir(&shared.dir).and_then(|()| reopen(&mut appender, &shared.path));
    match reopened {
        Ok(file) => appender.file = file,
        Err(e) => {
            let why = format!(
                "after compacting, {} couldn't be reopened ({e})",
                shared.path.display()
            );
            log::error!("[store] {why}");
            appender.broken = Some(why);
            return Err(e);
        }
    }
    appender.len = folded_len + tail;
    drop(appender);
    shared.compacted_len.store(folded_len, Ordering::Relaxed);
    *shared
        .last_compaction
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Instant::now();
    log::info!(
        "[store] compacted {}: {} entities, {} bytes ({} appended during it)",
        shared.path.display(),
        folded.events.len(),
        folded_len + tail,
        tail
    );
    if folded_len > WARN_BYTES {
        let mut last = shared.last_warned.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
            *last = Some(Instant::now());
            log::warn!(
                "[store] {} is {} MB after compaction: {:?}",
                shared.path.display(),
                folded_len / (1024 * 1024),
                folded.counts
            );
        }
    }
    Ok(())
}

/// Sync dirty writes every SYNC_EVERY, and compact once a day. Ends when the
/// persister is dropped.
fn start_background(shared: Weak<Shared>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(SYNC_EVERY);
            let Some(shared) = shared.upgrade() else {
                return;
            };
            if shared.dirty.swap(false, Ordering::AcqRel) {
                let appender = shared.appender.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(e) = appender.file.sync_data() {
                    log::warn!("[store] sync failed: {e}");
                }
            }
            let due = shared
                .last_compaction
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .elapsed()
                >= COMPACT_EVERY;
            if due {
                let me = FilePersister { shared };
                if let Err(e) = me.compact() {
                    log::warn!("[store] daily compaction failed, the file is unchanged: {e}");
                }
            }
        }
    });
}

impl Persister for FilePersister {
    fn persist(&self, event: MEvent) -> Result<(), PersistError> {
        if !DURABLE_KINDS.contains(&&*event.item_type) {
            let mut unkept = self.shared.unkept.lock().unwrap_or_else(|p| p.into_inner());
            if unkept.insert(event.item_type.to_string()) {
                log::info!("[store] {} isn't kept across restarts", event.item_type);
            }
            return Ok(());
        }
        let health = &self.shared.health;
        match self.append(&event) {
            Ok(()) => {
                health.total_persisted.fetch_add(1, Ordering::Relaxed);
                health.consecutive_errors.store(0, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                health.total_errors.fetch_add(1, Ordering::Relaxed);
                health.consecutive_errors.fetch_add(1, Ordering::Relaxed);
                let message = format!("writing {}: {e}", self.shared.path.display());
                if let Ok(mut last) = health.last_error.write() {
                    *last = Some(message.clone());
                }
                Err(PersistError {
                    entity_type: event.item_type.clone(),
                    message,
                })
            }
        }
    }

    fn health(&self) -> Arc<PersistHealth> {
        self.shared.health.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(kind: &str, id: &str, body: &str) -> MEvent {
        let mut event =
            MEvent::set_from_value(kind, json!({ "id": id, "body": body }), "test-host");
        event.created_at = Arc::from("2026-11-05T00:00:00Z");
        event
    }

    fn del(kind: &str, id: &str) -> MEvent {
        let mut event = set(kind, id, "");
        event.change_type = MEventType::DEL;
        event
    }

    fn lines(events: &[MEvent]) -> Vec<u8> {
        events.iter().flat_map(|e| event_line(e).unwrap()).collect()
    }

    fn state(folded: &Folded) -> Vec<(String, String, String)> {
        folded
            .events
            .iter()
            .map(|e| {
                (
                    e.item_type.to_string(),
                    e.item["id"].as_str().unwrap().to_string(),
                    e.item["body"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn file_state(path: &Path) -> Vec<(String, String, String)> {
        state(&fold(File::open(path).unwrap()).unwrap())
    }

    #[test]
    fn the_mode_is_opt_in_and_postgres_wins() {
        assert_eq!(resolve_mode(false, None).unwrap().0, PersistMode::Ephemeral);
        assert_eq!(
            resolve_mode(false, Some("")).unwrap().0,
            PersistMode::Ephemeral
        );
        assert_eq!(
            resolve_mode(false, Some("file")).unwrap(),
            (PersistMode::File, None)
        );
        assert_eq!(
            resolve_mode(true, None).unwrap(),
            (PersistMode::Postgres, None)
        );
        let (mode, note) = resolve_mode(true, Some("file")).unwrap();
        assert_eq!(mode, PersistMode::Postgres);
        assert!(note.unwrap().contains("ignored"));
        assert!(
            resolve_mode(false, Some("flie"))
                .unwrap_err()
                .contains("isn't a persistence mode")
        );
        assert!(resolve_mode(false, Some("off")).is_err());
    }

    #[test]
    fn the_last_set_wins_and_a_del_removes() {
        let folded = fold(
            &lines(&[
                set("Message", "m1", "first"),
                set("Room", "r1", "room"),
                set("Message", "m1", "edited"),
                set("Message", "m2", "gone soon"),
                del("Message", "m2"),
                set("MessageRead", "m1::s1", ""),
            ])[..],
        )
        .unwrap();
        assert_eq!(
            state(&folded),
            [
                ("Room".into(), "r1".into(), "room".into()),
                ("Message".into(), "m1".into(), "edited".into()),
                ("MessageRead".into(), "m1::s1".into(), "".into()),
            ]
        );
        assert_eq!(folded.counts["Message"], 1);
        assert!(!folded.torn_tail);
    }

    #[test]
    fn a_torn_last_line_is_skipped_but_a_bad_middle_line_stops_it() {
        let mut bytes = lines(&[set("Message", "m1", "kept")]);
        bytes.extend_from_slice(br#"{"item":{"id":"m2","bo"#);
        let folded = fold(&bytes[..]).unwrap();
        assert!(folded.torn_tail);
        assert_eq!(state(&folded).len(), 1);

        let mut bytes = lines(&[set("Message", "m1", "kept")]);
        bytes.extend_from_slice(b"not json\n");
        bytes.extend(lines(&[set("Message", "m2", "after")]));
        let err = fold(&bytes[..]).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    /// Open the store again after dropping the last persister. The background
    /// thread may still hold an upgraded `Arc` for one sync or compaction tick,
    /// and the lock goes only when it lets go, so a reopen can briefly see
    /// `WouldBlock`. That's the test racing its own previous persister, not a
    /// second daemon, so wait it out (seen on macOS, where `sync_data` is slow).
    fn reopen(dir: &Path) -> Opened {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match FilePersister::open(dir) {
                Ok(opened) => return opened,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("reopen: {e}"),
            }
        }
    }

    #[test]
    fn a_kind_marshal_doesnt_keep_is_refused() {
        let err = fold(&lines(&[set("Client", "c1", "")])[..]).unwrap_err();
        assert!(err.to_string().contains("doesn't keep"), "{err}");
    }

    #[test]
    fn a_restart_keeps_unread_mail_and_its_read_state() {
        let dir = tempfile::tempdir().unwrap();
        {
            let opened = FilePersister::open(dir.path()).unwrap();
            assert!(opened.folded.events.is_empty());
            let p = opened.persister;
            p.persist(set("Message", "m1", "unread")).unwrap();
            p.persist(set("Message", "m2", "read")).unwrap();
            p.persist(set("MessageRead", "m2::s1", "")).unwrap();
            p.persist(set("Client", "c1", "")).unwrap(); // not kept
        }
        let opened = reopen(dir.path());
        assert_eq!(
            state(&opened.folded),
            [
                ("Message".into(), "m1".into(), "unread".into()),
                ("Message".into(), "m2".into(), "read".into()),
                ("MessageRead".into(), "m2::s1".into(), "".into()),
            ]
        );
    }

    #[test]
    fn a_second_daemon_on_the_same_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _first = FilePersister::open(dir.path()).unwrap();
        let Err(err) = FilePersister::open(dir.path()) else {
            panic!("a second open succeeded");
        };
        assert!(err.to_string().contains("another marshal-daemon"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_a_readable_one_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let opened = FilePersister::open(dir.path()).unwrap();
            assert_eq!(
                std::fs::metadata(&opened.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            opened.path
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let Err(err) = FilePersister::open(dir.path()) else {
            panic!("opened a world-readable store");
        };
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }

    #[test]
    fn a_stray_tmp_from_a_cut_short_compaction_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        drop(FilePersister::open(dir.path()).unwrap());
        std::fs::write(
            dir.path().join(format!("{STORE_FILE}.tmp")),
            b"half a compaction",
        )
        .unwrap();
        let opened = reopen(dir.path());
        assert!(opened.folded.events.is_empty());
        assert!(!dir.path().join(format!("{STORE_FILE}.tmp")).exists());
    }

    /// frank-crocodile's send-back: a write that fails partway (disk full) must not
    /// leave a broken line for the next one to land after.
    #[test]
    fn a_failed_write_leaves_no_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let opened = FilePersister::open(dir.path()).unwrap();
        let p = opened.persister;
        p.persist(set("Message", "m1", "before")).unwrap();
        let good_len = p.len();

        p.shared.appender.lock().unwrap().fail_after = Some(17);
        assert!(p.persist(set("Message", "m2", "half written")).is_err());
        assert_eq!(
            std::fs::metadata(&opened.path).unwrap().len(),
            good_len,
            "the partial line was cut off"
        );
        assert_eq!(p.len(), good_len);
        assert_eq!(p.shared.health.total_errors.load(Ordering::Relaxed), 1);

        p.persist(set("Message", "m3", "after")).unwrap();
        // A compaction's cut still lands on a line boundary...
        p.compact().unwrap();
        drop(p);
        // ...and the file folds cleanly at the next boot.
        let reopened = reopen(dir.path());
        assert!(!reopened.folded.torn_tail);
        assert_eq!(
            state(&reopened.folded),
            [
                ("Message".into(), "m1".into(), "before".into()),
                ("Message".into(), "m3".into(), "after".into()),
            ]
        );
    }

    /// fiery-snapper's should-fix: if the compacted file can't be reopened after the
    /// rename, appends must be refused, not written into the old, deleted file.
    #[test]
    fn a_failed_reopen_after_compaction_refuses_writes_instead_of_losing_them() {
        let dir = tempfile::tempdir().unwrap();
        let p = FilePersister::open(dir.path()).unwrap().persister;
        p.persist(set("Message", "m1", "before")).unwrap();
        p.shared.appender.lock().unwrap().fail_reopen = true;
        let err = p.compact().unwrap_err();
        assert!(err.to_string().contains("injected reopen failure"), "{err}");
        let refused = p.persist(set("Message", "m2", "after")).unwrap_err();
        assert!(
            refused.message.contains("couldn't be reopened"),
            "{}",
            refused.message
        );
        assert!(
            p.shared
                .health
                .last_error
                .read()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("broken")
        );
        drop(p);
        // The rename did happen, so the file on disk is the compacted one, whole.
        let reopened = reopen(dir.path());
        assert_eq!(
            state(&reopened.folded),
            [("Message".into(), "m1".into(), "before".into())]
        );
    }

    #[test]
    fn a_store_that_cant_be_repaired_refuses_writes_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let p = FilePersister::open(dir.path()).unwrap().persister;
        p.shared.appender.lock().unwrap().broken =
            Some("a write failed (disk full) and ...".into());
        let err = p.persist(set("Message", "m1", "refused")).unwrap_err();
        assert!(err.message.contains("broken"), "{}", err.message);
        assert!(
            p.shared
                .health
                .last_error
                .read()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("broken")
        );
    }

    /// fiery-snapper's test: writes while a compaction is between its fold and its
    /// tail copy are all kept, including a DEL of something already folded.
    #[test]
    fn appends_during_a_compaction_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opened = FilePersister::open(dir.path()).unwrap();
        let p = opened.persister;
        let path = opened.path;
        let before = [
            set("Message", "m1", "one"),
            set("Message", "m2", "two"),
            set("Message", "m1", "one, edited"),
            set("Room", "r1", "room"),
        ];
        let during = [
            set("Message", "m3", "sent mid-compaction"),
            del("Message", "m2"),
            set("Room", "r1", "room, renamed"),
        ];
        for e in &before {
            p.persist(e.clone()).unwrap();
        }
        let uncompacted = state(&fold(&lines(&[&before[..], &during[..]].concat())[..]).unwrap());

        let writer = p.clone();
        let during_copy = during.clone();
        p.compact_with(move || {
            for e in during_copy {
                writer.persist(e).unwrap();
            }
        })
        .unwrap();

        assert_eq!(file_state(&path), uncompacted);
        // And later appends land in the new file.
        p.persist(set("Message", "m4", "after")).unwrap();
        let mut expected = uncompacted;
        expected.push(("Message".into(), "m4".into(), "after".into()));
        assert_eq!(file_state(&path), expected);
        drop(p);
        assert_eq!(state(&reopen(dir.path()).folded), expected);
    }

    #[test]
    fn compaction_shrinks_the_file_to_the_current_state() {
        let dir = tempfile::tempdir().unwrap();
        let opened = FilePersister::open(dir.path()).unwrap();
        let p = opened.persister;
        for i in 0..200 {
            p.persist(set("Session", "s1", &format!("task {i}")))
                .unwrap();
        }
        let grown = p.len();
        p.compact().unwrap();
        assert!(p.len() < grown / 100, "{} -> {}", grown, p.len());
        assert_eq!(
            file_state(&opened.path),
            [("Session".into(), "s1".into(), "task 199".into())]
        );
    }
}
