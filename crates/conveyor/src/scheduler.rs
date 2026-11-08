//! The deadline loop: sleep until the next due schedule (or a pub/sub wake, or the sleep cap),
//! claim due schedules under `FOR UPDATE SKIP LOCKED`, plan one task row per tick (applying
//! `overlap_policy`), then enqueue subject to each task's resource cap.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use redis::aio::{ConnectionManager, PubSubStream};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait, Statement,
    TransactionTrait,
};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::config::{self, claim_limit, max_sleep_secs, sweep_interval_secs};
use crate::entities::{schedule as schedule_entity, task as task_entity};
use crate::ids::TaskId;
use crate::queue;
use crate::reporting;
use crate::resource;
use crate::schedule_spec::{compute_next, ScheduleSpec};
use crate::sweeper;

/// Distinct from the host application's scheduler leader lock so the two never contend.
const LEADER_LOCK_KEY: &str = "arcodash:conveyor:scheduler:leader";
/// Channel [`lifecycle::finalize`] publishes to on every terminal transition, so this loop wakes
/// promptly instead of waiting out `max_sleep_secs`.
pub const WAKE_CHANNEL: &str = "arcodash:conveyor:sched:wake";
/// Cadence for the hourly `task_stats_hourly` rollup + terminal-task prune (both idempotent).
const MAINTENANCE_INTERVAL_SECS: u64 = 3600;
/// How often the loop retries `subscribe_wake` after losing the pub/sub connection.
const WAKE_RESUBSCRIBE_INTERVAL: Duration = Duration::from_secs(30);

/// Leader-lock acquisitions that lost to another replica (normal contention, not a failure).
pub static LEADER_ACQUIRE_CONTENDED: AtomicU64 = AtomicU64::new(0);
/// Leader-lock acquisitions that failed for another reason (Redis error, script failure); exposed
/// as `arcodash_leader_acquire_failed_total`.
pub static LEADER_ACQUIRE_FAILED: AtomicU64 = AtomicU64::new(0);

pub async fn run(
    db: DatabaseConnection,
    valkey: ConnectionManager,
    redis_url: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let lock_id = Uuid::new_v4().to_string();
    let mut wake = subscribe_wake(&redis_url).await;
    if wake.is_none() {
        tracing::warn!("conveyor scheduler: pub/sub wake subscription unavailable, falling back to the sleep cap");
    }
    let mut last_wake_attempt = Instant::now();
    let mut last_sweep = Instant::now()
        .checked_sub(Duration::from_secs(sweep_interval_secs()))
        .unwrap();
    let mut last_maintenance = Instant::now()
        .checked_sub(Duration::from_secs(MAINTENANCE_INTERVAL_SECS))
        .unwrap();

    loop {
        if *shutdown.borrow() {
            break;
        }

        if wake.is_none() && last_wake_attempt.elapsed() >= WAKE_RESUBSCRIBE_INTERVAL {
            last_wake_attempt = Instant::now();
            wake = subscribe_wake(&redis_url).await;
        }

        match acquire_leader_lock(&valkey, LEADER_LOCK_KEY, &lock_id).await {
            LeaderLockOutcome::Acquired => {}
            outcome @ (LeaderLockOutcome::Contended | LeaderLockOutcome::Error) => {
                let counter = if outcome == LeaderLockOutcome::Contended {
                    &LEADER_ACQUIRE_CONTENDED
                } else {
                    &LEADER_ACQUIRE_FAILED
                };
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            }
        }

        let now = Utc::now();
        let planned: Vec<task_entity::Model> = claim_and_plan(&db, now).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "conveyor scheduler: claim_and_plan failed this tick");
            Vec::new()
        });

        for t in &planned {
            if let Err(e) = try_enqueue(&db, &valkey, t).await {
                tracing::warn!(task_id = t.id, error = %e, "conveyor scheduler: enqueue failed (sweeper will retry)");
            }
        }

        if last_sweep.elapsed() >= Duration::from_secs(sweep_interval_secs()) {
            sweeper::sweep_expired_leases(&db, &valkey).await;
            sweeper::fail_abandoned_pending(&db, &valkey).await;
            sweeper::requeue_stale_pending(&db, &valkey).await;
            sweeper::fan_out_orphaned_waiters(&db, &valkey).await;
            last_sweep = Instant::now();
        }

        if last_maintenance.elapsed() >= Duration::from_secs(MAINTENANCE_INTERVAL_SECS) {
            if let Err(e) = reporting::rollup_recent_hours(&db, &valkey).await {
                tracing::error!(error = %e, "conveyor: hourly task_stats rollup failed this cycle");
            }
            match reporting::prune_old_tasks(&db, config::task_retention_days()).await {
                Ok(n) if n > 0 => {
                    tracing::info!(deleted = n, "conveyor: pruned aged terminal tasks");
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "conveyor: task prune failed this cycle"),
            }
            last_maintenance = Instant::now();
        }

        // Re-sample the clock so slow claim/sweep work doesn't delay every tick.
        let now = Utc::now();
        let next = min_next_run_at(&db, now).await;
        let dur = sleep_until(next, now);

        tokio::select! {
            () = tokio::time::sleep(dur) => {}
            () = recv_wake(&mut wake) => {}
            _ = shutdown.changed() => {}
        }
    }

    release_leader_lock(&valkey, LEADER_LOCK_KEY, &lock_id).await;
}

