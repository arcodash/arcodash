//! Small Redis helpers shared by any caller that lists keys or reads worker liveness state —
//! kept in one place so `apps/backend`, `apps/worker`, and any other reader can't drift apart
//! on either.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

/// One worker process's liveness heartbeat.
///
/// Written by `apps/worker/src/heartbeat.rs` to a
/// `worker:<id>` key (`SET`+`EX`, see that module's doc comment for the key format/TTL) and read by
/// both `apps/backend`'s org-scoped `/api/status` (count only) and any other worker listing (full
/// identity).
///
/// `#[serde(default)]` on every field: a writer-side field addition must degrade a reader to a
/// zeroed/empty value for that field rather than dropping the whole key and reading as fleet-dead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub concurrency: usize,
    #[serde(default)]
    pub last_heartbeat: String,
}

/// Non-blocking key enumeration via cursor-based `SCAN` instead of `KEYS`.
///
/// `KEYS` blocks the whole
/// Redis event loop (every other client, including the worker's own job dequeue) for as long as the
/// full keyspace scan takes; `SCAN` walks it in small batches instead.
///
/// Callers should only use this on already-bounded key patterns (TTL'd job/worker keys), since this
/// is about not blocking the shared Redis instance while scanning, not about result size.
pub async fn scan_keys(
    conn: &mut ConnectionManager,
    pattern: &str,
) -> redis::RedisResult<Vec<String>> {
    let mut iter: redis::AsyncIter<'_, String> = conn.scan_match(pattern).await?;
    let mut keys = Vec::new();
    while let Some(key) = iter.next_item().await {
        keys.push(key?);
    }
    Ok(keys)
}

/// Pub/sub channel the worker's deadline loop listens on, so schedule/run writes take effect
/// without waiting for its poll cap.
pub const SCHED_WAKE_CHANNEL: &str = "arcodash:sched:wake";

/// Wakes the deadline loop. Callers treat a failure as best-effort and log it with their own
/// fields; a lost publish only delays the loop until its next poll.
pub async fn publish_sched_wake(redis: &ConnectionManager) -> redis::RedisResult<()> {
    let mut conn = redis.clone();
    conn.publish::<_, _, i64>(SCHED_WAKE_CHANNEL, "1")
        .await
        .map(|_| ())
}
