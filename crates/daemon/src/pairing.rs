//! Per-machine pairing (MAR-24, from muster's gate). Fork only.
//!
//! With `--tls`, myko's listener gets myko#129's auth provider with this
//! module's [`PairingAuthenticator`], so every WebSocket and MCP connection
//! needs a bearer token the daemon knows. Tokens come from pairing:
//!
//! 1. On the daemon's host, `marshal-daemon pair-code <name>` writes a
//!    single-use invite, good for a while, and prints an `Invite` code.
//! 2. On the machine, `marshal-shim pair <code>` posts the invite's secret to
//!    the daemon's `/pair` (on the hook listener, over TLS pinned by the
//!    fingerprint in the code) and gets a token.
//! 3. `marshal-daemon unpair <name>` forgets the token. The machine's open
//!    connections close within a second ([`myko_auth::Authenticator::validate_identity`]
//!    returns `Revoked`), and its next connection gets 401.
//!
//! The daemon host's own shims and tools present the local token instead, a
//! secret the daemon writes owner-only to the config directory on first start
//! (MAR-80 and MAR-81 build on it). They never pair.
//!
//! The store is `pairing/` under the state directory: `tokens` holds one
//! `name sha256(token)` line per machine, `invites` one
//! `name sha256(secret) expires-unix-seconds` line per outstanding invite. Only
//! hashes, so neither file can be used to connect. Writes go through a
//! temporary file and a rename, so a reader never sees half a file. The
//! daemon and `pair-code` can both write `invites`; the loser's invite is
//! lost, which fails safe (the code doesn't work and the operator makes
//! another).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use marshal_entities::pairing::{hashes_match, token_hash, valid_name};
use myko_auth::{
    AuthError, AuthenticatedIdentity, Authenticator, AuthorizationRequest, Authorizer, Credentials,
    Principal, PrincipalKind,
};

pub const PAIRING_DIR: &str = "pairing";
const TOKENS_FILE: &str = "tokens";
const INVITES_FILE: &str = "invites";
/// The local token's file, in the config directory, where the shim looks too.
pub const LOCAL_TOKEN_FILE: &str = "local-token";
/// The name the local token's connections carry. `valid_name` can't produce
/// it, so no paired machine can share it.
pub const LOCAL_NAME: &str = "(local)";
/// The issuer in a paired identity's principal.
pub const ISSUER: &str = "marshal-pairing";
/// How long an invite is good for, unless `pair-code --ttl` says otherwise.
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a verified token's cache is trusted before the file is read again.
const CACHE_FOR: Duration = Duration::from_secs(1);

/// One read of `tokens`: when, and the `(name, hash)` lines.
type TokensRead = (std::time::Instant, Vec<(String, String)>);

/// The pairing store under `state_dir`.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    local_token_hash: Option<String>,
    /// The last read of `tokens`, with when.
    cache: Arc<Mutex<Option<TokensRead>>>,
}

static STORE: OnceLock<Store> = OnceLock::new();

/// The store the running daemon uses, once `install` has set it. The hook
/// listener's `/pair` route reads it.
pub fn store() -> Option<&'static Store> {
    STORE.get()
}

/// Make `store` the daemon's. Once.
pub fn install(store: Store) {
    let _ = STORE.set(store);
}

