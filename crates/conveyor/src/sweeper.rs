//! Reclaims tasks whose worker crashed mid-flight, and retries tasks that never made it onto a
//! queue (crashed before enqueue, or were parked over their resource cap and may fit now).

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, Statement};

use crate::config::{abandoned_pending_secs, stale_pending_secs, sweep_batch_limit};
use crate::conveyor::TaskState;
use crate::entities::task as task_entity;
use crate::ids::TaskId;
use crate::lifecycle::{self, Outcome};

/// Rows each sweep attempted to reclaim and got an `Ok` back for; cumulative process-wide, for a
/// host's health/metrics output.
///
/// Can over-count by one on a harmless double-finalize, so it is a coarse "sweeper is working"
/// signal.
pub static SWEEPER_RECLAIMED_EXPIRED_LEASE: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_ABANDONED_PENDING: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_STALE_PENDING: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RELEASED_ORPHANED_WAITER: AtomicU64 = AtomicU64::new(0);

/// A `pending` task this many times older than [`stale_pending_secs`] is beyond any resource-cap
/// park or crash-before-enqueue gap; the likelier cause is a producer/consumer config mismatch, so
/// it is logged loudly.
const STALE_PENDING_LOUD_MULTIPLE: i64 = 10;

/// Load one sweep batch, or log and give up on this cycle. `what` names the batch in the log.
async fn load_batch(
    db: &DatabaseConnection,
    stmt: Statement,
    what: &str,
) -> Option<Vec<sea_orm::QueryResult>> {
    match db.query_all_raw(stmt).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::error!(error = %e, what, "conveyor sweeper: failed to load a sweep batch");
            None
        }
    }
}

