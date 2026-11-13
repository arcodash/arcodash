//! Egress guard against SSRF: [`guard_host`] resolves a data-source/webhook hostname and rejects it
//! if any address is loopback/private/link-local/metadata/CGNAT/ULA/broadcast.
//!
//! Connectors and webhook-style notification destinations call it before connecting.
//!
//! Bypassed when `ARCODASH_ALLOW_PRIVATE_CONNECTORS` is truthy (local dev/self-host). It returns
//! the approved addresses so reqwest callers can pin their connection to them; native drivers take
//! a hostname, can't be pinned, and keep a documented residual gap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::Semaphore;

/// Env var that disables this guard entirely (dev/self-host opt-out).
pub const ALLOW_PRIVATE_ENV: &str = "ARCODASH_ALLOW_PRIVATE_CONNECTORS";

/// Latches true when [`guard_host`] takes the bypass path, so a misconfigured production deploy
/// shows up loudly in logs.
static BYPASS_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_guard_disabled_once() {
    if !BYPASS_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            env = ALLOW_PRIVATE_ENV,
            "SSRF egress guard is DISABLED by an operator override — every connector/webhook \
             destination is now unchecked against private/internal addresses. Expected only in \
             local dev/self-host; unset {ALLOW_PRIVATE_ENV} in any deployment reachable from \
             untrusted input."
        );
    }
}

/// Bound on the blocking `getaddrinfo` call in [`guard_host`]; matches the Postgres/MySQL connect
/// timeout.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on guard resolutions occupying the blocking pool: `timeout` can't abort a running
/// `getaddrinfo`, so a black-holed resolver would otherwise pin a thread per connect.
const MAX_CONCURRENT_RESOLVES: usize = 16;
static RESOLVE_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_RESOLVES);

/// IPv4 CGNAT range, RFC 6598: 100.64.0.0/10.
const fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000
}

/// IPv6 Unique Local Address range, RFC 4193: `fc00::/7`.
const fn is_unique_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// IPv6 link-local range: `fe80::/10` (covers link-local metadata-style endpoints on v6).
const fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// RFC 1122 "this network" (0.0.0.0/8); several stacks route the whole block to localhost.
const fn is_this_network(ip: Ipv4Addr) -> bool {
    ip.octets()[0] == 0
}

/// RFC 6890 IETF protocol assignments (192.0.0.0/24, includes the DS-Lite range).
const fn is_protocol_assignment(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 192 && o[1] == 0 && o[2] == 0
}

/// RFC 1112 reserved (240.0.0.0/4); routing behavior is stack-dependent, never safely public.
const fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    ip.octets()[0] >= 240
}

/// RFC 6052 NAT64 well-known prefix (`64:ff9b::/96`): translates to an embedded IPv4, including the
/// metadata IP. The whole prefix is blocked rather than decoded.
const fn is_nat64_wellknown(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0x0064 && s[1] == 0xff9b
}

/// RFC 3056 6to4 (`2002::/16`): embeds an IPv4 address, including the metadata IP; blocked whole,
/// like NAT64.
const fn is_6to4(ip: Ipv6Addr) -> bool {
    ip.segments()[0] == 0x2002
}

/// RFC 3879 deprecated site-local (`fec0::/10`) — still routed by some stacks.
const fn is_site_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfec0
}

/// Whether `ip` is in a range outbound connections must not reach (loopback, private, link-local,
/// metadata, CGNAT, ULA, multicast, reserved, and IPv4-embedding IPv6 forms).
#[must_use]
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || is_cgnat(v4)
                || is_this_network(v4)
                || is_protocol_assignment(v4)
                || is_reserved_v4(v4)
        }
        IpAddr::V6(v6) => {
            // Native V6 ranges first: `to_ipv4()` also matches `::1`/`::`, which the V4 rules
            // wouldn't flag.
            if v6.is_loopback()
                || v6.is_unspecified()
                || is_unique_local_v6(v6)
                || is_link_local_v6(v6)
                || is_nat64_wellknown(v6)
                || is_6to4(v6)
                || is_site_local_v6(v6)
                || v6.is_multicast()
            {
                return true;
            }
            // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible forms are missed by the V6
            // predicates but routed to the IPv4 (metadata!), so re-check via the V4 rules.
            v6.to_ipv4().is_some_and(|v4| is_blocked(IpAddr::V4(v4)))
        }
    }
}