impl Store {
    /// The store under `state_dir`, with no local token.
    pub fn open(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join(PAIRING_DIR),
            local_token_hash: None,
            cache: Arc::default(),
        }
    }

    /// Also accept `token` as the daemon host's own.
    #[must_use]
    pub fn with_local_token(mut self, token: &str) -> Self {
        self.local_token_hash = Some(token_hash(token));
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether any machine is paired, counting lines that don't parse: the
    /// start-up check that refuses to run open should err towards "paired".
    pub fn any_pairings(&self) -> io::Result<bool> {
        match std::fs::read_to_string(self.dir.join(TOKENS_FILE)) {
            Ok(text) => Ok(text.lines().any(|l| !l.trim().is_empty())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The paired machines' names, in pairing order.
    pub fn names(&self) -> io::Result<Vec<String>> {
        Ok(read_tokens(&self.dir)?
            .into_iter()
            .map(|(n, _)| n)
            .collect())
    }

    /// A single-use invite for `name`, good for `ttl`. Returns the secret. A
    /// new invite for the same name replaces an outstanding one.
    pub fn add_invite(&self, name: &str, ttl: Duration) -> io::Result<String> {
        check_name(name)?;
        let secret = random_token()?;
        let now = unix_now();
        let mut entries: Vec<_> = read_invites(&self.dir)?
            .into_iter()
            .filter(|(n, _, expires)| n != name && *expires > now)
            .collect();
        entries.push((
            name.to_string(),
            token_hash(&secret),
            now.saturating_add(ttl.as_secs()),
        ));
        write_invites(&self.dir, &entries)?;
        Ok(secret)
    }

    /// Exchange an invite's secret for its name and a fresh token. The invite
    /// is spent whether or not the rest succeeds, so it works once. An
    /// unknown, spent or expired secret gets `None`.
    pub fn redeem(&self, secret: &str) -> io::Result<Option<(String, String)>> {
        static REDEEM: Mutex<()> = Mutex::new(());
        let _one_at_a_time = REDEEM.lock().unwrap_or_else(|e| e.into_inner());
        let presented = token_hash(secret);
        let now = unix_now();
        let entries = read_invites(&self.dir)?;
        let before = entries.len();
        let mut found = None;
        let mut kept = Vec::with_capacity(before);
        for (name, hash, expires) in entries {
            if hashes_match(&hash, &presented) {
                if expires > now {
                    found = Some(name);
                }
            } else if expires > now {
                kept.push((name, hash, expires));
            }
        }
        if kept.len() != before {
            write_invites(&self.dir, &kept)?;
        }
        let Some(name) = found else {
            return Ok(None);
        };
        let token = self.add_token(&name)?;
        Ok(Some((name, token)))
    }

    fn add_token(&self, name: &str) -> io::Result<String> {
        check_name(name)?;
        let token = random_token()?;
        let mut entries: Vec<_> = read_tokens(&self.dir)?
            .into_iter()
            .filter(|(n, _)| n != name)
            .collect();
        entries.push((name.to_string(), token_hash(&token)));
        write_tokens(&self.dir, &entries)?;
        self.forget_cache();
        Ok(token)
    }

    /// Forget `name`'s token. Returns whether it was paired.
    pub fn remove(&self, name: &str) -> io::Result<bool> {
        let entries = read_tokens(&self.dir)?;
        let before = entries.len();
        let kept: Vec<_> = entries.into_iter().filter(|(n, _)| n != name).collect();
        if kept.len() == before {
            return Ok(false);
        }
        write_tokens(&self.dir, &kept)?;
        self.forget_cache();
        Ok(true)
    }

    /// The name a presented token belongs to: a machine's, or [`LOCAL_NAME`].
    /// `None` if it's nobody's. Reads `tokens` at most once a second, so
    /// `unpair` takes effect within that.
    pub fn verify(&self, token: &str) -> Option<String> {
        let presented = token_hash(token);
        if let Some(local) = &self.local_token_hash
            && hashes_match(local, &presented)
        {
            return Some(LOCAL_NAME.to_string());
        }
        let entries = self.tokens_cached()?;
        entries
            .into_iter()
            .find(|(_, hash)| hashes_match(hash, &presented))
            .map(|(name, _)| name)
    }

    /// Whether `name` is still paired (or is the local name).
    fn still_known(&self, name: &str) -> bool {
        if name == LOCAL_NAME {
            return self.local_token_hash.is_some();
        }
        self.tokens_cached()
            .is_some_and(|entries| entries.iter().any(|(n, _)| n == name))
    }

    fn tokens_cached(&self) -> Option<Vec<(String, String)>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, entries)) = cache.as_ref()
            && at.elapsed() < CACHE_FOR
        {
            return Some(entries.clone());
        }
        let entries = read_tokens(&self.dir).ok()?;
        *cache = Some((std::time::Instant::now(), entries.clone()));
        Some(entries)
    }

    fn forget_cache(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// myko#129's `Authenticator` over a [`Store`]: a bearer is a paired
/// machine's token or the local token, and a token that's gone revokes its
/// sessions.
pub struct PairingAuthenticator {
    store: Store,
}

impl PairingAuthenticator {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    fn identity(name: String) -> Result<AuthenticatedIdentity, AuthError> {
        Ok(AuthenticatedIdentity {
            user_id: None,
            principal: Principal {
                issuer: ISSUER.into(),
                subject: name.clone(),
                kind: PrincipalKind::Agent,
            },
            client_id: None,
            scopes: vec![],
            delegation: vec![],
            claims: serde_json::json!({ "machine": name }),
            // Tokens don't expire on their own; `unpair` ends them, through
            // `validate_identity`. The identity still needs a time.
            expires_at: SystemTime::now()
                .checked_add(Duration::from_secs(10 * 365 * 24 * 3600))
                .ok_or(AuthError::Unavailable)?,
        })
    }
}

impl Authenticator for PairingAuthenticator {
    fn authenticate(
        &self,
        credentials: Credentials,
    ) -> BoxFuture<'_, Result<AuthenticatedIdentity, AuthError>> {
        Box::pin(async move {
            let bearer = credentials.bearer.ok_or(AuthError::Unauthenticated)?;
            let name = self
                .store
                .verify(&bearer)
                .ok_or(AuthError::Unauthenticated)?;
            Self::identity(name)
        })
    }

    fn validate_identity(&self, identity: &AuthenticatedIdentity) -> Result<(), AuthError> {
        if self.store.still_known(&identity.principal.subject) {
            Ok(())
        } else {
            Err(AuthError::Revoked)
        }
    }
}

/// An `Authorizer` that allows every request from an authenticated caller.
/// Who may connect is the authenticator's; what a connection may do as whom
/// is MAR-80's `SessionBinding`, which wraps this.
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(
        &self,
        _identity: &AuthenticatedIdentity,
        _request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}

/// The daemon host's local token: read from `config_dir/local-token`, or made
/// and written there owner-only. The shim on the same host reads the same
/// file, so anything that can't read the operator's config directory can't
/// act as a local session.
pub fn ensure_local_token(config_dir: &Path) -> io::Result<String> {
    let path = config_dir.join(LOCAL_TOKEN_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.lines().next().unwrap_or("").trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }
    std::fs::create_dir_all(config_dir)?;
    let token = random_token()?;
    write_private(&path, format!("{token}\n").as_bytes())?;
    log::info!("[pairing] wrote a new local token to {}", path.display());
    Ok(token)
}

/// The operator's config directory, as the shim resolves it: `$XDG_CONFIG_HOME/marshal`,
/// else `~/.config/marshal` (`%APPDATA%\marshal` on Windows).
pub fn config_dir() -> Option<PathBuf> {
    let nonempty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(xdg) = nonempty("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("marshal"));
    }
    if cfg!(windows)
        && let Some(appdata) = nonempty("APPDATA")
    {
        return Some(PathBuf::from(appdata).join("marshal"));
    }
    nonempty("HOME").map(|home| PathBuf::from(home).join(".config").join("marshal"))
}

fn check_name(name: &str) -> io::Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a machine name is 1 to 64 letters, digits, '-', '_' or '.'",
        ))
    }
}

