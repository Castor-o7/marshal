//! Pairing codes (MAR-24, from muster's pairing). Fork only.
//!
//! `marshal-daemon pair-code <name>` prints an [`Invite`]: where the daemon
//! is, its certificate's fingerprint, and a one-time secret. `marshal-shim
//! pair <code>` exchanges the secret at the daemon's `/pair` for a token, and
//! keeps the result as a [`Pairing`]. Both are one line of URL-safe base64
//! over JSON behind a prefix, so they paste and parse without quoting.
//!
//! Only hashes of tokens and secrets are stored on the daemon
//! ([`token_hash`]), so its files can't be used to connect.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const INVITE_PREFIX: &str = "marshal-invite-v1:";
const PAIRING_PREFIX: &str = "marshal-pairing-v1:";

/// A one-time invite to pair a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// The daemon's `host:port`, as the machine should dial it.
    pub address: String,
    /// Its hook listener's `host:port`, where `/pair` and the hooks are.
    pub hooks: String,
    /// Lowercase hex SHA-256 of the daemon certificate's DER.
    pub fingerprint: String,
    /// The secret `/pair` exchanges for a token, once.
    pub invite: String,
    /// The name the machine will be paired under.
    pub name: String,
}

/// A machine's pairing: what it needs to connect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub address: String,
    pub hooks: String,
    pub fingerprint: String,
    /// The access token, presented as a bearer on every connection.
    pub token: String,
    pub name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CodeError {
    /// Doesn't start with a known prefix.
    NotACode,
    /// Starts right, but the rest doesn't decode or lacks a field.
    Malformed,
}

impl std::fmt::Display for CodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotACode => "not a marshal pairing code",
            Self::Malformed => "a marshal pairing code, but damaged",
        })
    }
}

impl std::error::Error for CodeError {}

fn encode<T: Serialize>(prefix: &str, value: &T) -> String {
    let json = serde_json::to_vec(value).expect("codes serialize");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(json))
}

fn decode<T: for<'de> Deserialize<'de>>(prefix: &str, code: &str) -> Result<T, CodeError> {
    let payload = code
        .trim()
        .strip_prefix(prefix)
        .ok_or(CodeError::NotACode)?;
    let json = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| CodeError::Malformed)?;
    serde_json::from_slice(&json).map_err(|_| CodeError::Malformed)
}

pub fn valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Invite {
    pub fn to_code(&self) -> String {
        encode(INVITE_PREFIX, self)
    }

    pub fn from_code(code: &str) -> Result<Self, CodeError> {
        let invite: Self = decode(INVITE_PREFIX, code)?;
        if invite.address.is_empty()
            || invite.hooks.is_empty()
            || invite.invite.is_empty()
            || !valid_name(&invite.name)
            || !valid_fingerprint(&invite.fingerprint)
        {
            return Err(CodeError::Malformed);
        }
        Ok(invite)
    }
}

impl Pairing {
    pub fn to_code(&self) -> String {
        encode(PAIRING_PREFIX, self)
    }

    pub fn from_code(code: &str) -> Result<Self, CodeError> {
        let pairing: Self = decode(PAIRING_PREFIX, code)?;
        if pairing.address.is_empty()
            || pairing.hooks.is_empty()
            || pairing.token.is_empty()
            || !valid_fingerprint(&pairing.fingerprint)
        {
            return Err(CodeError::Malformed);
        }
        Ok(pairing)
    }

    /// For showing: the token replaced by its length.
    pub fn masked(&self) -> String {
        format!(
            "paired as {} with wss://{} (fingerprint {}, token of {} characters)",
            self.name,
            self.address,
            self.fingerprint,
            self.token.len()
        )
    }
}

/// What a pairing code or token in a message body is replaced with (MAR-86).
pub const CODE_REDACTED: &str = "[marshal pairing code redacted]";
pub const TOKEN_REDACTED: &str = "[marshal token redacted]";

/// Says whether a base64url run in a body is a token the daemon knows. The
/// daemon installs one over its store with `--tls`; without one (plain mode)
/// only codes are redacted, since a bare token can't be told from any other
/// base64 by its shape.
pub type SecretCheck = dyn Fn(&str) -> bool + Send + Sync;

static SECRET_CHECK: std::sync::OnceLock<std::sync::Arc<SecretCheck>> = std::sync::OnceLock::new();

/// Install the token check. Once; a later call changes nothing.
pub fn install_secret_check(check: std::sync::Arc<SecretCheck>) {
    let _ = SECRET_CHECK.set(check);
}

/// The length of a token or invite secret: 32 random bytes as base64url.
const TOKEN_LEN: usize = 43;

