//! argon2id password hashing for local accounts (CLAUDE.md "Auth &
//! access": "Local accounts (username/password, argon2id hashing)").
//!
//! Used for `User.password_hash` (local login) only. App-password tokens
//! also use argon2id, but via [`crate::auth::app_password`] — the two are
//! kept in separate modules because their hash/verify call sites have
//! different shapes (one password per user vs. a prefix-narrowed lookup
//! among many tokens).

use std::sync::OnceLock;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

#[derive(thiserror::Error, Debug)]
pub enum PasswordError {
    #[error("failed to hash password")]
    Hash,
    #[error("failed to parse stored password hash")]
    InvalidHash,
}

/// A real argon2id hash (of a throwaway password) used only to burn an
/// equivalent verify cost on the user-not-found / no-local-password path.
/// Computed once with `Argon2::default()` so its cost matches a genuine
/// `verify_password` against a stored hash. See [`verify_dummy`].
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        // Hashing a fixed input cannot realistically fail; if it ever did,
        // fall back to a constant PHC string so callers still pay a parse +
        // (failed) verify rather than returning instantly.
        hash_password("lied-timing-equalization-dummy").unwrap_or_else(|_| {
            "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()
        })
    })
}

/// Run an argon2 verify against a fixed dummy hash, discarding the result.
///
/// Called on the authentication path when no user (or no local password
/// hash) was found, so that the request spends the same ~argon2 time it
/// would for a real user with a wrong password. Without this, "no such
/// username" returns in microseconds while a real username runs the slow
/// verify, leaking account existence through response latency (username
/// enumeration). See [`crate::auth::session::login`].
pub fn verify_dummy() {
    let _ = verify_password("lied-not-a-real-password", dummy_hash());
}

/// Hash a plaintext password with argon2id (the crate default algorithm)
/// and a fresh random salt. Returns the full PHC string (algorithm + salt +
/// hash combined) suitable for storing directly in `User.password_hash`.
pub fn hash_password(plaintext: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|_| PasswordError::Hash)?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against a stored PHC hash string. Returns
/// `Ok(true)`/`Ok(false)` for a well-formed hash; `Err` only if the stored
/// hash string itself is malformed (a data integrity problem, not a wrong
/// password).
pub fn verify_password(plaintext: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(stored_hash).map_err(|_| PasswordError::InvalidHash)?;
    Ok(Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(!verify_password("wrong password", &hash).unwrap());
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        let result = verify_password("anything", "not-a-valid-phc-hash");
        assert!(matches!(result, Err(PasswordError::InvalidHash)));
    }

    #[test]
    fn hash_uses_distinct_salts() {
        let h1 = hash_password("same-password").unwrap();
        let h2 = hash_password("same-password").unwrap();
        assert_ne!(h1, h2, "hashes of the same password should differ by salt");
    }

    /// Regression for the username-enumeration timing oracle (#4): the dummy
    /// verify on the not-found path must cost roughly the same as a real
    /// verify, not return instantly. Compared *relative* to a real verify so
    /// the assertion is robust across machine speeds: an unmitigated
    /// not-found path would be ~microseconds (orders of magnitude faster),
    /// while the dummy runs a full argon2 verify (~same order as the real
    /// one).
    #[test]
    fn dummy_verify_costs_about_the_same_as_a_real_verify() {
        use std::time::Instant;

        let real_hash = hash_password("a-real-password").unwrap();

        // Warm both paths (first call to `dummy_hash` computes + caches it).
        verify_dummy();
        let _ = verify_password("wrong", &real_hash);

        let t0 = Instant::now();
        let _ = verify_password("wrong", &real_hash);
        let real = t0.elapsed();

        let t1 = Instant::now();
        verify_dummy();
        let dummy = t1.elapsed();

        assert!(
            dummy.as_secs_f64() >= real.as_secs_f64() * 0.25,
            "dummy verify ({dummy:?}) should be the same order as a real verify ({real:?}); \
             a near-instant dummy means the not-found path skips argon2 and leaks user existence"
        );
    }
}