/// Opens a dedicated pub/sub connection (`ConnectionManager` can't hold a blocking `SUBSCRIBE`).
/// `None` means the subscription failed or died; the loop falls back to the sleep cap and retries
/// on `WAKE_RESUBSCRIBE_INTERVAL`.
async fn subscribe_wake(redis_url: &str) -> Option<PubSubStream> {
    let client = redis::Client::open(redis_url).ok()?;
    let mut pubsub = client.get_async_pubsub().await.ok()?;
    pubsub.subscribe(WAKE_CHANNEL).await.ok()?;
    Some(pubsub.into_on_message())
}

/// Awaits the next wake message, or pends forever with no subscription. A closed stream
/// (`Ready(None)` forever) is cleared to `None` instead of spinning `run`'s `select!`.
async fn recv_wake(wake: &mut Option<PubSubStream>) {
    let dead = match wake {
        Some(stream) => stream.next().await.is_none(),
        None => std::future::pending::<bool>().await,
    };
    if dead {
        tracing::warn!("conveyor scheduler: pub/sub wake connection closed, falling back to the sleep cap until it reconnects");
        *wake = None;
    }
}

/// A plain `SET NX` would make the holder lose its own lock on the next tick; this atomically lets
/// the same replica renew while still failing for a foreign holder.
const ACQUIRE_OR_RENEW_SCRIPT: &str = r"
local v = redis.call('GET', KEYS[1])
if v == false then
    redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
    return 1
elseif v == ARGV[1] then
    redis.call('PEXPIRE', KEYS[1], ARGV[2])
    return 1
else
    return 0
end
";

/// Releases `domain_key` iff still held by `replica_id`, never a blind `DEL` of another replica's
/// lock.
const RELEASE_IF_HELD_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('DEL', KEYS[1])
else
    return 0
end
";

pub const LEADER_LOCK_TTL_MS: u64 = 35_000;

/// Distinguishes normal contention from a genuine acquire/renew failure, so they are counted
/// separately.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderLockOutcome {
    Acquired,
    Contended,
    Error,
}

/// Try to acquire (or renew) a scheduler leader lock. Efficiency-only: correctness never depends on
/// who holds it. `domain_key` keeps unrelated schedulers apart.
pub async fn acquire_leader_lock(
    valkey: &ConnectionManager,
    domain_key: &str,
    replica_id: &str,
) -> LeaderLockOutcome {
    let mut conn = valkey.clone();
    match redis::Script::new(ACQUIRE_OR_RENEW_SCRIPT)
        .key(domain_key)
        .arg(replica_id)
        .arg(LEADER_LOCK_TTL_MS)
        .invoke_async::<i64>(&mut conn)
        .await
    {
        Ok(1) => LeaderLockOutcome::Acquired,
        Ok(_) => LeaderLockOutcome::Contended,
        Err(e) => {
            tracing::error!(domain_key, error = %e, "leader lock acquire/renew failed, treating this tick as not-leader");
            LeaderLockOutcome::Error
        }
    }
}