/// A `running` task whose heartbeat stopped renewing `lease_expires_at` (worker crash) is failed
/// here, vacating its `uq_conveyor_tasks_inflight` slot. Tasks parked on `waiting_on_task_id` hold
/// no lease and are skipped; a claimed-but-never-queued waiter does hold one, so it is reclaimed
/// here.
pub async fn sweep_expired_leases(db: &DatabaseConnection, valkey: &ConnectionManager) {
    let Some(rows) = load_batch(
        db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM conveyor_tasks \
             WHERE status = 'running' AND lease_expires_at < now() AND waiting_on_task_id IS NULL \
             ORDER BY id LIMIT $1",
            [sweep_batch_limit().into()],
        ),
        "expired-lease tasks",
    )
    .await
    else {
        return;
    };

    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            continue;
        };
        tracing::warn!(
            task_id = id,
            "conveyor sweeper: reclaiming task with an expired lease"
        );
        if let Err(e) = lifecycle::finalize(
            db,
            valkey,
            TaskId(id),
            Outcome::new(TaskState::Failed, Some("worker lost".to_string())),
        )
        .await
        {
            tracing::error!(task_id = id, error = %e, "conveyor sweeper: failed to finalize an expired-lease task");
        } else {
            SWEEPER_RECLAIMED_EXPIRED_LEASE.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A `pending` task whose `enqueued_at` is older than [`abandoned_pending_secs`] has lost its queue
/// message (eviction, dead-letter, drain abort); past this ceiling it is finalized as `timed_out`
/// instead of retried forever. Keyed on `enqueued_at`, not `created_at`: `enqueued_at IS NULL`
/// means parked over cap or not yet pushed, which [`requeue_stale_pending`] owns. Goes through
/// [`lifecycle::finalize_pending`] for the full terminal treatment (inflight slot, Redis status,
/// schedule bookkeeping, waiter fan-out).
pub async fn fail_abandoned_pending(db: &DatabaseConnection, valkey: &ConnectionManager) {
    let Some(rows) = load_batch(
        db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, task_type, entity_id, enqueued_at FROM conveyor_tasks \
             WHERE status = 'pending' AND enqueued_at IS NOT NULL \
               AND enqueued_at < now() - ($1 || ' seconds')::interval \
             ORDER BY id LIMIT $2",
            [abandoned_pending_secs().into(), sweep_batch_limit().into()],
        ),
        "abandoned pending tasks",
    )
    .await
    else {
        return;
    };

    for row in rows {
        let (Ok(id), Ok(task_type), Ok(entity_id), Ok(enqueued_at)) = (
            row.try_get::<i64>("", "id"),
            row.try_get::<String>("", "task_type"),
            row.try_get::<Option<i64>>("", "entity_id"),
            row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "enqueued_at"),
        ) else {
            tracing::error!("conveyor sweeper: malformed abandoned-pending row, skipping");
            continue;
        };
        let age_secs = (Utc::now() - enqueued_at.with_timezone(&Utc)).num_seconds();
        tracing::error!(
            task_id = id,
            task_type = %task_type,
            ?entity_id,
            ?enqueued_at,
            age_secs,
            "conveyor sweeper: enqueued but never claimed — its queue message was lost"
        );
        if let Err(e) = lifecycle::finalize_pending(
            db,
            valkey,
            TaskId(id),
            Outcome::new(
                TaskState::TimedOut,
                Some(format!(
                    "never picked up off the queue: enqueued {age_secs}s ago, no worker ever claimed it"
                )),
            ),
        )
        .await
        {
            tracing::error!(task_id = id, error = %e, "conveyor sweeper: failed to finalize an abandoned pending task");
        } else {
            SWEEPER_RECLAIMED_ABANDONED_PENDING.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Fleet-wide cooldown between two re-pushes of the same already-enqueued task.
const REQUEUE_COOLDOWN_SECS: u64 = 300;

/// A `pending` task older than [`stale_pending_secs`] never made it onto its queue or was parked at
/// its resource cap; re-checking the cap and enqueuing (or leaving it for the next sweep) is the
/// retry.
///
/// An already-enqueued task still holds an envelope, so re-pushing every tick would duplicate it;
/// that is gated by [`REQUEUE_COOLDOWN_SECS`] in Redis. A never-enqueued task retries every sweep,
/// ungated.
pub async fn requeue_stale_pending(db: &DatabaseConnection, valkey: &ConnectionManager) {
    let Some(rows) = load_batch(
        db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM conveyor_tasks \
             WHERE status = 'pending' \
               AND created_at < now() - ($1 || ' seconds')::interval \
             ORDER BY id LIMIT $2",
            [stale_pending_secs().into(), sweep_batch_limit().into()],
        ),
        "stale pending tasks",
    )
    .await
    else {
        return;
    };

    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            continue;
        };
        let mut task = match task_entity::Entity::find_by_id(id).one(db).await {
            Ok(Some(task)) => task,
            Ok(None) => continue,
            Err(e) => {
                tracing::error!(task_id = id, error = %e, "conveyor sweeper: failed to load stale pending task");
                continue;
            }
        };

        let age_secs = (Utc::now() - task.created_at.with_timezone(&Utc)).num_seconds();
        if age_secs > stale_pending_secs() * STALE_PENDING_LOUD_MULTIPLE {
            tracing::error!(
                task_id = id,
                task_type = %task.task_type,
                entity_id = ?task.entity_id,
                age_secs,
                "conveyor sweeper: task has been pending for over 10x the stale threshold — \
                 likely a producer with no matching consumer (task type never registered, or a \
                 feature flag mismatch between producer and consumer processes)"
            );
        }

        if task.enqueued_at.is_some() {
            let mut conn = valkey.clone();
            let key = format!("arcodash:requeue-cooldown:task:{id}");
            let claimed: Result<Option<String>, _> = conn
                .set_options(
                    &key,
                    Utc::now().to_rfc3339(),
                    redis::SetOptions::default()
                        .conditional_set(redis::ExistenceCheck::NX)
                        .with_expiration(redis::SetExpiry::EX(REQUEUE_COOLDOWN_SECS)),
                )
                .await;
            match claimed {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(task_id = id, error = %e, "conveyor sweeper: requeue-cooldown check failed, re-enqueuing anyway");
                }
            }
        }

        // A row parked over its resource cap isn't really retried, so it must not burn an attempt;
        // pre-check the predicate `try_enqueue` applies.
        match crate::resource::over_cap(db, task.resource_key.as_deref(), task.resource_cap, id)
            .await
        {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                tracing::error!(task_id = id, error = %e, "conveyor sweeper: failed to check resource cap for stale pending task");
                continue;
            }
        }

        // Bumped before the retry so `attempt` moves past 1 for a task stuck retrying (letting a
        // max-attempts policy fire). If the UPDATE fails, skip the retry this pass rather than push
        // an unpersisted attempt number.
        let new_attempt = task.attempt.saturating_add(1);
        if let Err(e) = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_tasks SET attempt = $1 WHERE id = $2",
                [new_attempt.into(), id.into()],
            ))
            .await
        {
            tracing::error!(task_id = id, error = %e, "conveyor sweeper: failed to bump attempt before requeue");
            continue;
        }
        task.attempt = new_attempt;

        match crate::scheduler::try_enqueue(db, valkey, &task).await {
            Ok(true) => {
                SWEEPER_RECLAIMED_STALE_PENDING.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(task_id = id, error = %e, "conveyor sweeper: failed to re-enqueue a stale pending task");
            }
        }
    }
}

