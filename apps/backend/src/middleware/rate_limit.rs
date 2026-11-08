//! General-purpose keyed rate limiting.
//!
//! [`limit_by_ip`] (via [`keyed_ip_limiter`]) for pre-auth/anonymous endpoints, [`limit_by_user`]
//! (via [`keyed_user_limiter`]) for authenticated ones where per-IP would mis-count (NAT, shared
//! IPs).

use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, FromRef, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use governor::clock::DefaultClock;
use governor::{Quota, RateLimiter};

use crate::auth::CurrentUser;
use crate::error::ApiError;
use crate::state::AppState;

pub type IpKeyedLimiter = governor::DefaultKeyedRateLimiter<IpAddr>;
pub type UserKeyedLimiter = governor::DefaultKeyedRateLimiter<i64>;

/// Replicas the in-process limiters divide their quota across, so the product-wide limit matches
/// the configured one. 1 when `RATE_LIMIT_REPLICAS` is unset.
fn rate_limit_replicas() -> u32 {
    arcodash_core::config::env_parsed("RATE_LIMIT_REPLICAS", 1).max(1)
}

/// Read `env_var` as a per-minute quota (lenient `default`), divided by [`rate_limit_replicas`] so
/// the configured number is the product-wide limit.
pub fn quota_per_minute(env_var: &str, default: u32) -> Quota {
    let n = NonZeroU32::new(arcodash_core::config::env_parsed(env_var, default))
        .unwrap_or_else(|| NonZeroU32::new(default).expect("default rate limit must be nonzero"));

    let replicas = rate_limit_replicas();
    let per_replica = NonZeroU32::new((n.get() / replicas).max(1)).expect("max(1) is never zero");
    tracing::info!(
        env_var,
        configured_per_min = n.get(),
        replicas,
        effective_per_replica_per_min = per_replica.get(),
        "rate limiter quota"
    );
    Quota::per_minute(per_replica)
}

/// Combined state for [`limit_by_ip`]: the keyed limiter plus the trusted-proxy-hop count, read
/// once here (env reads take a lock and allocate).
///
/// Not a process-global `OnceLock`: tests set `TRUSTED_PROXY_HOPS` per `spawn_app()` and a
/// `OnceLock` would freeze the first value.
#[derive(Clone)]
pub struct IpLimiterState {
    limiter: Arc<IpKeyedLimiter>,
    trusted_hops: u32,
}

#[must_use]
pub fn keyed_ip_limiter(quota: Quota) -> IpLimiterState {
    IpLimiterState {
        limiter: swept_keyed_limiter(quota),
        trusted_hops: trusted_proxy_hops(),
    }
}

#[must_use]
pub fn keyed_user_limiter(quota: Quota) -> Arc<UserKeyedLimiter> {
    swept_keyed_limiter(quota)
}

/// How often each keyed limiter drops decayed entries; `governor`'s in-memory store never shrinks
/// on its own and would grow unbounded.
const RETAIN_RECENT_INTERVAL: Duration = Duration::from_secs(600);

type KeyedLimiter<K> =
    RateLimiter<K, governor::state::keyed::DefaultKeyedStateStore<K>, DefaultClock>;

/// A keyed limiter plus a periodic background sweep evicting its stale entries; the sweep's
/// `JoinHandle` is dropped, so it runs for the process lifetime.
fn swept_keyed_limiter<K>(quota: Quota) -> Arc<KeyedLimiter<K>>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
{
    let limiter = Arc::new(RateLimiter::keyed(quota));
    let swept = limiter.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETAIN_RECENT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            swept.retain_recent();
        }
    });
    limiter
}

const TOO_MANY_REQUESTS: &str = "too many requests, please try again later";

/// Default cap: 10 login attempts per minute per IP, overridable via `LOGIN_RATE_LIMIT_PER_MIN`.
#[must_use]
pub fn build_login_rate_limiter() -> IpLimiterState {
    keyed_ip_limiter(quota_per_minute("LOGIN_RATE_LIMIT_PER_MIN", 10))
}