/// Release the leader lock on clean shutdown so a waiting replica takes over immediately.
pub async fn release_leader_lock(valkey: &ConnectionManager, domain_key: &str, replica_id: &str) {
    let mut conn = valkey.clone();
    if let Err(e) = redis::Script::new(RELEASE_IF_HELD_SCRIPT)
        .key(domain_key)
        .arg(replica_id)
        .invoke_async::<i64>(&mut conn)
        .await
    {
        tracing::warn!(domain_key, error = %e, "leader lock release failed, leaving it to expire on its own TTL");
    }
}

/// Claim every due, enabled, unpaused schedule, advance `next_run_at` in the same transaction (so
/// it can't re-fire while in flight), and plan one task row per tick. Returns only tasks needing
/// enqueue.
pub async fn claim_and_plan(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<task_entity::Model>> {
    let txn = db.begin().await?;

    let due: Vec<schedule_entity::Model> = schedule_entity::Entity::find()
        .from_raw_sql(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT * FROM conveyor_schedules \
             WHERE next_run_at <= $1 AND enabled AND auto_paused_at IS NULL \
               AND (paused_until IS NULL OR paused_until <= $1) \
             ORDER BY next_run_at LIMIT $2 \
             FOR UPDATE SKIP LOCKED",
            [now.into(), claim_limit().into()],
        ))
        .all(&txn)
        .await?;

    let mut planned = Vec::new();
    for s in due {
        let Some(tick) = s.next_run_at.map(|t| t.with_timezone(&Utc)) else {
            continue;
        };

        let spec: ScheduleSpec = match serde_json::from_value(s.spec.clone()) {
            Ok(sp) => sp,
            Err(e) => {
                auto_pause_poison(&txn, s.id, &format!("unparseable spec: {e}")).await?;
                continue;
            }
        };
        let next = match compute_next(
            &spec,
            &s.timezone,
            now,
            s.consecutive_failures,
            s.id.cast_unsigned(),
        ) {
            Ok(n) => n,
            Err(e) => {
                auto_pause_poison(&txn, s.id, &format!("compute_next failed: {e}")).await?;
                continue;
            }
        };

        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_schedules SET next_run_at = $1, updated_at = now() WHERE id = $2",
            [next.into(), s.id.into()],
        ))
        .await?;

        if let Some(t) = plan_tick(&txn, &s, tick).await? {
            planned.push(t);
        }
    }

    txn.commit().await?;
    Ok(planned)
}

async fn auto_pause_poison(
    txn: &DatabaseTransaction,
    schedule_id: i64,
    reason: &str,
) -> anyhow::Result<()> {
    tracing::error!(
        schedule_id,
        reason,
        "conveyor scheduler: auto-pausing a schedule with an invalid spec/timezone"
    );
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE conveyor_schedules SET auto_paused_at = now(), updated_at = now() WHERE id = $1",
        [schedule_id.into()],
    ))
    .await?;
    Ok(())
}

