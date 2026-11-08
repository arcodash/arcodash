//! Valkey list transport — `RPUSH`/`BLPOP` on `arcodash:queue:<task_type>`, keyed by task-id/
//! queue-name strings (not `JobMessage`/`QueueName`, which stay in `arcodash-core` and are not
//! used by this crate). Same key shapes as `crates/shared/src/queue.rs`: `arcodash:queue:<name>`,
//! `job:<id>`.

use anyhow::Result;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};

use crate::envelope::Envelope;
use crate::ids::{TaskId, TaskType};

const QUEUE_KEY_PREFIX: &str = "arcodash:queue";
/// Where a task type with no registered handler lands (`consumer.rs` step 1).
pub const DEAD_LETTER_QUEUE: &str = "arcodash:queue:dead";
pub const JOB_STATUS_TTL_SECS: u64 = 60 * 60;

pub fn queue_key(task_type: &str) -> String {
    format!("{QUEUE_KEY_PREFIX}:{task_type}")
}

fn job_status_key(task_id: TaskId) -> String {
    format!("job:{}", task_id.0)
}

/// Non-blocking key enumeration via cursor-based `SCAN` instead of `KEYS` — `KEYS` blocks the
/// whole Redis event loop (every other client, including every consumer's `BLPOP`) for as long as
/// the full keyspace scan takes; `SCAN` walks it in small batches instead. This crate can't depend
/// on `arcodash-core` (`arcodash-core` depends on it, the opposite direction), so this duplicates
/// that crate's `redis_util::scan_keys` rather than reusing it.
async fn scan_keys(conn: &mut ConnectionManager, pattern: &str) -> Result<Vec<String>> {
    let mut iter: redis::AsyncIter<'_, String> = conn.scan_match(pattern).await?;
    let mut keys = Vec::new();
    while let Some(key) = iter.next_item().await {
        keys.push(key?);
    }
    Ok(keys)
}

/// `RPUSH` an envelope onto its task type's queue — the tail, so `BLPOP` at the head serves in
/// FIFO order.
pub async fn push(valkey: &ConnectionManager, envelope: &Envelope) -> Result<()> {
    let payload = serde_json::to_string(envelope)?;
    push_raw(valkey, &queue_key(&envelope.task_type.0), &payload).await
}

/// `RPUSH` a raw payload onto an arbitrary queue key — used for the dead-letter queue, which
/// carries the original (possibly unroutable-type) envelope verbatim.
pub async fn push_raw(valkey: &ConnectionManager, key: &str, payload: &str) -> Result<()> {
    let mut conn = valkey.clone();
    conn.rpush::<_, _, ()>(key, payload).await?;
    Ok(())
}

/// Stamp `enqueued_at` then push the task's envelope, in that order: a crash between the two still
/// leaves a durable row the sweeps can see, because `resource::over_cap` only counts a pending row
/// once `enqueued_at` is set and `fail_abandoned_pending`'s ceiling only applies to a row that has
/// it. The reverse order would leave a queued task nothing accounts for.
pub async fn stamp_and_push(
    db: &impl ConnectionTrait,
    valkey: &ConnectionManager,
    task_id: TaskId,
    task_type: TaskType,
    payload: serde_json::Value,
    attempt: u32,
) -> Result<()> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE conveyor_tasks SET enqueued_at = COALESCE(enqueued_at, now()) WHERE id = $1",
        [task_id.0.into()],
    ))
    .await?;
    let envelope = Envelope {
        task_id,
        task_type,
        payload,
        attempt,
        enqueued_at: Utc::now(),
        resume: false,
    };
    push(valkey, &envelope).await?;
    set_job_status(valkey, task_id, "pending", None).await?;
    Ok(())
}

