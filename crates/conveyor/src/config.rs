//! Env-read constants for the conveyor crate.
//!
//! This crate can't reuse `arcodash_core::config::env_parsed` for this — `arcodash-core` depends
//! on `arcodash-conveyor`, so importing back from core would be a dependency cycle. Same behavior,
//! duplicated on purpose: unset falls back to `default` silently; present-but-unparseable also
//! falls back to `default`, but logs loudly (a typo'd wedge-recovery timer failing open to its
//! default is far less costly than this crate refusing to boot over it).
fn env_parsed<T: std::str::FromStr>(key: &str, default: T) -> T
where
    T::Err: std::fmt::Display,
{
    let Ok(raw) = std::env::var(key) else {
        return default;
    };
    raw.trim().parse().unwrap_or_else(|e| {
        tracing::error!(key, raw, error = %e, "invalid env var value, using default");
        default
    })
}

/// How long a claimed (`running`) task's lease lasts before the sweeper reclaims it.
pub fn lease_secs() -> i64 {
    env_parsed("CONVEYOR_LEASE_SECS", 90)
}

/// How often the scheduler loop runs its sweep pass.
pub fn sweep_interval_secs() -> u64 {
    env_parsed("CONVEYOR_SWEEP_INTERVAL_SECS", 60)
}

/// How old a `pending` task must be before the sweeper considers it stale (never enqueued, or
/// parked over a resource cap).
pub fn stale_pending_secs() -> i64 {
    env_parsed("CONVEYOR_STALE_PENDING_SECS", 60)
}

/// Consecutive schedule failures before a schedule auto-pauses.
pub fn auto_pause_threshold() -> i32 {
    env_parsed("CONVEYOR_AUTO_PAUSE_THRESHOLD", 10)
}

/// The concurrency cap applied when a caller sets `resource_key` but no explicit `resource_cap`.
pub fn default_resource_cap() -> i64 {
    env_parsed("CONVEYOR_DEFAULT_RESOURCE_CAP", 4)
}

/// Ceiling on the deadline loop's sleep between ticks (also its fallback when the pub/sub wake
/// subscription is unavailable).
pub fn max_sleep_secs() -> u64 {
    env_parsed("CONVEYOR_MAX_SLEEP_SECS", 120)
}

/// Max due schedules claimed per deadline tick.
pub fn claim_limit() -> i64 {
    env_parsed("CONVEYOR_CLAIM_LIMIT", 100)
}

/// Max rows any one sweep query processes per pass — bounds a single tick's scan against a large
/// stale-task backlog instead of an unbounded `SELECT`; whatever doesn't fit is picked up by the
/// next pass. `sweep_expired_leases`/`fail_abandoned_pending` terminalize every row they match, so
/// a persistent backlog always shrinks pass over pass regardless of this limit.
/// `requeue_stale_pending` doesn't: a `pending` row it re-pushes stays `pending`, so it keeps
/// matching every later pass until something else (a claim, or `fail_abandoned_pending`'s ceiling
/// once it has an `enqueued_at`) moves it out of that status — a sustained backlog past this limit
/// could in principle starve out newer stale rows in `id` order forever. Sized generously to make
/// that impractical rather than solved (would need ordering by least-recently-swept, not `id`).
pub fn sweep_batch_limit() -> i64 {
    env_parsed("CONVEYOR_SWEEP_BATCH_LIMIT", 2000)
}

/// How many days of terminal tasks the prune pass keeps before deleting.
pub fn task_retention_days() -> i64 {
    env_parsed("CONVEYOR_TASK_RETENTION_DAYS", 14)
}

/// Rows deleted per `DELETE` statement in the prune pass — bounds how long any one statement holds
/// its lock, at the cost of needing several round trips to clear a large backlog.
pub fn prune_batch_limit() -> i64 {
    env_parsed("CONVEYOR_PRUNE_BATCH_LIMIT", 5000)
}

/// Ceiling on rows the prune pass deletes in one leader-loop cycle — the residual backlog is picked
/// up next cycle rather than blocking scheduling for an unbounded number of chunks in one go.
pub fn prune_max_per_cycle() -> i64 {
    env_parsed("CONVEYOR_PRUNE_MAX_PER_CYCLE", 100_000)
}

/// How old an enqueued-but-never-claimed `pending` task must be before the sweeper gives up on it
/// and finalizes it as `timed_out`. Must exceed the longest legitimate wait between a push and a
/// worker claiming it (bounded by tier concurrency, not the resource cap) — a deployment running a
/// persistently backlogged tier should raise this alongside it.
pub fn abandoned_pending_secs() -> i64 {
    env_parsed("CONVEYOR_ABANDONED_PENDING_SECS", 3600)
}

/// Hard ceiling on one handler invocation, independent of any per-task-type timeout the handler
/// enforces itself — this is what makes `TaskState::TimedOut` reachable for a handler that simply
/// hangs, since nothing else bounds `handler.run`'s own await. A deployment raising a handler's own
/// timeout well past this must raise this alongside it.
pub fn task_timeout_secs() -> u64 {
    env_parsed("CONVEYOR_TASK_TIMEOUT_SECS", 21_600)
}

/// Max rows `Conveyor::live()` returns per running/stuck-pending category — it's a diagnostic
/// snapshot, not an inventory query, and must never scale with the size of the table.
pub fn live_snapshot_limit() -> u64 {
    env_parsed("CONVEYOR_LIVE_SNAPSHOT_LIMIT", 500)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars are process-global — guards every test in this module against interleaving with
    /// another test (in this file or elsewhere in the binary) that reads the same var while it's
    /// set to a deliberately-invalid value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn malformed_value_logs_and_falls_back_to_the_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("CONVEYOR_LEASE_SECS", "not-a-number");
        assert_eq!(
            lease_secs(),
            90,
            "a malformed value must fall back to the default, not panic or propagate"
        );
        std::env::remove_var("CONVEYOR_LEASE_SECS");
    }

    #[test]
    fn unset_uses_the_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("CONVEYOR_LEASE_SECS");
        assert_eq!(lease_secs(), 90);
    }
}