/// Plan one task row for `s`'s due `tick`. `ON CONFLICT DO NOTHING` can trip
/// `uq_conveyor_tasks_entity_tick` or `uq_conveyor_tasks_inflight`; the follow-up `SELECT` tells
/// which, since they need opposite handling.
async fn plan_tick(
    txn: &DatabaseTransaction,
    s: &schedule_entity::Model,
    tick: DateTime<Utc>,
) -> anyhow::Result<Option<task_entity::Model>> {
    let inserted = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (tenant_id, task_type, entity_id, schedule_id, payload, resource_key, resource_cap, \
              scheduled_for, triggered_by, status, attempt, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'schedule', 'pending', 1, now()) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
            [
                s.tenant_id.into(),
                s.task_type.clone().into(),
                s.entity_id.into(),
                s.id.into(),
                s.payload.clone().into(),
                s.resource_key.clone().into(),
                s.resource_cap.into(),
                tick.into(),
            ],
        ))
        .await?;

    if let Some(row) = inserted.into_iter().next() {
        let id: i64 = row.try_get("", "id")?;
        return Ok(task_entity::Entity::find_by_id(id).one(txn).await?);
    }

    // Keyed on schedule_id: `entity_id = $2` never matches a NULL entity_id, which would skip the
    // overlap check.
    let tick_already_planned = !txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS one FROM conveyor_tasks \
             WHERE schedule_id = $1 AND scheduled_for = $2 LIMIT 1",
            [s.id.into(), tick.into()],
        ))
        .await?
        .is_empty();

    if tick_already_planned {
        return Ok(None);
    }

    // `coalesce` is not yet implemented; treated as `skip`, with a visible skipped row recorded.
    if s.overlap_policy != "skip" {
        tracing::warn!(
            schedule_id = s.id,
            overlap_policy = %s.overlap_policy,
            "conveyor scheduler: overlap_policy not implemented, treating as skip"
        );
    }
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO conveyor_tasks \
         (tenant_id, task_type, entity_id, schedule_id, scheduled_for, triggered_by, status, attempt, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'schedule', 'skipped', 1, now()) \
         ON CONFLICT DO NOTHING",
        [
            s.tenant_id.into(),
            s.task_type.clone().into(),
            s.entity_id.into(),
            s.id.into(),
            tick.into(),
        ],
    ))
    .await?;
    Ok(None)
}

/// Enqueue a planned task unless its resource is at cap (it then stays `pending`). The cap is soft:
/// two leaders can overshoot by one.
pub async fn try_enqueue(
    db: &DatabaseConnection,
    valkey: &ConnectionManager,
    task: &task_entity::Model,
) -> anyhow::Result<bool> {
    if resource::over_cap(db, task.resource_key.as_deref(), task.resource_cap, task.id).await? {
        return Ok(false);
    }

    queue::stamp_and_push(
        db,
        valkey,
        TaskId(task.id),
        task.task_type.clone().into(),
        task.payload.clone().unwrap_or(serde_json::Value::Null),
        task.attempt.max(1).cast_unsigned(),
    )
    .await?;
    Ok(true)
}

/// Same predicate as the claim query (minus `FOR UPDATE`), so a paused row can't pin the sleep to
/// 0.
async fn min_next_run_at(db: &DatabaseConnection, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let row = match db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT MIN(next_run_at) AS min_next FROM conveyor_schedules \
             WHERE enabled AND auto_paused_at IS NULL AND (paused_until IS NULL OR paused_until <= $1)",
            [now.into()],
        ))
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            // Log a transient DB blip; otherwise it looks like "nothing scheduled".
            tracing::warn!(error = %e, "conveyor scheduler: failed to compute the next due time, backing off");
            return None;
        }
    };
    row.try_get::<Option<DateTime<Utc>>>("", "min_next")
        .ok()
        .flatten()
}