/// Trusted reverse-proxy hops in front of this process (`TRUSTED_PROXY_HOPS`), read at
/// limiter-construction time, not cached globally (see [`IpLimiterState`]). `Config::from_env`
/// already validated the raw value.
fn trusted_proxy_hops() -> u32 {
    arcodash_core::config::env_parsed("TRUSTED_PROXY_HOPS", 0)
}

/// Pick the rate-limiting key: the peer address when `trusted_hops` is `0`, else the
/// `trusted_hops`-th address from the right of `X-Forwarded-For`. Counting from the right is
/// unspoofable: clients can only prepend on the left. Falls back to the peer address if the header
/// is absent, too short, or the trusted entry doesn't parse (fail closed, never the leftmost
/// value).
fn select_rate_limit_key(peer: IpAddr, xff_header: Option<&str>, trusted_hops: u32) -> IpAddr {
    let Some(header) = xff_header.filter(|_| trusted_hops > 0) else {
        return peer;
    };
    header
        .split(',')
        .rev()
        .nth((trusted_hops - 1) as usize)
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

/// `HeaderMap::get` returns only the first `X-Forwarded-For` line; a client sending the header
/// twice could rotate that line and mint fresh keys when `trusted_hops > 0`. Join all lines in wire
/// order (RFC 7230 §3.2.2) before [`select_rate_limit_key`], so the right-anchored trusted entry is
/// the same however the client splits its prefix.
fn join_xff_header_lines(headers: &axum::http::HeaderMap) -> Option<String> {
    let mut lines = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .peekable();
    lines.peek()?;
    Some(lines.collect::<Vec<_>>().join(", "))
}

/// `from_fn_with_state` handler: rejects with 429 once the calling IP exceeds its budget.
pub async fn limit_by_ip(
    State(state): State<IpLimiterState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let xff = join_xff_header_lines(request.headers());
    let key = select_rate_limit_key(addr.ip(), xff.as_deref(), state.trusted_hops);
    if state.limiter.check_key(&key).is_ok() {
        return Ok(next.run(request).await);
    }
    crate::metrics::incr_ip_rejection();
    // Highest-value rejection log in the auth stack (brute force on login/SAML/LDAP). Never log the
    // query string.
    tracing::warn!(ip = %key, path = %request.uri().path(), "rate limit exceeded: rejecting request by ip");
    Err(ApiError::too_many_requests(TOO_MANY_REQUESTS))
}

/// Combined state for [`limit_by_user`]: `AppState` (to resolve `CurrentUser`) plus the
/// route-specific limiter, with `FromRef` impls for each half.
#[derive(Clone)]
pub struct UserLimiterState {
    pub app: AppState,
    pub limiter: Arc<UserKeyedLimiter>,
}

impl FromRef<UserLimiterState> for AppState {
    fn from_ref(input: &UserLimiterState) -> Self {
        input.app.clone()
    }
}

impl FromRef<UserLimiterState> for Arc<UserKeyedLimiter> {
    fn from_ref(input: &UserLimiterState) -> Self {
        input.limiter.clone()
    }
}

/// `from_fn_with_state` handler: rejects with 429 once the authenticated caller exceeds their
/// budget; `CurrentUser`'s 401 applies first.
pub async fn limit_by_user(
    CurrentUser(user): CurrentUser,
    State(limiter): State<Arc<UserKeyedLimiter>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if limiter.check_key(&user.id).is_ok() {
        return Ok(next.run(request).await);
    }
    crate::metrics::incr_user_rejection();
    // Mounted on /api/users/me/password, so a user_id here is a password-probing signal.
    tracing::warn!(
        user_id = user.id, org_id = user.org_id, path = %request.uri().path(),
        "rate limit exceeded: rejecting request by user"
    );
    Err(ApiError::too_many_requests(TOO_MANY_REQUESTS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn zero_hops_always_uses_the_peer_regardless_of_the_header() {
        let peer = ip(127, 0, 0, 1);
        assert_eq!(select_rate_limit_key(peer, Some("203.0.113.1"), 0), peer);
    }

    #[test]
    fn one_hop_uses_the_last_xff_entry() {
        let peer = ip(127, 0, 0, 1);
        let real_client = ip(203, 0, 113, 9);
        let header = "198.51.100.1, 203.0.113.9";
        assert_eq!(select_rate_limit_key(peer, Some(header), 1), real_client);
    }

    #[test]
    fn two_hops_uses_the_second_from_last_xff_entry() {
        let peer = ip(127, 0, 0, 1);
        let real_client = ip(203, 0, 113, 9);
        let header = "10.0.0.1, 203.0.113.9, 198.51.100.1";
        assert_eq!(select_rate_limit_key(peer, Some(header), 2), real_client);
    }

    #[test]
    fn join_xff_header_lines_combines_duplicate_lines_in_wire_order() {
        let mut headers = axum::http::HeaderMap::new();
        headers.append("x-forwarded-for", "203.0.113.1".parse().unwrap());
        headers.append("x-forwarded-for", "203.0.113.77".parse().unwrap());
        assert_eq!(
            join_xff_header_lines(&headers).as_deref(),
            Some("203.0.113.1, 203.0.113.77")
        );
    }

    #[test]
    fn join_xff_header_lines_is_none_when_the_header_is_absent() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(join_xff_header_lines(&headers), None);
    }

    #[test]
    fn duplicate_xff_lines_cant_rotate_the_key_past_the_trusted_final_line() {
        // Two X-Forwarded-For lines: a rotating attacker-controlled one and a constant one from the
        // trusted hop. Joining and taking the last entry keys on the trusted one.
        let peer = ip(127, 0, 0, 1);
        let trusted = ip(203, 0, 113, 77);
        let mut headers = axum::http::HeaderMap::new();
        headers.append("x-forwarded-for", "203.0.113.1".parse().unwrap());
        headers.append("x-forwarded-for", "203.0.113.77".parse().unwrap());
        let joined = join_xff_header_lines(&headers);
        assert_eq!(select_rate_limit_key(peer, joined.as_deref(), 1), trusted);

        let mut headers2 = axum::http::HeaderMap::new();
        headers2.append("x-forwarded-for", "198.51.100.9".parse().unwrap());
        headers2.append("x-forwarded-for", "203.0.113.77".parse().unwrap());
        let joined2 = join_xff_header_lines(&headers2);
        assert_eq!(select_rate_limit_key(peer, joined2.as_deref(), 1), trusted);
    }

    #[test]
    fn missing_header_falls_back_to_the_peer() {
        let peer = ip(127, 0, 0, 1);
        assert_eq!(select_rate_limit_key(peer, None, 1), peer);
    }

    #[test]
    fn malformed_header_falls_back_to_the_peer() {
        let peer = ip(127, 0, 0, 1);
        assert_eq!(
            select_rate_limit_key(peer, Some("not-an-ip, also-not"), 1),
            peer
        );
    }

    #[test]
    fn a_malformed_entry_left_of_the_trusted_position_is_irrelevant() {
        // Left-hand entries are client-controlled; a malformed one must not collapse every request
        // onto one peer bucket.
        let peer = ip(127, 0, 0, 1);
        let trusted = ip(1, 2, 3, 4);
        assert_eq!(
            select_rate_limit_key(peer, Some("garbage, 1.2.3.4"), 1),
            trusted
        );
    }

    #[test]
    fn a_malformed_entry_at_the_trusted_position_falls_back_to_the_peer() {
        let peer = ip(127, 0, 0, 1);
        assert_eq!(
            select_rate_limit_key(peer, Some("1.2.3.4, garbage"), 1),
            peer
        );
    }

    #[test]
    fn two_hops_skips_past_malformed_left_hand_entries() {
        let peer = ip(127, 0, 0, 1);
        let trusted = ip(5, 6, 7, 8);
        assert_eq!(
            select_rate_limit_key(peer, Some("junk, junk, 5.6.7.8, 9.9.9.9"), 2),
            trusted
        );
    }

    #[test]
    fn fewer_entries_than_the_declared_hop_count_falls_back_to_the_peer() {
        // Fail closed: never fall through to the leftmost (client-controlled) entry.
        let peer = ip(127, 0, 0, 1);
        let header = "203.0.113.9";
        assert_eq!(select_rate_limit_key(peer, Some(header), 3), peer);
    }

    #[test]
    fn allows_requests_under_the_quota_then_rejects() {
        let limiter: IpKeyedLimiter =
            RateLimiter::keyed(Quota::per_minute(NonZeroU32::new(2).unwrap()));
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert!(limiter.check_key(&ip).is_ok());
        assert!(limiter.check_key(&ip).is_ok());
        assert!(limiter.check_key(&ip).is_err());
    }

    #[test]
    fn different_ips_have_independent_budgets() {
        let limiter: IpKeyedLimiter =
            RateLimiter::keyed(Quota::per_minute(NonZeroU32::new(1).unwrap()));
        let ip_a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip_b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert!(limiter.check_key(&ip_a).is_ok());
        assert!(limiter.check_key(&ip_a).is_err());
        assert!(limiter.check_key(&ip_b).is_ok());
    }

    #[test]
    fn different_users_have_independent_budgets() {
        let limiter: UserKeyedLimiter =
            RateLimiter::keyed(Quota::per_minute(NonZeroU32::new(1).unwrap()));
        assert!(limiter.check_key(&1i64).is_ok());
        assert!(limiter.check_key(&1i64).is_err());
        assert!(limiter.check_key(&2i64).is_ok());
    }

    // One test to avoid a cross-thread env-var race (the only test touching these vars); a
    // dedicated env var name avoids colliding with other tests.
    #[test]
    fn quota_per_minute_divides_by_replica_count() {
        const ENV_VAR: &str = "TEST_RATE_LIMIT_REPLICA_DIVISION_PER_MIN";
        std::env::remove_var("RATE_LIMIT_REPLICAS");

        std::env::set_var(ENV_VAR, "100");
        assert_eq!(
            quota_per_minute(ENV_VAR, 10),
            Quota::per_minute(NonZeroU32::new(100).unwrap())
        );

        std::env::set_var("RATE_LIMIT_REPLICAS", "4");
        assert_eq!(
            quota_per_minute(ENV_VAR, 10),
            Quota::per_minute(NonZeroU32::new(25).unwrap())
        );

        // A quota below the replica count floors at 1, never 0 (a zero burst would panic or block
        // everything).
        std::env::set_var(ENV_VAR, "3");
        assert_eq!(
            quota_per_minute(ENV_VAR, 10),
            Quota::per_minute(NonZeroU32::new(1).unwrap())
        );

        // A garbage RATE_LIMIT_REPLICAS must fall back to a divisor of 1, never panic.
        std::env::set_var(ENV_VAR, "100");
        std::env::set_var("RATE_LIMIT_REPLICAS", "not-a-number");
        assert_eq!(
            quota_per_minute(ENV_VAR, 10),
            Quota::per_minute(NonZeroU32::new(100).unwrap())
        );

        std::env::remove_var(ENV_VAR);
        std::env::remove_var("RATE_LIMIT_REPLICAS");
    }

    #[test]
    fn trusted_proxy_hops_falls_back_to_zero_on_garbage_rather_than_panicking() {
        std::env::set_var("TRUSTED_PROXY_HOPS", "not-a-number");
        assert_eq!(trusted_proxy_hops(), 0);
        std::env::remove_var("TRUSTED_PROXY_HOPS");
    }

    #[test]
    fn sixth_request_in_a_minute_is_rejected_at_a_five_per_minute_quota() {
        let limiter: IpKeyedLimiter =
            RateLimiter::keyed(Quota::per_minute(NonZeroU32::new(5).unwrap()));
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        for _ in 0..5 {
            assert!(limiter.check_key(&ip).is_ok());
        }
        assert!(limiter.check_key(&ip).is_err());
    }
}