/// `BLPOP` one envelope off `key`, blocking up to `timeout_secs`. `Ok(None)` on the timeout
/// elapsing with nothing queued (normal idle poll).
///
/// Generic over the connection type so the consumer's per-task-type loop (`consumer.rs`) can call
/// this against its own dedicated `MultiplexedConnection` — opened specifically so its blocking
/// wait doesn't contend with other commands — while other, single-queue callers can still pass a
/// shared `ConnectionManager` directly.
pub async fn blpop<C>(valkey: &C, key: &str, timeout_secs: f64) -> Result<Option<Envelope>>
where
    C: redis::aio::ConnectionLike + Clone + Send + Sync,
{
    let mut conn = valkey.clone();
    let popped: Option<(String, String)> = conn.blpop(key, timeout_secs).await?;
    Ok(popped
        .map(|(_, raw)| serde_json::from_str(&raw))
        .transpose()?)
}

/// Depth of every `arcodash:queue:*` list except the dead-letter list. Independent of any process's
/// registry — a producer-only caller still sees the true fleet-wide picture.
pub async fn all_queue_depths(valkey: &ConnectionManager) -> Result<Vec<(String, i64)>> {
    let mut conn = valkey.clone();
    let prefix = format!("{QUEUE_KEY_PREFIX}:");
    let keys = scan_keys(&mut conn, &format!("{prefix}*")).await?;
    let mut out = Vec::new();
    for key in keys {
        if key == DEAD_LETTER_QUEUE {
            continue;
        }
        let len: i64 = conn.llen(&key).await?;
        out.push((key.strip_prefix(&prefix).unwrap_or(&key).to_string(), len));
    }
    Ok(out)
}