fn is_base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// `body` with every pairing code, and every run the installed check knows
/// as a token, replaced; and how many were. Everything else is untouched.
pub fn redact_secrets(body: &str) -> (String, usize) {
    let check = SECRET_CHECK.get();
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut redacted = 0;
    let mut i = 0;
    while i < bytes.len() {
        // A code: its prefix and the base64url run after it.
        let code = [INVITE_PREFIX, PAIRING_PREFIX]
            .iter()
            .find(|prefix| body[i..].starts_with(*prefix));
        if let Some(prefix) = code {
            let start = i + prefix.len();
            let end = start
                + bytes[start..]
                    .iter()
                    .take_while(|b| is_base64url(**b))
                    .count();
            if end > start {
                out.push_str(CODE_REDACTED);
                redacted += 1;
                i = end;
                continue;
            }
        }
        // A bare run of base64url, maybe a token.
        if is_base64url(bytes[i]) && (i == 0 || !is_base64url(bytes[i - 1])) {
            let end = i + bytes[i..].iter().take_while(|b| is_base64url(**b)).count();
            if end - i == TOKEN_LEN
                && let Some(check) = check
                && check(&body[i..end])
            {
                out.push_str(TOKEN_REDACTED);
                redacted += 1;
                i = end;
                continue;
            }
            out.push_str(&body[i..end]);
            i = end;
            continue;
        }
        let ch = body[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, redacted)
}

/// A machine name: 1 to 64 letters, digits, `-`, `_` or `.`. It appears in
/// file lines and log lines, so nothing else.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Lowercase hex SHA-256 of a token or secret: what the daemon stores and
/// compares, never the token itself.
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether two hashes are the same, in time that doesn't depend on where
/// they differ.
pub fn hashes_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_round_trips_and_a_damaged_one_is_refused() {
        let invite = Invite {
            address: "murph:6155".into(),
            hooks: "murph:6156".into(),
            fingerprint: "ab".repeat(32),
            invite: "s3cret".into(),
            name: "laptop".into(),
        };
        let code = invite.to_code();
        assert!(code.starts_with(INVITE_PREFIX));
        assert_eq!(Invite::from_code(&format!(" {code}\n")), Ok(invite.clone()));
        assert_eq!(Invite::from_code("hello"), Err(CodeError::NotACode));
        assert_eq!(
            Invite::from_code(&format!("{INVITE_PREFIX}!!!")),
            Err(CodeError::Malformed)
        );
        let bad = Invite {
            name: "no spaces".into(),
            ..invite
        };
        assert_eq!(Invite::from_code(&bad.to_code()), Err(CodeError::Malformed));
    }

    #[test]
    fn a_pairing_round_trips_and_masks_its_token() {
        let pairing = Pairing {
            address: "murph:6155".into(),
            hooks: "murph:6156".into(),
            fingerprint: "cd".repeat(32),
            token: "tok".repeat(10),
            name: "laptop".into(),
        };
        assert_eq!(Pairing::from_code(&pairing.to_code()), Ok(pairing.clone()));
        let masked = pairing.masked();
        assert!(masked.contains("30 characters") && !masked.contains(&pairing.token));
    }

    #[test]
    fn hashes_compare_whole() {
        let h = token_hash("a");
        assert_eq!(h.len(), 64);
        assert!(hashes_match(&h, &token_hash("a")));
        assert!(!hashes_match(&h, &token_hash("b")));
        assert!(!hashes_match(&h, "short"));
    }

    #[test]
    fn codes_are_redacted_wherever_they_sit_and_nothing_else_is() {
        let invite = Invite {
            address: "murph:6155".into(),
            hooks: "murph:6156".into(),
            fingerprint: "ab".repeat(32),
            invite: "s3cret".into(),
            name: "laptop".into(),
        }
        .to_code();
        let body = format!("pair with {invite} please, and keep marshal-invite-v1: as a word");
        let (out, n) = redact_secrets(&body);
        assert_eq!(n, 1, "{out}");
        assert!(
            !out.contains(&invite) && out.contains(CODE_REDACTED),
            "{out}"
        );
        assert!(out.ends_with("keep marshal-invite-v1: as a word"), "{out}");
        assert_eq!(
            redact_secrets("nothing here, 01a1c2a0-dfa5-7a33-a234-1eabc97d86e8").1,
            0
        );
        assert_eq!(redact_secrets("héllo wörld").0, "héllo wörld");
    }

    #[test]
    fn a_known_token_is_redacted_only_through_the_installed_check() {
        let token = "A".repeat(TOKEN_LEN);
        let other = "B".repeat(TOKEN_LEN);
        let known = token.clone();
        install_secret_check(std::sync::Arc::new(move |run: &str| run == known));
        let (out, n) = redact_secrets(&format!(
            "token {token} and not {other}, nor {}",
            "A".repeat(42)
        ));
        assert_eq!(n, 1, "{out}");
        assert!(
            out.contains(TOKEN_REDACTED) && out.contains(&other),
            "{out}"
        );
    }
}
