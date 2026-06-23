//! WebDAV app-password token generation and verification (CLAUDE.md
//! Entities: AppPassword).
//!
//! Token format: `lied_<base64url(32 random bytes)>`. The plaintext is
//! returned to the caller exactly once, at creation; only an argon2id hash
//! plus an 8-character `prefix` (the first 8 characters of the *full* token
//! string, i.e. `lied_xx`) are persisted. The prefix lets WebDAV Basic-auth
//! narrow `SELECT ... WHERE user_id = ? AND prefix = ?` to a small candidate
//! set before paying argon2's intentionally slow verify cost (CLAUDE.md
//! AppPassword entity notes).

use argon2::password_hash::rand_core::OsRng as PasswordOsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;

pub const TOKEN_PREFIX: &str = "lied_";
/// Length, in characters, of the stored `AppPassword.prefix` column — the
/// first 8 characters of the full token string (CLAUDE.md: "first 8 chars
/// of the token"), including the `lied_` literal.
pub const PREFIX_LEN: usize = 8;

#[derive(thiserror::Error, Debug)]
pub enum AppPasswordError {
    #[error("failed to hash app password token")]
    Hash,
    #[error("failed to parse stored app password hash")]
    InvalidHash,
}

/// A freshly minted token: the plaintext (shown once), its prefix, and its
/// argon2id hash (what actually gets persisted).
pub struct GeneratedToken {
    pub plaintext: String,
    pub prefix: String,
    pub hash: String,
}

/// Generate a new `lied_<base64url(32 random bytes)>` token and hash it.
pub fn generate() -> Result<GeneratedToken, AppPasswordError> {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let plaintext = format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let prefix = plaintext.chars().take(PREFIX_LEN).collect::<String>();
    let hash = hash_token(&plaintext)?;
    Ok(GeneratedToken {
        plaintext,
        prefix,
        hash,
    })
}

/// Hash a plaintext app-password token with argon2id. Separate from
/// [`crate::auth::password::hash_password`] only by call site/intent — same
/// algorithm choice (CLAUDE.md: "argon2id of the random token").
pub fn hash_token(plaintext: &str) -> Result<String, AppPasswordError> {
    let salt = SaltString::generate(&mut PasswordOsRng);
    let hash = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|_| AppPasswordError::Hash)?;
    Ok(hash.to_string())
}

/// Verify a plaintext token against a stored hash.
pub fn verify_token(plaintext: &str, stored_hash: &str) -> Result<bool, AppPasswordError> {
    let parsed = PasswordHash::new(stored_hash).map_err(|_| AppPasswordError::InvalidHash)?;
    Ok(Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

/// Extract the lookup prefix from a presented plaintext token (first
/// [`PREFIX_LEN`] characters), for the `WHERE user_id = ? AND prefix = ?`
/// narrowing query.
pub fn prefix_of(plaintext: &str) -> String {
    plaintext.chars().take(PREFIX_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_has_expected_shape() {
        let token = generate().unwrap();
        assert!(token.plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(token.prefix.len(), PREFIX_LEN);
        assert!(token.plaintext.starts_with(&token.prefix));
    }

    #[test]
    fn verify_round_trips() {
        let token = generate().unwrap();
        assert!(verify_token(&token.plaintext, &token.hash).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let token = generate().unwrap();
        assert!(!verify_token("lied_wrong-token-value", &token.hash).unwrap());
    }

    #[test]
    fn prefix_of_matches_generated_prefix() {
        let token = generate().unwrap();
        assert_eq!(prefix_of(&token.plaintext), token.prefix);
    }

    #[test]
    fn distinct_tokens_are_generated() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a.plaintext, b.plaintext);
    }
}