/// 32 random bytes as URL-safe base64: a token or an invite's secret.
fn random_token() -> io::Result<String> {
    use base64::Engine as _;
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(io::Error::other)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_tokens(dir: &Path) -> io::Result<Vec<(String, String)>> {
    let text = match std::fs::read_to_string(dir.join(TOKENS_FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let (name, hash) = line.trim().split_once(' ')?;
            Some((name.to_string(), hash.trim().to_string()))
        })
        .collect())
}

fn write_tokens(dir: &Path, entries: &[(String, String)]) -> io::Result<()> {
    let text: String = entries.iter().map(|(n, h)| format!("{n} {h}\n")).collect();
    write_private_file(dir, TOKENS_FILE, text.as_bytes())
}

fn read_invites(dir: &Path) -> io::Result<Vec<(String, String, u64)>> {
    let text = match std::fs::read_to_string(dir.join(INVITES_FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (name, hash, expires) = (parts.next()?, parts.next()?, parts.next()?);
            Some((name.to_string(), hash.to_string(), expires.parse().ok()?))
        })
        .collect())
}

fn write_invites(dir: &Path, entries: &[(String, String, u64)]) -> io::Result<()> {
    let text: String = entries
        .iter()
        .map(|(n, h, e)| format!("{n} {h} {e}\n"))
        .collect();
    write_private_file(dir, INVITES_FILE, text.as_bytes())
}