fn sleep_until(next: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
    match next {
        Some(t) if t > now => (t - now)
            .to_std()
            .unwrap_or_default()
            .min(Duration::from_secs(max_sleep_secs())),
        Some(_) => Duration::from_millis(0),
        None => Duration::from_secs(max_sleep_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn sleep_duration_uses_post_work_clock() {
        let base = Utc::now();
        let next = base + chrono::Duration::seconds(100);
        let now = base + chrono::Duration::seconds(5);
        let dur = sleep_until(Some(next), now);
        assert!(
            dur >= Duration::from_secs(94) && dur <= Duration::from_secs(95),
            "expected ~95s (collapsed toward remaining time), got {dur:?}"
        );

        assert_eq!(sleep_until(Some(now), now), Duration::from_millis(0));
        assert_eq!(
            sleep_until(Some(now - chrono::Duration::seconds(1)), now),
            Duration::from_millis(0)
        );
        assert_eq!(
            sleep_until(None, now),
            Duration::from_secs(max_sleep_secs())
        );
    }

    #[tokio::test]
    async fn claim_and_plan_is_idempotent_under_two_concurrent_claims() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let entity_id = 1_000_000 + (suffix.as_u128() % 1_000_000) as i64;
        let due_at = Utc::now() - chrono::Duration::seconds(5);
        insert_schedule(
            &db,
            "toy_ok",
            Some(entity_id),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let now = Utc::now();
        let (a, b) = tokio::join!(claim_and_plan(&db, now), claim_and_plan(&db, now));
        a.unwrap();
        b.unwrap();

        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM conveyor_tasks WHERE task_type='toy_ok' AND entity_id=$1 AND schedule_id IS NOT NULL",
                [entity_id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one task row for this tick, not two");
    }

    /// The `uq_conveyor_tasks_schedule_inflight` partial unique index rejects a second in-flight
    /// task for an entity-less schedule.
    #[tokio::test]
    async fn an_entity_less_schedule_gets_overlap_protection() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("no_entity_overlap_{}", uuid::Uuid::new_v4());
        let first_due = Utc::now() - chrono::Duration::seconds(10);
        let schedule_id = insert_schedule(
            &db,
            &task_type,
            None,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            first_due,
            0,
        )
        .await;

        // Scoped to our own schedule_id; other tests' schedules share the Postgres instance.
        let planned = claim_and_plan(&db, Utc::now()).await.unwrap();
        assert_eq!(
            planned
                .iter()
                .filter(|t| t.schedule_id == Some(schedule_id))
                .count(),
            1,
            "the first tick must plan exactly one task for this schedule"
        );

        // A second tick becomes due before the first task completed.
        let second_due = Utc::now() - chrono::Duration::seconds(5);
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_schedules SET next_run_at = $1 WHERE id = $2",
            [second_due.into(), schedule_id.into()],
        ))
        .await
        .unwrap();

        let planned_again = claim_and_plan(&db, Utc::now()).await.unwrap();
        assert!(
            planned_again
                .iter()
                .all(|t| t.schedule_id != Some(schedule_id)),
            "the second, overlapping tick must not have planned a new in-flight task for this schedule"
        );

        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE schedule_id = $1 ORDER BY id",
                [schedule_id.into()],
            ))
            .await
            .unwrap();
        let statuses: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String>("", "status").unwrap())
            .collect();
        assert_eq!(
            statuses
                .iter()
                .filter(|s| *s == "pending" || *s == "running")
                .count(),
            1,
            "exactly one in-flight task row for this schedule, not two: {statuses:?}"
        );
        assert!(
            statuses.iter().any(|s| s == "skipped"),
            "the overlapping tick must have recorded a visible skip, not silently vanished: {statuses:?}"
        );
    }

    /// Entity-less variant of the concurrent-claim idempotency test.
    #[tokio::test]
    async fn claim_and_plan_is_idempotent_under_two_concurrent_claims_for_an_entity_less_schedule()
    {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("no_entity_concurrent_{}", uuid::Uuid::new_v4());
        let due_at = Utc::now() - chrono::Duration::seconds(5);
        insert_schedule(
            &db,
            &task_type,
            None,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let now = Utc::now();
        let (a, b) = tokio::join!(claim_and_plan(&db, now), claim_and_plan(&db, now));
        a.unwrap();
        b.unwrap();

        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM conveyor_tasks WHERE task_type = $1 AND entity_id IS NULL AND schedule_id IS NOT NULL",
                [task_type.into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one task row for this tick, not two");
    }

    /// The `ConveyorNullEntityIndexes` migration's `up()` must tolerate being applied twice.
    #[tokio::test]
    async fn conveyor_null_entity_indexes_migration_is_idempotent() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        use sea_orm_migration::{MigrationTrait, SchemaManager};
        let manager = SchemaManager::new(&db);
        crate::schema::ConveyorNullEntityIndexes
            .up(&manager)
            .await
            .unwrap();
        crate::schema::ConveyorNullEntityIndexes
            .up(&manager)
            .await
            .expect("a second up() against an already-migrated database must not error");
    }

    #[tokio::test]
    async fn build_plan_uses_the_moved_schedule_spec_and_advances_next_run_at() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let entity_id = 2_000_000 + (suffix.as_u128() % 1_000_000) as i64;
        let due_at = Utc::now() - chrono::Duration::seconds(5);
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(entity_id),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let planned = claim_and_plan(&db, Utc::now()).await.unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].entity_id, Some(entity_id));

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.next_run_at.unwrap().with_timezone(&Utc) > due_at,
            "next_run_at must advance past the fired tick"
        );
    }

    /// A retry through `try_enqueue` must never move `enqueued_at`.
    #[tokio::test]
    async fn try_enqueue_twice_leaves_the_first_enqueued_at_in_place() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_id =
            insert_task(&db, "toy", Some(unique_entity_id()), None, None, "pending").await;
        let task = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        assert!(try_enqueue(&db, &valkey, &task).await.unwrap());
        let read_enqueued_at = || async {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT enqueued_at FROM conveyor_tasks WHERE id = $1",
                    [task_id.into()],
                ))
                .await
                .unwrap()
                .unwrap();
            row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "enqueued_at")
                .unwrap()
        };
        let first = read_enqueued_at().await;

        assert!(try_enqueue(&db, &valkey, &task).await.unwrap());
        let second = read_enqueued_at().await;

        assert_eq!(first, second, "a retry must never reset the ceiling clock");
    }

    /// `enqueued_at` must be durable before the push, so a push failure still leaves the row
    /// visible to the sweeps.
    #[tokio::test]
    async fn try_enqueue_stamps_enqueued_at_before_pushing() {
        let Some((db, _valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_id =
            insert_task(&db, "toy", Some(unique_entity_id()), None, None, "pending").await;
        let task = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        // A Redis client on a dead port, built lazily, so the push fails.
        let dead_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let dead_valkey = dead_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");

        let result = try_enqueue(&db, &dead_valkey, &task).await;
        assert!(result.is_err(), "the push must actually fail in this test");

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT enqueued_at FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let enqueued_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "enqueued_at").unwrap();
        assert!(
            enqueued_at.is_some(),
            "enqueued_at must be stamped even when the push itself fails, so the sweeps can see this row"
        );
    }
}

