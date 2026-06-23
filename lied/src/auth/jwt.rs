//! Bearer tokens: HS256 JWTs with a `kid` header and a keyring that can
//! honor an old signing key during a rotation grace window (CLAUDE.md
//! "Tech stack": jsonwebtoken section).
//!
//! Phase 1 seeds the keyring from the single `config.jwt_signing_key` — see
//! [`Keyring::from_single_key`]. The `kid` is derived deterministically from
//! the key material (a short hash) rather than configured separately, so
//! there is nothing extra an operator needs to set today; a future
//! hot-rotation endpoint can construct a multi-key [`Keyring`] the same way
//! this one does, by inserting an additional `(kid, key)` pair and keeping
//! the old one for the grace window.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Claims carried by every Lied bearer token (CLAUDE.md: "Claims: `sub`
/// (user id), `org` (active org id, nullable...), `iat`, `exp`, `jti`").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub org: Option<Uuid>,
    pub iat: i64,
    pub exp: i64,
    pub jti: Uuid,
}

#[derive(thiserror::Error, Debug)]
pub enum JwtError {
    #[error("token is missing a kid header")]
    MissingKid,
    #[error("token kid does not match any known signing key")]
    UnknownKid,
    #[error("token encoding failed")]
    Encode(#[source] jsonwebtoken::errors::Error),
    #[error("token is invalid or expired")]
    Invalid(#[source] jsonwebtoken::errors::Error),
}

/// Derive a stable, non-secret `kid` from key material: the first 16 hex
/// characters of its SHA-256 digest. Deterministic so the same key always
/// yields the same `kid` across restarts (no separate kid storage needed),
/// and one-way so the `kid` itself leaks nothing about the key.
fn derive_kid(key_material: &str) -> String {
    let digest = Sha256::digest(key_material.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A signing key plus its derived `kid`, and the means to encode/decode
/// with it.
struct KeyEntry {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

/// Holds the current signing key plus a `kid`-keyed map of every key
/// (current + any still-honored old ones) usable for *verification*. New
/// tokens are always signed with [`Keyring::current_kid`]; verification
/// selects the matching key by the token's `kid` header rather than trying
/// every key in turn (CLAUDE.md: "the verifier selects by `kid` rather than
/// trial-verifying against each").
pub struct Keyring {
    current_kid: String,
    keys: HashMap<String, KeyEntry>,
    lifetime: Duration,
}

impl Keyring {
    /// Seed a keyring from a single signing key (phase-1 shape: one
    /// operator-configured `LIED_JWT_SIGNING_KEY`). The `kid` is derived
    /// from the key material itself.
    pub fn from_single_key(signing_key: &str, lifetime_days: i64) -> Self {
        let kid = derive_kid(signing_key);
        let mut keys = HashMap::new();
        keys.insert(
            kid.clone(),
            KeyEntry {
                encoding: EncodingKey::from_secret(signing_key.as_bytes()),
                decoding: DecodingKey::from_secret(signing_key.as_bytes()),
            },
        );
        Self {
            current_kid: kid,
            keys,
            lifetime: Duration::days(lifetime_days),
        }
    }

    /// Mint a fresh bearer token for `user_id`, optionally scoped to an
    /// active org. Signed with the current key; lifetime per
    /// `config.jwt_lifetime_days`.
    pub fn mint(&self, user_id: Uuid, org: Option<Uuid>) -> Result<(String, Claims), JwtError> {
        let entry = self
            .keys
            .get(&self.current_kid)
            .expect("current_kid always has a corresponding entry");

        let now: DateTime<Utc> = Utc::now();
        let claims = Claims {
            sub: user_id,
            org,
            iat: now.timestamp(),
            exp: (now + self.lifetime).timestamp(),
            jti: Uuid::now_v7(),
        };

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(self.current_kid.clone());

        let token = encode(&header, &claims, &entry.encoding).map_err(JwtError::Encode)?;
        Ok((token, claims))
    }

    /// Verify a presented bearer token: looks up the signing key by the
    /// token's `kid` header (no trial-and-error across keys), then verifies
    /// signature + expiry.
    pub fn verify(&self, token: &str) -> Result<Claims, JwtError> {
        let header = jsonwebtoken::decode_header(token).map_err(JwtError::Invalid)?;
        let kid = header.kid.ok_or(JwtError::MissingKid)?;
        let entry = self.keys.get(&kid).ok_or(JwtError::UnknownKid)?;

        let validation = Validation::new(Algorithm::HS256);
        let data =
            decode::<Claims>(token, &entry.decoding, &validation).map_err(JwtError::Invalid)?;
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_then_verify_round_trips() {
        let keyring = Keyring::from_single_key("test-signing-key-material", 30);
        let user_id = Uuid::now_v7();
        let (token, minted_claims) = keyring.mint(user_id, None).unwrap();

        let claims = keyring.verify(&token).unwrap();
        assert_eq!(claims.sub, user_id);
        assert_eq!(claims.jti, minted_claims.jti);
        assert_eq!(claims.org, None);
    }

    #[test]
    fn mint_carries_org_claim() {
        let keyring = Keyring::from_single_key("test-signing-key-material", 30);
        let org_id = Uuid::now_v7();
        let (token, _) = keyring.mint(Uuid::now_v7(), Some(org_id)).unwrap();

        let claims = keyring.verify(&token).unwrap();
        assert_eq!(claims.org, Some(org_id));
    }

    #[test]
    fn verify_rejects_token_from_different_key() {
        let keyring_a = Keyring::from_single_key("key-a", 30);
        let keyring_b = Keyring::from_single_key("key-b", 30);
        let (token, _) = keyring_a.mint(Uuid::now_v7(), None).unwrap();

        let result = keyring_b.verify(&token);
        assert!(matches!(result, Err(JwtError::UnknownKid)));
    }

    #[test]
    fn verify_rejects_expired_token() {
        // Negative lifetime: exp is already in the past at mint time.
        let keyring = Keyring::from_single_key("test-signing-key-material", -1);
        let (token, _) = keyring.mint(Uuid::now_v7(), None).unwrap();

        let result = keyring.verify(&token);
        assert!(matches!(result, Err(JwtError::Invalid(_))));
    }

    #[test]
    fn verify_rejects_garbage_token() {
        let keyring = Keyring::from_single_key("test-signing-key-material", 30);
        let result = keyring.verify("not.a.jwt");
        assert!(result.is_err());
    }

    #[test]
    fn distinct_keys_yield_distinct_kids() {
        let kid_a = derive_kid("key-a");
        let kid_b = derive_kid("key-b");
        assert_ne!(kid_a, kid_b);
    }

    #[test]
    fn same_key_yields_same_kid_deterministically() {
        assert_eq!(derive_kid("stable-key"), derive_kid("stable-key"));
    }
}