/// Resolve `host` and reject if any resolved address is in a blocked range. No-ops (returns
/// `Ok(vec![])`) when `ARCODASH_ALLOW_PRIVATE_CONNECTORS` is truthy.
///
/// Returns every address that passed, so a reqwest caller can pin its client to them and close the
/// DNS-rebind gap. Callers that can't pin (native drivers) keep a residual TOCTOU gap: the
/// connector re-resolves the hostname later, so a record flipping to a private IP slips past.
pub async fn guard_host(host: &str) -> Result<Vec<SocketAddr>> {
    guard_host_with(host, system_resolve).await
}

/// Blocking system (`getaddrinfo`) resolution of `host`; IP literals parse without a lookup.
pub fn system_resolve(host: &str) -> std::io::Result<Vec<SocketAddr>> {
    (host, 0)
        .to_socket_addrs()
        .map(std::iter::Iterator::collect)
}

/// [`guard_host`] with an injectable resolver on the blocking pool, so tests can check verdicts
/// without real DNS.
pub async fn guard_host_with<R>(host: &str, resolve: R) -> Result<Vec<SocketAddr>>
where
    R: FnOnce(&str) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
{
    // An unparseable value must leave the guard on — fail closed, never open.
    if crate::config::parse_bool_env(ALLOW_PRIVATE_ENV, false).unwrap_or(false) {
        warn_guard_disabled_once();
        return Ok(vec![]);
    }

    // `ToSocketAddrs` is a blocking `getaddrinfo` with no timeout, so run it on the blocking pool,
    // bounded. The permit is acquired inside the timeout and moved into the closure so it's held
    // until `getaddrinfo` returns; releasing early would let timed-out callers pile onto stuck
    // threads.
    let owned = host.to_string();
    let addrs: Vec<SocketAddr> = tokio::time::timeout(RESOLVE_TIMEOUT, async {
        let permit = RESOLVE_SLOTS
            .acquire()
            .await
            .expect("RESOLVE_SLOTS is never closed");
        tokio::task::spawn_blocking(move || {
            let result = resolve(owned.as_str());
            drop(permit);
            result
        })
        .await
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out resolving host '{host}'"))?
    .context("DNS resolution task panicked")?
    .map_err(|e| anyhow::anyhow!("failed to resolve host '{host}': {e}"))?;

    if let Some(ip) = addrs.iter().map(SocketAddr::ip).find(|ip| is_blocked(*ip)) {
        crate::metrics::record_ssrf_guard_rejection();
        tracing::warn!(host, %ip, "blocked connection to a private/internal address");
        // Don't name the bypass env var: this message reaches end users, and it would hand a
        // prober the knob to ask an operator to flip.
        bail!(
            "connection to '{host}' ({ip}) is blocked: private/internal network addresses \
             are not allowed"
        );
    }
    if addrs.is_empty() {
        bail!("host '{host}' did not resolve to any address");
    }
    tracing::debug!(
        host,
        approved = addrs.len(),
        "resolved host cleared the SSRF guard"
    );
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_internal_ip_literals() {
        let _env = crate::test_support::private_net_env(None);
        for ip in [
            "169.254.169.254",
            "127.0.0.1",
            "10.0.0.1",
            "::1",
            "100.64.0.1",
            "fd00::1",
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(guard_host(ip).await.is_err(), "{ip} must be blocked");
        }
    }

    fn resolving_to(
        ips: &[&str],
    ) -> impl FnOnce(&str) -> std::io::Result<Vec<SocketAddr>> + Send + 'static {
        let addrs: Vec<SocketAddr> = ips
            .iter()
            .map(|ip| SocketAddr::new(ip.parse().unwrap(), 0))
            .collect();
        move |_| Ok(addrs)
    }

    #[tokio::test]
    async fn hostname_resolving_public_is_allowed_and_returned() {
        let _env = crate::test_support::private_net_env(None);
        let addrs = guard_host_with(
            "api.example.com",
            resolving_to(&["1.1.1.1", "2606:4700::1111"]),
        )
        .await
        .unwrap();
        assert_eq!(addrs.len(), 2);
    }

    /// The real SSRF case: a public-looking name whose DNS answer is internal.
    #[tokio::test]
    async fn hostname_resolving_to_internal_is_blocked() {
        let _env = crate::test_support::private_net_env(None);
        for ip in ["169.254.169.254", "127.0.0.1", "10.0.0.5", "::1", "fd00::1"] {
            assert!(
                guard_host_with("evil.example.com", resolving_to(&[ip]))
                    .await
                    .is_err(),
                "{ip} must be blocked"
            );
        }
    }

    #[tokio::test]
    async fn hostname_resolving_to_public_and_private_mix_is_blocked() {
        let _env = crate::test_support::private_net_env(None);
        for order in [["1.1.1.1", "10.0.0.5"], ["10.0.0.5", "1.1.1.1"]] {
            assert!(guard_host_with("mixed.example.com", resolving_to(&order))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn resolver_error_and_empty_answer_fail_closed() {
        let _env = crate::test_support::private_net_env(None);
        let err =
            guard_host_with("nx.example.com", |_| Err(std::io::Error::other("NXDOMAIN"))).await;
        assert!(err.is_err());
        assert!(guard_host_with("empty.example.com", resolving_to(&[]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn allows_public_address() {
        let _env = crate::test_support::private_net_env(None);
        // An IP literal keeps this deterministic/offline (no DNS lookup).
        assert!(guard_host("8.8.8.8").await.is_ok());
    }

    /// A pinning caller needs the resolved address itself, not just a verdict; this closes the
    /// DNS-rebind gap on `guard_host`.
    #[tokio::test]
    async fn returns_the_resolved_address_it_approved() {
        let _env = crate::test_support::private_net_env(None);
        let addrs = guard_host("8.8.8.8")
            .await
            .expect("public IP literal is allowed");
        assert_eq!(addrs, vec![SocketAddr::from(([8, 8, 8, 8], 0))]);
    }

    #[tokio::test]
    async fn opt_out_env_allows_what_is_otherwise_blocked() {
        let _env = crate::test_support::private_net_env(None);
        std::env::remove_var(ALLOW_PRIVATE_ENV);
        assert!(guard_host("127.0.0.1").await.is_err());
        BYPASS_WARNED.store(false, Ordering::Relaxed);
        std::env::set_var(ALLOW_PRIVATE_ENV, "1");
        assert!(guard_host("127.0.0.1").await.is_ok());
        assert!(
            BYPASS_WARNED.load(Ordering::Relaxed),
            "bypassing the guard must latch a loud one-time warning, not disable silently"
        );

        std::env::set_var(ALLOW_PRIVATE_ENV, "true");
        assert!(guard_host("127.0.0.1").await.is_ok());

        // An unparseable value must leave the guard ON, never off — fail closed.
        std::env::set_var(ALLOW_PRIVATE_ENV, "maybe");
        assert!(guard_host("127.0.0.1").await.is_err());

        std::env::remove_var(ALLOW_PRIVATE_ENV);
    }

    /// The opt-out fast path must never touch the blocking pool (asserted explicitly, since a
    /// resolve-then-discard regression would still return Ok).
    #[tokio::test]
    async fn opt_out_env_returns_empty_without_resolving() {
        let _env = crate::test_support::private_net_env(None);
        std::env::set_var(ALLOW_PRIVATE_ENV, "1");
        let before = RESOLVE_SLOTS.available_permits();
        let addrs = guard_host("this-host-name-is-never-resolved.invalid")
            .await
            .expect("opt-out short-circuits before resolution");
        assert!(addrs.is_empty());
        assert_eq!(
            RESOLVE_SLOTS.available_permits(),
            before,
            "the opt-out path must return before ever touching the resolve semaphore"
        );
        std::env::remove_var(ALLOW_PRIVATE_ENV);
    }

    /// An ordinary failure stays a resolution error, not a timeout. `something.invalid` is reserved
    /// (RFC 2606) but still hits the real resolver, so skip if resolution itself times out.
    #[tokio::test]
    async fn unresolvable_host_returns_resolution_error_not_timeout() {
        let _env = crate::test_support::private_net_env(None);
        std::env::remove_var(ALLOW_PRIVATE_ENV);
        match guard_host("something.invalid").await {
            Err(e) => assert!(
                !e.to_string().contains("timed out"),
                "expected a resolution error, got a timeout: {e}"
            ),
            Ok(_) => panic!("something.invalid must not resolve"),
        }
    }

    /// Proves the timeout wrapper bounds a slow lookup, not just the constant's value; needs a
    /// genuinely blocking resolver, so it's opt-in.
    #[tokio::test]
    async fn resolve_timeout_actually_bounds_a_slow_lookup() {
        let _env = crate::test_support::private_net_env(None);
        if std::env::var("ARCODASH_TEST_SLOW_DNS").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set ARCODASH_TEST_SLOW_DNS=1 against a host known to resolve slower \
                 than RESOLVE_TIMEOUT to exercise this"
            );
            return;
        }
        std::env::remove_var(ALLOW_PRIVATE_ENV);
        let start = std::time::Instant::now();
        let result = guard_host("this-domain-must-resolve-slower-than-the-timeout.invalid").await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < RESOLVE_TIMEOUT + Duration::from_secs(5),
            "took {elapsed:?}, expected the wrapper to cut this off near {RESOLVE_TIMEOUT:?}"
        );
        match result {
            Err(e) => assert!(
                e.to_string().contains("timed out"),
                "expected a timeout error, got: {e}"
            ),
            Ok(_) => panic!("expected the deliberately slow lookup to time out"),
        }
    }

    /// Proves the semaphore bounds concurrency and never leaks a permit: a burst past
    /// `MAX_CONCURRENT_RESOLVES` all succeed and all permits return.
    #[tokio::test]
    async fn resolve_slots_are_bounded_and_fully_released_under_a_burst() {
        let _env = crate::test_support::private_net_env(None);
        std::env::remove_var(ALLOW_PRIVATE_ENV);
        let handles: Vec<_> = (0..MAX_CONCURRENT_RESOLVES * 4)
            .map(|_| tokio::spawn(guard_host("8.8.8.8")))
            .collect();
        for h in handles {
            h.await
                .expect("task panicked")
                .expect("IP literal must always resolve/pass the guard");
        }
        assert_eq!(
            RESOLVE_SLOTS.available_permits(),
            MAX_CONCURRENT_RESOLVES,
            "every permit must be returned once all resolutions complete"
        );
    }

    /// A blocked-connection error must not name the env var that disables the guard.
    #[tokio::test]
    async fn blocked_error_does_not_hint_at_the_bypass_env_var() {
        let _env = crate::test_support::private_net_env(None);
        let err = guard_host("127.0.0.1").await.unwrap_err().to_string();
        assert!(
            !err.contains(ALLOW_PRIVATE_ENV),
            "error message must not mention the bypass env var: {err}"
        );
    }

    /// Table-driven sweep over the ranges [`is_blocked`] special-cases, plus allowed addresses that
    /// must NOT become blocked.
    #[test]
    fn is_blocked_table() {
        let blocked = [
            "0.0.0.1",
            "0.255.255.255",
            "192.0.0.170",
            "240.0.0.1",
            "255.255.255.254",
            "224.0.0.1",
            "239.1.2.3",
            "64:ff9b::169.254.169.254",
            "64:ff9b::8.8.8.8",
            "ff02::1",
            "2002:a9fe:a9fe::",
            "2002:0a00:0001::",
            "fec0::1",
        ];
        for ip in blocked {
            let addr: IpAddr = ip.parse().expect("valid IP literal");
            assert!(is_blocked(addr), "{ip} must be blocked");
        }

        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ];
        for ip in allowed {
            let addr: IpAddr = ip.parse().expect("valid IP literal");
            assert!(!is_blocked(addr), "{ip} must NOT be blocked (regression)");
        }
    }
}