#[cfg(test)]
mod wake_stream_tests {
    //! `recv_wake` must treat a dead `PubSubStream` as dead rather than spin the loop. Real Redis
    //! only.
    use super::*;
    use redis::AsyncCommands;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    /// Serializes the two tests that diff `CLIENT LIST` snapshots to find their connection.
    static SUBSCRIBE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Every connected client id whose last command was `SUBSCRIBE`, for isolating a fresh pub/sub
    /// connection.
    async fn subscribe_client_ids(conn: &mut redis::aio::MultiplexedConnection) -> Vec<String> {
        let list: String = redis::cmd("CLIENT")
            .arg("LIST")
            .query_async(conn)
            .await
            .unwrap();
        list.lines()
            .filter(|line| line.contains("cmd=subscribe"))
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|f| f.strip_prefix("id="))
                    .map(|id| id.to_string())
            })
            .collect()
    }

    #[tokio::test]
    async fn a_dead_pubsub_stream_does_not_spin_the_select_arm() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let _guard = SUBSCRIBE_LOCK.lock().await;
        let client = redis::Client::open(redis_url.clone()).unwrap();
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();

        let before = subscribe_client_ids(&mut admin).await;
        let mut wake = subscribe_wake(&redis_url).await;
        assert!(
            wake.is_some(),
            "subscribe_wake must have succeeded against a live Redis"
        );

        let after = subscribe_client_ids(&mut admin).await;
        let new_id = after
            .into_iter()
            .find(|id| !before.contains(id))
            .expect("the new pub/sub connection must show up in CLIENT LIST as cmd=subscribe");

        // Kills only this connection by id, not `TYPE pubsub`.
        let _: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(&new_id)
            .query_async(&mut admin)
            .await
            .unwrap();

        recv_wake(&mut wake).await;
        assert!(
            wake.is_none(),
            "recv_wake must clear a dead stream to None so run() falls back to the sleep cap and \
             retries the subscription later"
        );

        // A cleared wake must pend forever, not be instantly Ready (which would spin `run()`).
        assert!(
            tokio::time::timeout(Duration::from_millis(300), recv_wake(&mut wake))
                .await
                .is_err(),
            "recv_wake on an already-dead (now None) stream must stay pending, not resolve \
             instantly"
        );
    }

    #[tokio::test]
    async fn recv_wake_on_a_missing_subscription_is_pending() {
        let mut wake: Option<PubSubStream> = None;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), recv_wake(&mut wake))
                .await
                .is_err(),
            "recv_wake with no subscription must stay pending, not resolve instantly"
        );
    }

    #[tokio::test]
    async fn a_live_wake_message_resolves_recv_wake_and_keeps_the_stream() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let _guard = SUBSCRIBE_LOCK.lock().await;
        let mut wake = subscribe_wake(&redis_url).await;
        assert!(wake.is_some());

        let client = redis::Client::open(redis_url).unwrap();
        let mut publisher = client.get_multiplexed_async_connection().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _: i64 = publisher.publish(WAKE_CHANNEL, "1").await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), recv_wake(&mut wake))
            .await
            .expect("a live wake publish must resolve recv_wake promptly");
        assert!(
            wake.is_some(),
            "a live message must not clear the stream to None"
        );
    }
}