/// A waiter parked on a task that went terminal without a lifecycle finalize (e.g. a migration's
/// raw `UPDATE`) is stranded, since it holds no lease. Re-runs the fan-out for those; idempotent.
pub async fn fan_out_orphaned_waiters(db: &DatabaseConnection, valkey: &ConnectionManager) {
    let Some(rows) = load_batch(
        db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT DISTINCT w.waiting_on_task_id AS done_id, t.status FROM conveyor_tasks w \
             JOIN conveyor_tasks t ON t.id = w.waiting_on_task_id \
             WHERE w.status = 'running' AND w.waiting_on_task_id IS NOT NULL \
               AND t.status NOT IN ('pending','running') \
             ORDER BY done_id LIMIT $1",
            [sweep_batch_limit().into()],
        ),
        "orphaned waiters",
    )
    .await
    else {
        return;
    };

    if rows.is_empty() {
        return;
    }
    tracing::warn!(
        count = rows.len(),
        "conveyor sweeper: releasing waiters parked on a task that went terminal outside the lifecycle"
    );

    for row in rows {
        let (Ok(done_id), Ok(status)) = (
            row.try_get::<i64>("", "done_id"),
            row.try_get::<String>("", "status"),
        ) else {
            tracing::error!("conveyor sweeper: malformed orphaned-waiter row, skipping");
            continue;
        };
        let Some(state) = TaskState::from_db_str(&status) else {
            tracing::error!(done_id, status = %status, "conveyor sweeper: unrecognized terminal status on orphaned-waiter target, skipping");
            continue;
        };
        if let Err(e) = lifecycle::fan_out_waiters(db, valkey, TaskId(done_id), state).await {
            tracing::error!(done_id, error = %e, "conveyor sweeper: failed to fan out orphaned waiters");
        } else {
            SWEEPER_RELEASED_ORPHANED_WAITER.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue;
    use crate::test_support::*;
    use chrono::Utc;

    /// A bad query must come back as `None`, not panic or propagate, so one sweep's malformed SQL
    /// never takes down the loop.
    #[tokio::test]
    async fn load_batch_returns_none_on_a_bad_query() {
        let Some((db, _valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let result = load_batch(
            &db,
            Statement::from_string(
                DbBackend::Postgres,
                "SELECT id FROM this_table_does_not_exist",
            ),
            "test bad batch",
        )
        .await;
        assert!(
            result.is_none(),
            "a failed query must return None, not panic or propagate"
        );
    }

    #[tokio::test]
    async fn sweep_expired_leases_fails_the_task_and_frees_the_inflight_slot() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "running",
                lease_expires_at: Some(Utc::now() - chrono::Duration::seconds(30)),
                ..Default::default()
            },
        )
        .await;

        sweep_expired_leases(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status, error FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("worker lost"));

        // The in-flight slot must be vacated: a fresh pending row for the same (task_type,
        // entity_id) can be inserted.
        let insert = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks (task_type, entity_id, triggered_by, status, attempt, created_at) \
                 VALUES ('toy_ok', $1, 'enqueue', 'pending', 1, now())",
                [entity_id.into()],
            ))
            .await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the lease-expiry sweep"
        );
    }

    /// Each sweep function must bump its own counter on an actual reclaim.
    #[tokio::test]
    async fn sweeper_counters_track_a_reclaim_in_each_sweep_function() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let expired_lease_before = SWEEPER_RECLAIMED_EXPIRED_LEASE.load(Ordering::Relaxed);
        insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                status: "running",
                lease_expires_at: Some(Utc::now() - chrono::Duration::seconds(30)),
                ..Default::default()
            },
        )
        .await;
        sweep_expired_leases(&db, &valkey).await;
        assert!(
            SWEEPER_RECLAIMED_EXPIRED_LEASE.load(Ordering::Relaxed) > expired_lease_before,
            "sweep_expired_leases must bump its counter on an actual reclaim"
        );

        let abandoned_before = SWEEPER_RECLAIMED_ABANDONED_PENDING.load(Ordering::Relaxed);
        insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                status: "pending",
                enqueued_at: Some(
                    Utc::now() - chrono::Duration::seconds(abandoned_pending_secs() + 30),
                ),
                ..Default::default()
            },
        )
        .await;
        fail_abandoned_pending(&db, &valkey).await;
        assert!(
            SWEEPER_RECLAIMED_ABANDONED_PENDING.load(Ordering::Relaxed) > abandoned_before,
            "fail_abandoned_pending must bump its counter on an actual reclaim"
        );

        let stale_before = SWEEPER_RECLAIMED_STALE_PENDING.load(Ordering::Relaxed);
        insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                ..Default::default()
            },
        )
        .await;
        requeue_stale_pending(&db, &valkey).await;
        assert!(
            SWEEPER_RECLAIMED_STALE_PENDING.load(Ordering::Relaxed) > stale_before,
            "requeue_stale_pending must bump its counter on an actual re-enqueue"
        );
    }

    /// A leaked `pending` task (queue message long gone) must be terminalized by
    /// `fail_abandoned_pending`, freeing its inflight slot and writing a terminal Redis job status.
    #[tokio::test]
    async fn fail_abandoned_pending_finalizes_a_leaked_enqueued_task_and_frees_the_inflight_slot() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                enqueued_at: Some(
                    Utc::now() - chrono::Duration::seconds(abandoned_pending_secs() + 30),
                ),
                ..Default::default()
            },
        )
        .await;

        fail_abandoned_pending(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status, finished_at, error FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        let finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "finished_at").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(status, "timed_out");
        assert!(finished_at.is_some());
        assert!(error.map(|e| !e.is_empty()).unwrap_or(false));

        // The in-flight slot must be vacated: a fresh pending row for the same (task_type,
        // entity_id) can be inserted.
        let insert = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks (task_type, entity_id, triggered_by, status, attempt, created_at) \
                 VALUES ('toy_ok', $1, 'enqueue', 'pending', 1, now())",
                [entity_id.into()],
            ))
            .await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after fail_abandoned_pending"
        );

        let job_status = queue::get_job_status(&valkey, TaskId(task_id))
            .await
            .unwrap();
        assert_eq!(
            job_status.map(|s| s.status),
            Some("timed_out".to_string()),
            "the terminal Redis job status must actually be written"
        );
    }

    /// A `pending` task parked over its resource cap (`enqueued_at IS NULL`) belongs to
    /// `requeue_stale_pending`; `fail_abandoned_pending` must never touch it.
    #[tokio::test]
    async fn fail_abandoned_pending_leaves_a_never_enqueued_parked_task_untouched() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                created_at: Some(
                    Utc::now() - chrono::Duration::seconds(abandoned_pending_secs() * 10),
                ),
                ..Default::default()
            },
        )
        .await;

        fail_abandoned_pending(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        assert_eq!(
            status, "pending",
            "a task parked over its cap (never enqueued) must not be touched by the ceiling sweep"
        );
    }

    /// A task claimed into `running` between the sweep's batch `SELECT` and the per-row finalize
    /// must not be terminalized; `finalize_pending`'s CAS only matches `pending`.
    #[tokio::test]
    async fn fail_abandoned_pending_does_not_clobber_a_task_claimed_into_running_after_its_scan() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                enqueued_at: Some(
                    Utc::now() - chrono::Duration::seconds(abandoned_pending_secs() + 30),
                ),
                ..Default::default()
            },
        )
        .await;

        // Simulates a consumer winning the pending->running CAS after the batch SELECT but before
        // the per-row finalize.
        assert!(lifecycle::start(&db, TaskId(task_id)).await.unwrap());

        fail_abandoned_pending(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        assert_eq!(
            status, "running",
            "a task claimed into running after the sweep's scan must not be finalized out from under it"
        );

        // uq_conveyor_tasks_inflight must still be held by the running row: a fresh pending row is
        // refused.
        let insert = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks (task_type, entity_id, triggered_by, status, attempt, created_at) \
                 VALUES ('toy_ok', $1, 'enqueue', 'pending', 1, now())",
                [entity_id.into()],
            ))
            .await;
        assert!(
            insert.is_err(),
            "the claimed task's in-flight slot must not have been released"
        );

        // Left `running` with no lease, this row would inflate later tests' reclaim counts.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET status = 'failed' WHERE id = $1",
            [task_id.into()],
        ))
        .await
        .unwrap();
    }

    /// A leaked `pending` task must not saturate a resource cap of 1 forever: proves B is refused
    /// first, then that the sweep pair in production order clears it.
    #[tokio::test]
    async fn fail_abandoned_pending_then_requeue_frees_the_wedged_cap_slot() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let key = format!("r-wedge-{}", uuid::Uuid::new_v4());

        let task_a = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                resource_key: Some(&key),
                resource_cap: Some(1),
                status: "pending",
                enqueued_at: Some(
                    Utc::now() - chrono::Duration::seconds(abandoned_pending_secs() + 30),
                ),
                ..Default::default()
            },
        )
        .await;

        // Task B: never-enqueued, stale enough to match requeue_stale_pending's predicate.
        let task_b_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                resource_key: Some(&key),
                resource_cap: Some(1),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                ..Default::default()
            },
        )
        .await;

        let task_b = task_entity::Entity::find_by_id(task_b_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let admitted = crate::scheduler::try_enqueue(&db, &valkey, &task_b)
            .await
            .unwrap();
        assert!(
            !admitted,
            "the wedge is real: B must be refused while A's leaked count still saturates the cap"
        );

        fail_abandoned_pending(&db, &valkey).await;
        requeue_stale_pending(&db, &valkey).await;

        let a_row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [task_a.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let a_status: String = a_row.try_get("", "status").unwrap();
        assert_eq!(
            a_status, "timed_out",
            "the leaked task must be terminalized"
        );

        let b_status = queue::get_job_status(&valkey, TaskId(task_b_id))
            .await
            .unwrap();
        assert_eq!(
            b_status.map(|s| s.status),
            Some("pending".to_string()),
            "B must actually be enqueued for real once the leaked slot frees"
        );
    }

    #[tokio::test]
    async fn requeue_stale_pending_enqueues_a_task_that_never_made_it_onto_a_queue() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                payload: Some(serde_json::json!({"n": 1})),
                created_at: Some(Utc::now() - chrono::Duration::seconds(120)),
                ..Default::default()
            },
        )
        .await;

        requeue_stale_pending(&db, &valkey).await;

        let status = queue::get_job_status(&valkey, TaskId(task_id))
            .await
            .unwrap();
        assert!(
            status.is_some(),
            "a stale pending task must actually be re-enqueued"
        );
        assert_eq!(status.unwrap().status, "pending");
    }

    /// A task stuck retrying stale-pending must leave `attempt = 1` behind on its first retry.
    #[tokio::test]
    async fn requeue_stale_pending_increments_the_attempt_counter() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("toy-attempt-bump-{}", uuid::Uuid::new_v4());
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                entity_id: Some(entity_id),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                ..Default::default()
            },
        )
        .await;

        // Tests share one Postgres and the sweep scans every stale-pending row, so assert forward
        // progress relative to the previous read, not an exact value.
        let before = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        requeue_stale_pending(&db, &valkey).await;

        let after = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            after.attempt > before.attempt,
            "a stale-pending requeue must bump the attempt counter (before={}, after={})",
            before.attempt,
            after.attempt
        );

        let popped = queue::blpop(&valkey, &queue::queue_key(&task_type), 2.0)
            .await
            .unwrap()
            .expect("the requeue must have pushed an envelope");
        assert!(
            popped.attempt >= 2,
            "the re-pushed envelope must carry a bumped attempt, not the original value of 1, \
             got {}",
            popped.attempt
        );
    }

    /// A stale-pending row refused by its resource cap must keep its `attempt`; once the cap frees,
    /// the next sweep both bumps the counter and enqueues it.
    #[tokio::test]
    async fn requeue_stale_pending_does_not_bump_attempt_while_cap_parked() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("toy-cap-parked-{}", uuid::Uuid::new_v4());
        let key = format!("r-cap-parked-{}", uuid::Uuid::new_v4());

        let holder_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(unique_entity_id()),
                resource_key: Some(&key),
                resource_cap: Some(1),
                status: "running",
                ..Default::default()
            },
        )
        .await;

        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                entity_id: Some(unique_entity_id()),
                resource_key: Some(&key),
                resource_cap: Some(1),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                ..Default::default()
            },
        )
        .await;

        requeue_stale_pending(&db, &valkey).await;

        let parked = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            parked.attempt, 1,
            "a row refused by its own resource cap must not have its attempt bumped"
        );
        assert!(
            queue::get_job_status(&valkey, TaskId(task_id))
                .await
                .unwrap()
                .is_none(),
            "a cap-parked row must never be enqueued"
        );

        // Free the slot and sweep again: now the retry actually happens, and the bump goes with it.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET status = 'success', finished_at = now() WHERE id = $1",
            [holder_id.into()],
        ))
        .await
        .unwrap();

        requeue_stale_pending(&db, &valkey).await;

        let retried = task_entity::Entity::find_by_id(task_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            retried.attempt > parked.attempt,
            "once the cap frees up, the retry must bump the attempt"
        );

        let popped = queue::blpop(&valkey, &queue::queue_key(&task_type), 2.0)
            .await
            .unwrap()
            .expect("the requeue must have pushed an envelope once the cap freed");
        assert_eq!(
            popped.attempt, retried.attempt as u32,
            "the persisted attempt and the enqueued envelope's attempt must agree"
        );
    }

    /// Scans a task-type's queue for envelopes carrying `task_id`, removes them all and returns the
    /// count (scoped to one task id).
    async fn drain_envelopes_for_task(
        valkey: &ConnectionManager,
        task_type: &str,
        task_id: i64,
    ) -> usize {
        let mut conn = valkey.clone();
        let key = crate::queue::queue_key(task_type);
        let items: Vec<String> = conn.lrange(&key, 0, -1).await.unwrap();
        let mut count = 0;
        for item in items {
            let Ok(envelope) = serde_json::from_str::<crate::envelope::Envelope>(&item) else {
                continue;
            };
            if envelope.task_id == TaskId(task_id) {
                let _: i64 = conn.lrem(&key, 1, item.as_str()).await.unwrap();
                count += 1;
            }
        }
        count
    }

    /// An already-enqueued task stuck `pending` (its message never claimed) must get exactly one
    /// re-push per cooldown window, not one per sweep tick.
    #[tokio::test]
    async fn requeue_stale_pending_gates_a_second_push_of_an_already_enqueued_task_behind_a_cooldown(
    ) {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                enqueued_at: Some(
                    Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30),
                ),
                ..Default::default()
            },
        )
        .await;

        requeue_stale_pending(&db, &valkey).await;
        assert_eq!(
            drain_envelopes_for_task(&valkey, "toy_ok", task_id).await,
            1,
            "the first sweep to see this stale, already-enqueued task must re-push it once"
        );

        requeue_stale_pending(&db, &valkey).await;
        assert_eq!(
            drain_envelopes_for_task(&valkey, "toy_ok", task_id).await,
            0,
            "an immediate second sweep must be gated by the cooldown, not push a duplicate"
        );

        let mut conn = valkey.clone();
        let _: () = conn
            .del(format!("arcodash:requeue-cooldown:task:{task_id}"))
            .await
            .unwrap();
        requeue_stale_pending(&db, &valkey).await;
        assert_eq!(
            drain_envelopes_for_task(&valkey, "toy_ok", task_id).await,
            1,
            "clearing the cooldown key must restore the push"
        );
    }

    /// A never-enqueued task holds no envelope to duplicate, so it retries every sweep with no
    /// cooldown.
    #[tokio::test]
    async fn requeue_stale_pending_never_enqueued_task_is_pushed_by_consecutive_sweeps() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let entity_id = unique_entity_id();
        let task_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "toy_ok",
                entity_id: Some(entity_id),
                status: "pending",
                created_at: Some(Utc::now() - chrono::Duration::seconds(stale_pending_secs() + 30)),
                ..Default::default()
            },
        )
        .await;

        requeue_stale_pending(&db, &valkey).await;
        assert_eq!(
            drain_envelopes_for_task(&valkey, "toy_ok", task_id).await,
            1
        );
        requeue_stale_pending(&db, &valkey).await;
        assert_eq!(
            drain_envelopes_for_task(&valkey, "toy_ok", task_id).await,
            1,
            "a task with no prior enqueued_at must not be blocked by a cooldown it never claimed"
        );
    }

    /// Simulates an out-of-band `UPDATE` (e.g. a migration) terminalizing a task a waiter is parked
    /// on; the sweep must release it.
    #[tokio::test]
    async fn fan_out_orphaned_waiters_releases_a_waiter_parked_on_an_out_of_band_terminal_task() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "target",
                entity_id: Some(unique_entity_id()),
                status: "running",
                ..Default::default()
            },
        )
        .await;
        let waiter_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "waiter",
                entity_id: Some(unique_entity_id()),
                status: "running",
                waiting_on_task_id: Some(target_id),
                lease_expires_at: None,
                ..Default::default()
            },
        )
        .await;

        // Out-of-band terminalization, bypassing lifecycle::finalize's fan-out.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET status = 'cancelled', finished_at = now() WHERE id = $1",
            [target_id.into()],
        ))
        .await
        .unwrap();

        fan_out_orphaned_waiters(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT waiting_on_task_id FROM conveyor_tasks WHERE id = $1",
                [waiter_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_task_id").unwrap();
        assert_eq!(
            waiting_on, None,
            "the sweep must clear the stranded waiter's wait link"
        );

        let popped = queue::blpop(&valkey, &queue::queue_key("waiter"), 2.0)
            .await
            .unwrap()
            .expect("a resume envelope must have been enqueued for the released waiter");
        assert_eq!(popped.task_id, TaskId(waiter_id));
        assert!(popped.resume);
    }

    /// A waiter whose target is still in flight is not orphaned; the sweep must leave it alone.
    #[tokio::test]
    async fn fan_out_orphaned_waiters_ignores_a_waiter_whose_target_is_still_in_flight() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "target",
                entity_id: Some(unique_entity_id()),
                status: "running",
                ..Default::default()
            },
        )
        .await;
        let waiter_id = insert_task_full(
            &db,
            InsertTask {
                task_type: "waiter",
                entity_id: Some(unique_entity_id()),
                status: "running",
                waiting_on_task_id: Some(target_id),
                lease_expires_at: None,
                ..Default::default()
            },
        )
        .await;

        fan_out_orphaned_waiters(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT waiting_on_task_id FROM conveyor_tasks WHERE id = $1",
                [waiter_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_task_id").unwrap();
        assert_eq!(
            waiting_on,
            Some(target_id),
            "a waiter whose target is still in flight must not be touched"
        );

        // Clean up: leave nothing running-with-no-lease behind for a later test's sweep to trip on.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET status = 'failed' WHERE id IN ($1, $2)",
            [target_id.into(), waiter_id.into()],
        ))
        .await
        .unwrap();
    }
}