/// Write `dir/name` owner-only through a sibling and a rename.
fn write_private_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!("{name}.tmp.{}", std::process::id()));
    write_private(&tmp, bytes)?;
    std::fs::rename(tmp, dir.join(name))
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        (dir, store)
    }

    #[test]
    fn an_invite_pairs_once_and_the_token_verifies_until_removed() {
        let (_dir, store) = store();
        assert!(!store.any_pairings().unwrap());
        let secret = store.add_invite("laptop", Duration::from_secs(60)).unwrap();
        assert_eq!(store.redeem("wrong").unwrap(), None);
        let (name, token) = store
            .redeem(&secret)
            .unwrap()
            .expect("a fresh invite redeems");
        assert_eq!(name, "laptop");
        assert_eq!(store.redeem(&secret).unwrap(), None, "an invite works once");
        assert!(store.any_pairings().unwrap());
        assert_eq!(store.verify(&token), Some("laptop".into()));
        assert_eq!(store.verify("not-it"), None);
        assert_eq!(store.names().unwrap(), vec!["laptop".to_string()]);
        assert!(store.remove("laptop").unwrap());
        assert!(!store.remove("laptop").unwrap());
        assert_eq!(
            store.verify(&token),
            None,
            "removed at once, not after the cache"
        );
    }

    #[test]
    fn an_expired_invite_is_refused_and_a_new_one_replaces_the_old() {
        let (_dir, store) = store();
        let old = store.add_invite("laptop", Duration::from_secs(0)).unwrap();
        assert_eq!(store.redeem(&old).unwrap(), None);
        let first = store.add_invite("desk", Duration::from_secs(60)).unwrap();
        let second = store.add_invite("desk", Duration::from_secs(60)).unwrap();
        assert_eq!(store.redeem(&first).unwrap(), None);
        assert!(store.redeem(&second).unwrap().is_some());
    }

    #[test]
    fn a_bad_name_is_refused() {
        let (_dir, store) = store();
        assert!(
            store
                .add_invite("has space", Duration::from_secs(60))
                .is_err()
        );
        assert!(store.add_invite("", Duration::from_secs(60)).is_err());
        assert!(
            store
                .add_invite(&"x".repeat(65), Duration::from_secs(60))
                .is_err()
        );
    }

    #[test]
    fn the_local_token_verifies_as_local_and_is_kept_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let token = ensure_local_token(dir.path()).unwrap();
        assert_eq!(ensure_local_token(dir.path()).unwrap(), token);
        let store = Store::open(dir.path()).with_local_token(&token);
        assert_eq!(store.verify(&token), Some(LOCAL_NAME.into()));
        assert!(store.still_known(LOCAL_NAME));
        assert!(!Store::open(dir.path()).still_known(LOCAL_NAME));
    }

    #[cfg(unix)]
    #[test]
    fn the_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, store) = store();
        store.add_invite("laptop", Duration::from_secs(60)).unwrap();
        ensure_local_token(dir.path()).unwrap();
        for path in [
            dir.path().join(PAIRING_DIR),
            dir.path().join(PAIRING_DIR).join(INVITES_FILE),
            dir.path().join(LOCAL_TOKEN_FILE),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert!(
                mode == 0o600 || mode == 0o700,
                "{}: {mode:03o}",
                path.display()
            );
        }
    }
}
