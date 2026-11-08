//! Stable per-process identity written to `runs.worker_id`, `conveyor_tasks.worker_id`, and the
//! Redis `worker:<id>` heartbeat key, for attribution and heartbeat liveness — not a fencing token.
//!
//! No CAS in this crate compares against it today: `lifecycle::start`'s pending->running claim and
//! the heartbeat's lease renewal both key off task id and status alone, so two processes resolving
//! to the same identity would go undetected by any of them, not "defeat" a guard that doesn't
//! exist. What it does still protect: a stable, collision-resistant value is what makes
//! `runs`/`conveyor_tasks.worker_id` and the heartbeat key trustworthy for showing "who ran this" —
//! two replicas colliding on one identity would misattribute both.

use std::sync::OnceLock;

/// Resolution order, first non-empty wins: `WORKER_ID` (explicit operator override), then
/// `HOSTNAME` (the usual container-runtime value), then `HOST`, then a random per-process
/// fallback. Never the literal `"unknown"` — that literal is what two replicas of the same
/// generic PID-1 entrypoint would both produce, silently colliding on every fence keyed off
/// this value.
///
/// Not cached itself (see [`hostname`] for the memoized, process-stable entry point) — kept
/// separate so a test can call it more than once to exercise the priority order and the random
/// fallback, neither of which a `OnceLock`-memoized function could demonstrate after its first
/// call in a given process.
fn resolve_host_uncached() -> String {
    if let Some(v) = non_empty_env("WORKER_ID").or_else(|| non_empty_env("HOSTNAME")) {
        return v;
    }
    if let Some(v) = non_empty_env("HOST") {
        // Unlike HOSTNAME, HOST is not reliably per-container — a collision here is real and
        // worth flagging on every boot.
        tracing::warn!(
            "neither WORKER_ID nor HOSTNAME is set; falling back to HOST for this process's \
             worker identity — two replicas can end up sharing an identity this way, silently \
             defeating the optimistic-concurrency fences keyed off it. Set HOSTNAME (or \
             WORKER_ID) in the deploy environment."
        );
        return v;
    }
    tracing::info!(
        "neither WORKER_ID, HOSTNAME nor HOST is set; falling back to a random per-process \
         suffix for this process's worker identity — unique to this process but not stable \
         across restarts. Set WORKER_ID or HOSTNAME for a stable identity."
    );
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The resolved host component alone, with no `-{pid}` suffix — for callers (e.g. the worker
/// heartbeat) that report hostname and process id as separate fields.
pub fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(resolve_host_uncached)
}

/// Stable per-process identity written to `runs.worker_id`, `conveyor_tasks.worker_id`, and the
/// Redis `worker:<id>` heartbeat key — see this module's doc comment for what it does and does not
/// guard.
pub fn worker_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("{}-{}", hostname(), std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No other test in this crate touches these three vars, so a plain clear is enough —
    /// matches `lifecycle.rs`/`consumer.rs`'s existing env-var test convention. One test, not
    /// several: `#[test]`s run on parallel threads by default and these mutate process-global
    /// state.
    fn clear_identity_env() {
        std::env::remove_var("WORKER_ID");
        std::env::remove_var("HOSTNAME");
        std::env::remove_var("HOST");
    }

    #[test]
    fn resolve_host_uncached_follows_the_documented_priority_order() {
        clear_identity_env();

        // Nothing set: falls back to a random suffix, never the literal "unknown", and two
        // independent resolutions (standing in for two replica processes that both hit this
        // path) don't collide. Also the common container path — it must not warn on every boot.
        let random_fallback_log = captured_log(|| {
            let a = resolve_host_uncached();
            let b = resolve_host_uncached();
            assert_ne!(a, "unknown");
            assert_ne!(b, "unknown");
            assert_ne!(
                a, b,
                "two unresolvable processes must not collide on the same fallback id"
            );
        });
        assert!(
            !random_fallback_log.contains("WARN"),
            "the random per-process fallback must not warn: {random_fallback_log}"
        );

        // HOST wins once set — and, unlike the random fallback, a genuinely collision-prone path
        // that must warn on every boot.
        std::env::set_var("HOST", "from-host-var");
        let host_fallback_log = captured_log(|| {
            assert_eq!(resolve_host_uncached(), "from-host-var");
        });
        assert!(
            host_fallback_log.contains("WARN"),
            "the HOST fallback must warn: {host_fallback_log}"
        );

        // HOSTNAME beats HOST.
        std::env::set_var("HOSTNAME", "from-hostname");
        assert_eq!(resolve_host_uncached(), "from-hostname");

        // WORKER_ID beats everything.
        std::env::set_var("WORKER_ID", "from-worker-id");
        assert_eq!(resolve_host_uncached(), "from-worker-id");

        clear_identity_env();
    }

    /// `worker_id()` is memoized once per process (by design — see its doc comment), so this
    /// can't re-derive it under controlled env vars the way the test above does; it only
    /// re-asserts the invariant that holds regardless of when/how it was first resolved.
    #[test]
    fn worker_id_never_carries_the_literal_unknown_prefix() {
        assert!(!worker_id().starts_with("unknown-"));
    }

    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured_log(f: impl FnOnce()) -> String {
        let buf = SharedBuf::default();
        let writer_buf = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer_buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        f();
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        text
    }
}