#[cfg(test)]
mod leader_lock_tests {
    use super::*;
    use redis::AsyncCommands;

    /// A plain `SET NX` can't renew; the same replica renews immediately and a foreign replica is
    /// still rejected.
    #[tokio::test]
    async fn same_replica_renews_immediately_foreign_replica_is_rejected() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let client = redis::Client::open(redis_url).unwrap();
        let redis = client.get_connection_manager().await.unwrap();
        let key = format!("test:conveyor-leader-lock:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        assert_eq!(
            acquire_leader_lock(&redis, &key, &holder).await,
            LeaderLockOutcome::Acquired
        );
        assert_eq!(
            acquire_leader_lock(&redis, &key, &other).await,
            LeaderLockOutcome::Contended
        );
        assert_eq!(
            acquire_leader_lock(&redis, &key, &holder).await,
            LeaderLockOutcome::Acquired,
            "the holder must be able to renew its own lock on the very next call"
        );

        let mut conn = redis.clone();
        let _: () = conn.del(&key).await.unwrap_or(());
    }

    /// Contention is never counted as failure, and a Redis error never as contention.
    #[tokio::test]
    async fn acquire_leader_lock_distinguishes_contention_from_error() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let client = redis::Client::open(redis_url).unwrap();
        let redis = client.get_connection_manager().await.unwrap();
        let key = format!("test:conveyor-leader-lock-outcome:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        assert_eq!(
            acquire_leader_lock(&redis, &key, &holder).await,
            LeaderLockOutcome::Acquired
        );
        assert_eq!(
            acquire_leader_lock(&redis, &key, &other).await,
            LeaderLockOutcome::Contended,
            "losing to another replica's live lock is contention, not an error"
        );

        // A client on a dead port, built lazily, so the script invocation fails.
        let bad_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let bad_redis = bad_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");
        assert_eq!(
            acquire_leader_lock(&bad_redis, &key, &holder).await,
            LeaderLockOutcome::Error,
            "an unreachable Redis must be counted as a genuine error, not contention"
        );

        let mut conn = redis.clone();
        let _: () = conn.del(&key).await.unwrap_or(());
    }

    #[tokio::test]
    async fn release_deletes_only_when_still_held_by_this_replica() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let client = redis::Client::open(redis_url).unwrap();
        let redis = client.get_connection_manager().await.unwrap();
        let key = format!("test:conveyor-leader-lock-release:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        assert_eq!(
            acquire_leader_lock(&redis, &key, &holder).await,
            LeaderLockOutcome::Acquired
        );
        release_leader_lock(&redis, &key, &holder).await;
        let mut conn = redis.clone();
        assert!(!conn.exists::<_, bool>(&key).await.unwrap());

        assert_eq!(
            acquire_leader_lock(&redis, &key, &other).await,
            LeaderLockOutcome::Acquired
        );
        release_leader_lock(&redis, &key, &holder).await;
        assert!(
            conn.exists::<_, bool>(&key).await.unwrap(),
            "must not delete a key a different replica now legitimately holds"
        );
        let _: () = conn.del(&key).await.unwrap_or(());
    }
}
