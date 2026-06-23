//! Login rate-limit bucket (CLAUDE.md Security baseline: "Login / password
//! endpoints: 10 req/min per IP" / `LIED_RATELIMIT_LOGIN_PER_MIN`).
//!
//! Built on `tower_governor`, keyed by client IP via [`SmartIpKeyExtractor`],
//! which reads the real client address from `X-Forwarded-For` / `X-Real-IP`
//! (the standard reverse-proxy headers) and falls back to the TCP peer IP —
//! the fallback requires the server to be served with
//! `into_make_service_with_connect_info::<SocketAddr>()` (wired in
//! `lied-server/src/main.rs`).
//!
//! `PeerIpKeyExtractor` was rejected: Lied's documented deployment is a
//! container behind nginx/traefik, where the peer IP is the *proxy's*
//! address — that collapses every client into one shared bucket, so a single
//! attacker would lock out all users while no individual attacker is ever
//! isolated. `SmartIpKeyExtractor` keys on the forwarded client IP instead.
//! Caveat: those headers are client-spoofable when Lied is *not* behind a
//! trusted proxy; a per-deployment "trust proxy headers" toggle is a
//! follow-up, but the proxy topology is the one we optimize for.
//!
//! Only the login/password surface gets this bucket in this item — general
//! per-identity/per-IP limits (`LIED_RATELIMIT_AUTH_PER_MIN` /
//! `LIED_RATELIMIT_ANON_PER_MIN`) are a broader cross-cutting concern left
//! to a later item, per the issue scope ("Only the login/password surface
//! needs it in this item").

use std::sync::Arc;
use std::time::Duration;

use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_governor::GovernorLayer;

/// Build a `tower_governor` layer for the login endpoint: `burst_size =
/// limit_per_min`, replenishing one token every `60 / limit_per_min`
/// seconds — i.e. a steady-state cap of `limit_per_min` requests/minute per
/// client IP, matching `config.ratelimit_login_per_min`.
///
/// Also spawns the crate's recommended periodic GC: `tower_governor` keeps
/// in-memory state for every distinct key (IP) it has seen, which would grow
/// without bound on an internet-exposed login endpoint. A detached task calls
/// `retain_recent()` once a minute to evict idle buckets.
pub fn login_rate_limit_layer(
    limit_per_min: u32,
) -> GovernorLayer<SmartIpKeyExtractor, governor::middleware::NoOpMiddleware> {
    let limit_per_min = limit_per_min.max(1);
    let period_ms = (60_000 / u64::from(limit_per_min)).max(1);

    let config = GovernorConfigBuilder::default()
        .key_extractor(SmartIpKeyExtractor)
        .per_millisecond(period_ms)
        .burst_size(limit_per_min)
        .finish()
        .expect("burst_size and period are both non-zero by construction");

    let config = Arc::new(config);

    // Evict idle per-IP buckets so the in-memory map can't grow unbounded.
    // Only spawn when a Tokio runtime is actually running (it always is at
    // router-build time in the server and in `#[tokio::test]`s); constructing
    // the layer outside a runtime — e.g. a plain unit test — just skips the
    // GC task rather than panicking.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let gc = config.limiter().clone();
        handle.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                gc.retain_recent();
            }
        });
    }

    GovernorLayer { config }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_layer_without_panicking() {
        let _layer = login_rate_limit_layer(10);
    }

    #[test]
    fn builds_layer_for_minimum_limit() {
        let _layer = login_rate_limit_layer(0);
    }
}