/// Live worker heartbeats, read from the `worker:*` keys `apps/worker/src/heartbeat.rs` writes.
/// Tolerant: a stale/garbage value is skipped (logged), never fails the snapshot. Returns
/// (`worker_id`, `last_seen`).
pub async fn worker_heartbeats(valkey: &ConnectionManager) -> Result<Vec<(String, DateTime<Utc>)>> {
    #[derive(serde::Deserialize)]
    struct Raw {
        id: String,
        last_heartbeat: String,
    }
    let mut conn = valkey.clone();
    let keys = scan_keys(&mut conn, "worker:*").await?;
    let mut out = Vec::new();
    for key in keys {
        let Ok(Some(raw)) = conn.get::<_, Option<String>>(&key).await else {
            continue;
        };
        if let Some(hb) = serde_json::from_str::<Raw>(&raw).ok().and_then(|r| {
            DateTime::parse_from_rfc3339(&r.last_heartbeat)
                .ok()
                .map(|t| (r.id, t.with_timezone(&Utc)))
        }) {
            out.push(hb);
        } else {
            tracing::warn!(%key, "skipping unparseable worker:* key");
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatusRecord {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub async fn set_job_status(
    valkey: &ConnectionManager,
    task_id: TaskId,
    status: &str,
    error: Option<String>,
) -> Result<()> {
    let record = JobStatusRecord {
        status: status.to_string(),
        error,
        updated_at: Utc::now(),
    };
    let payload = serde_json::to_string(&record)?;
    let mut conn = valkey.clone();
    conn.set_ex::<_, _, ()>(job_status_key(task_id), payload, JOB_STATUS_TTL_SECS)
        .await?;
    Ok(())
}

pub async fn get_job_status(
    valkey: &ConnectionManager,
    task_id: TaskId,
) -> Result<Option<JobStatusRecord>> {
    let mut conn = valkey.clone();
    let raw: Option<String> = conn.get(job_status_key(task_id)).await?;
    Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
}

#[cfg(test)]
mod tests {
    //! Real Valkey only (skips cleanly without `TEST_REDIS_URL`).
    use super::*;

    async fn setup() -> Option<ConnectionManager> {
        let redis_url = std::env::var("TEST_REDIS_URL").ok()?;
        let client = redis::Client::open(redis_url).unwrap();
        Some(client.get_connection_manager().await.unwrap())
    }

    fn envelope(id: i64, task_type: &str, payload: serde_json::Value) -> Envelope {
        Envelope {
            task_id: TaskId(id),
            task_type: task_type.into(),
            payload,
            attempt: 1,
            enqueued_at: Utc::now(),
            resume: false,
        }
    }

    #[tokio::test]
    async fn push_and_blpop_round_trip() {
        let Some(valkey) = setup().await else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let task_type = format!("toy-{}", uuid::Uuid::new_v4());
        let envelope = envelope(42, &task_type, serde_json::json!({"x": 1}));
        push(&valkey, &envelope).await.unwrap();

        let popped = blpop(&valkey, &queue_key(&task_type), 2.0)
            .await
            .unwrap()
            .expect("envelope must be present");
        assert_eq!(popped.task_id, envelope.task_id);
        assert_eq!(popped.payload, envelope.payload);
    }

    /// Push order must be serve order: `RPUSH` writes the tail, `BLPOP` reads the head.
    #[tokio::test]
    async fn push_then_blpop_is_fifo() {
        let Some(valkey) = setup().await else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let task_type = format!("toy-fifo-{}", uuid::Uuid::new_v4());
        for id in [1, 2, 3] {
            push(&valkey, &envelope(id, &task_type, serde_json::json!({})))
                .await
                .unwrap();
        }

        let key = queue_key(&task_type);
        for expected in [1, 2, 3] {
            let popped = blpop(&valkey, &key, 2.0)
                .await
                .unwrap()
                .expect("envelope must be present");
            assert_eq!(popped.task_id, TaskId(expected));
        }
    }

    /// `scan_keys` must find every key matching the pattern (via non-blocking `SCAN`, not `KEYS`)
    /// and none that don't.
    #[tokio::test]
    async fn scan_keys_finds_matching_keys_only() {
        let Some(valkey) = setup().await else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let mut conn = valkey.clone();
        let prefix = format!("test:scan-keys:{}", uuid::Uuid::new_v4());
        let matching = format!("{prefix}:a");
        let other = format!("{prefix}-unrelated");
        conn.set::<_, _, ()>(&matching, "1").await.unwrap();
        conn.set::<_, _, ()>(&other, "1").await.unwrap();

        let found = scan_keys(&mut conn, &format!("{prefix}:*")).await.unwrap();
        assert_eq!(found, vec![matching.clone()]);

        let _: () = conn.del(&matching).await.unwrap();
        let _: () = conn.del(&other).await.unwrap();
    }

    /// Exercises `all_queue_depths`'s SCAN-based key enumeration end to end: a pushed envelope's
    /// queue must show up with the right depth, and the dead-letter queue must never appear.
    #[tokio::test]
    async fn all_queue_depths_reflects_a_pushed_envelope_and_excludes_the_dead_letter_queue() {
        let Some(valkey) = setup().await else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let task_type = format!("toy-depths-{}", uuid::Uuid::new_v4());
        push(&valkey, &envelope(1, &task_type, serde_json::json!({})))
            .await
            .unwrap();
        push_raw(&valkey, DEAD_LETTER_QUEUE, "irrelevant-payload")
            .await
            .unwrap();

        let depths = all_queue_depths(&valkey).await.unwrap();
        assert_eq!(
            depths
                .iter()
                .find(|(t, _)| *t == task_type)
                .map(|(_, d)| *d),
            Some(1)
        );
        assert!(
            depths.iter().all(|(t, _)| t != "dead"),
            "the dead-letter queue must never appear in the depth listing"
        );

        let mut conn = valkey.clone();
        let _: () = conn.del(queue_key(&task_type)).await.unwrap();
        let _: () = conn.del(DEAD_LETTER_QUEUE).await.unwrap();
    }

    #[tokio::test]
    async fn job_status_set_get_and_ttl() {
        let Some(valkey) = setup().await else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let task_id = TaskId(rand_id());
        assert!(get_job_status(&valkey, task_id).await.unwrap().is_none());

        set_job_status(&valkey, task_id, "running", None)
            .await
            .unwrap();
        let status = get_job_status(&valkey, task_id).await.unwrap().unwrap();
        assert_eq!(status.status, "running");

        let mut conn = valkey.clone();
        let ttl: i64 = conn.ttl(job_status_key(task_id)).await.unwrap();
        assert!(ttl > 0 && ttl <= JOB_STATUS_TTL_SECS as i64);
    }

    fn rand_id() -> i64 {
        (uuid::Uuid::new_v4().as_u128() % (i64::MAX as u128)) as i64
    }
}
