//! The task state machine: `pending -> running -> success|failed|timed_out|cancelled|skipped`, plus
//! the `WaitOn` park/resume primitive. Every terminal transition funnels through [`finalize`], so
//! bookkeeping happens exactly once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use sea_orm::{ConnectionTrait, DbBackend, EntityTrait, Statement, TransactionTrait};

use crate::config::{abandoned_pending_secs, auto_pause_threshold, lease_secs};
use crate::conveyor::TaskState;
use crate::entities::schedule as schedule_entity;
use crate::envelope::{Envelope, ResumePayload};
use crate::ids::TaskId;
use crate::lease::HeartbeatGuard;
use crate::queue;
use crate::schedule_spec::{compute_next, ScheduleSpec};
use crate::worker_identity::worker_id;

/// Finalizes that lost the terminal CAS and discarded their outcome
/// (`arcodash_conveyor_finalize_lost_race_total`).
pub static FINALIZE_LOST_RACE: AtomicU64 = AtomicU64::new(0);

/// Schedules auto-paused after crossing [`auto_pause_threshold`]
/// (`arcodash_conveyor_schedule_auto_paused_total`), so operators can alert on a silent schedule.
pub static SCHEDULE_AUTO_PAUSED: AtomicU64 = AtomicU64::new(0);

/// Claim a `pending` task via `pending->running` CAS; `Ok(false)` means it was already claimed, so
/// drop the job.
pub async fn start(db: &impl ConnectionTrait, task_id: TaskId) -> anyhow::Result<bool> {
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks \
             SET status = 'running', started_at = now(), worker_id = $1, \
                 lease_expires_at = now() + ($2 || ' seconds')::interval \
             WHERE id = $3 AND status = 'pending'",
            [
                worker_id().into(),
                lease_secs().to_string().into(),
                task_id.0.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Spawn a heartbeat guard extending `lease_expires_at` every `lease_secs/3` while the task stays
/// `running`; dropping the guard stops renewal.
pub fn spawn_heartbeat(db: sea_orm::DatabaseConnection, task_id: TaskId) -> HeartbeatGuard {
    let period = Duration::from_secs((lease_secs() / 3).max(1).cast_unsigned());
    HeartbeatGuard::spawn(period, move || {
        let db = db.clone();
        async move {
            let res = db
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE conveyor_tasks SET lease_expires_at = now() + ($1 || ' seconds')::interval \
                     WHERE id = $2 AND status = 'running'",
                    [lease_secs().to_string().into(), task_id.0.into()],
                ))
                .await?;
            Ok(res.rows_affected())
        }
    })
}

/// Park the current task on `WaitOn { task_id }`: set `waiting_on_task_id`, clear the lease
/// (`sweep_expired_leases` skips parked waiters), keep `status = 'running'`. Does NOT finalize;
/// [`fan_out_waiters`] gives a resumed waiter a lease again.
pub async fn park(
    db: &impl ConnectionTrait,
    task_id: TaskId,
    waiting_on: TaskId,
) -> anyhow::Result<()> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE conveyor_tasks SET waiting_on_task_id = $1, lease_expires_at = NULL \
         WHERE id = $2 AND status = 'running'",
        [waiting_on.0.into(), task_id.0.into()],
    ))
    .await?;
    Ok(())
}

/// Caps a discarded `error/result_ref` logged on a lost finalize race at ~512 chars.
fn truncate_for_log(s: Option<&str>) -> Option<String> {
    const LIMIT: usize = 512;
    s.map(|s| {
        let mut truncated: String = s.chars().take(LIMIT).collect();
        if truncated.len() < s.len() {
            truncated.push('…');
        }
        truncated
    })
}

pub struct Outcome {
    pub status: TaskState,
    pub error: Option<String>,
    pub result_ref: Option<serde_json::Value>,
}

impl Outcome {
    pub(crate) const fn new(status: TaskState, error: Option<String>) -> Self {
        Self {
            status,
            error,
            result_ref: None,
        }
    }
}

/// `cas_statuses` is a literal `IN (...)` list, never caller data (Postgres can't bind it as one
/// parameter). `infra_caused` (only from `finalize_pending`) keeps the outcome out of auto-pause
/// accounting.
async fn finalize_matching(
    db: &sea_orm::DatabaseConnection,
    valkey: &redis::aio::ConnectionManager,
    task_id: TaskId,
    outcome: Outcome,
    cas_statuses: &'static str,
    infra_caused: bool,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let rows = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "UPDATE conveyor_tasks \
                 SET status = $1, finished_at = now(), lease_expires_at = NULL, \
                     duration_ms = CASE WHEN started_at IS NOT NULL \
                                        THEN EXTRACT(EPOCH FROM (now() - started_at)) * 1000 \
                                        ELSE NULL END, \
                     error = $2, result_ref = $3 \
                 WHERE id = $4 AND status IN {cas_statuses} \
                 RETURNING schedule_id"
            ),
            [
                outcome.status.as_db_str().into(),
                outcome.error.clone().into(),
                outcome.result_ref.clone().into(),
                task_id.0.into(),
            ],
        ))
        .await?;

    let Some(row) = rows.into_iter().next() else {
        const TERMINAL_STATUSES: &[&str] =
            &["success", "failed", "timed_out", "cancelled", "skipped"];
        // The CAS matched no row. A different terminal status already written is the defect this
        // counts; the same status is a harmless double-finalize; a non-terminal/missing row is
        // `finalize_pending`'s routine CAS-miss and MUST stay quiet
        // (`sweeper::fail_abandoned_pending` relies on it).
        let actual_status: Option<String> = match txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [task_id.0.into()],
            ))
            .await?
        {
            Some(r) => r.try_get("", "status")?,
            None => None,
        };
        txn.commit().await?;

        match actual_status.as_deref() {
            Some(actual) if TERMINAL_STATUSES.contains(&actual) => {
                if actual == outcome.status.as_db_str() {
                    tracing::debug!(
                        task_id = task_id.0,
                        status = actual,
                        "lifecycle: finalize CAS missed a row already finalized with the same status (double-finalize)"
                    );
                } else {
                    FINALIZE_LOST_RACE.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        task_id = task_id.0,
                        requested = outcome.status.as_db_str(),
                        actual,
                        discarded_error = ?truncate_for_log(outcome.error.as_deref()),
                        discarded_result_ref = ?truncate_for_log(
                            outcome.result_ref.as_ref().map(std::string::ToString::to_string).as_deref()
                        ),
                        "lifecycle: finalize lost the terminal CAS to another writer and discarded its outcome"
                    );
                }
            }
            _ => {
                tracing::debug!(
                    task_id = task_id.0,
                    "lifecycle: finalize CAS missed a non-terminal or missing row (e.g. finalize_pending losing to a claim)"
                );
            }
        }
        return Ok(());
    };
    let schedule_id: Option<i64> = row.try_get("", "schedule_id")?;

    if let Some(schedule_id) = schedule_id {
        // Infra-caused terminations say nothing about the task itself, so they leave the failure
        // streak exactly as found (neither counted nor reset).
        let accounting = if infra_caused {
            FailureAccounting::Unchanged
        } else if outcome.status.is_failure() {
            FailureAccounting::Counted
        } else {
            FailureAccounting::Reset
        };
        apply_schedule_bookkeeping(&txn, schedule_id, accounting).await?;
    }

    txn.commit().await?;

    // The Redis `job:<task_id>` record is written `pending` at enqueue and must be updated here.
    // Best-effort: a Redis failure must not fail a committed finalize.
    if let Err(e) = queue::set_job_status(
        valkey,
        task_id,
        outcome.status.as_db_str(),
        outcome.error.clone(),
    )
    .await
    {
        tracing::error!(task_id = task_id.0, error = %e, "lifecycle: failed to write terminal job status to redis");
    }

    // Best-effort: the finalize already committed, so a fan-out failure must not report it as
    // failed.
    if let Err(e) = fan_out_waiters(db, valkey, task_id, outcome.status).await {
        tracing::error!(task_id = task_id.0, error = %e, "lifecycle: failed to fan out to waiting tasks");
    }

    publish_wake(valkey).await;

    Ok(())
}

/// Terminal transition for one task. Idempotent: the UPDATE only matches `running`/`pending` rows,
/// so a double-finalize is a harmless 0-row no-op.
pub async fn finalize(
    db: &sea_orm::DatabaseConnection,
    valkey: &redis::aio::ConnectionManager,
    task_id: TaskId,
    outcome: Outcome,
) -> anyhow::Result<()> {
    finalize_matching(
        db,
        valkey,
        task_id,
        outcome,
        "('running', 'pending')",
        false,
    )
    .await
}

/// Like [`finalize`], but only matches a `pending` row, so the abandoned-pending sweep never
/// terminalizes a row a consumer already claimed. Always infra-caused, so never counted toward
/// auto-pause.
pub async fn finalize_pending(
    db: &sea_orm::DatabaseConnection,
    valkey: &redis::aio::ConnectionManager,
    task_id: TaskId,
    outcome: Outcome,
) -> anyhow::Result<()> {
    finalize_matching(db, valkey, task_id, outcome, "('pending')", true).await
}

/// Nudge the scheduler's deadline loop; a finished task may free a resource-cap slot. Best-effort.
async fn publish_wake(valkey: &redis::aio::ConnectionManager) {
    use redis::AsyncCommands;
    let mut conn = valkey.clone();
    let result: Result<i64, _> = conn.publish(crate::scheduler::WAKE_CHANNEL, "1").await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to publish a scheduler wake; the loop falls back to its sleep cap");
    }
}

/// Wake every task waiting on the just-finalized task. The claim UPDATE atomically clears
/// `waiting_on_task_id`, so each waiter is resumed exactly once (contending with the sweeper's
/// CAS). It also gives each waiter a lease (`abandoned_pending_secs`), so if the process dies
/// before the push reaches Redis, [`sweep_expired_leases`] reclaims it. A push failure is logged
/// and skipped so it can't strand the rest of the batch; the first error is returned for the caller
/// to log.
pub async fn fan_out_waiters(
    db: &sea_orm::DatabaseConnection,
    valkey: &redis::aio::ConnectionManager,
    done_task_id: TaskId,
    status: TaskState,
) -> anyhow::Result<()> {
    let claimed = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks \
             SET waiting_on_task_id = NULL, \
                 lease_expires_at = now() + ($2 || ' seconds')::interval \
             WHERE waiting_on_task_id = $1 AND status = 'running' \
             RETURNING id, task_type, attempt",
            [done_task_id.0.into(), abandoned_pending_secs().into()],
        ))
        .await?;

    let mut first_err: Option<anyhow::Error> = None;
    for row in claimed {
        let waiter_id: i64 = row.try_get("", "id")?;
        let task_type: String = row.try_get("", "task_type")?;
        let attempt: i32 = row.try_get("", "attempt")?;
        let payload = serde_json::to_value(ResumePayload {
            done_task_id,
            done_task_status: status.as_db_str().to_string(),
        })?;
        let envelope = Envelope {
            task_id: TaskId(waiter_id),
            task_type: task_type.into(),
            payload,
            attempt: attempt.max(1).cast_unsigned(),
            enqueued_at: Utc::now(),
            resume: true,
        };
        if let Err(e) = queue::push(valkey, &envelope).await {
            tracing::error!(
                task_id = waiter_id,
                done_task_id = done_task_id.0,
                error = %e,
                "lifecycle: failed to push a resume envelope for a fanned-out waiter — it holds a \
                 lease and will be reclaimed by the lease sweep"
            );
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

enum FailureAccounting {
    /// A genuine failure/timeout: increments the streak, auto-pausing at [`auto_pause_threshold`].
    Counted,
    /// A genuine success (or cancelled/skipped): resets the streak to 0.
    Reset,
    /// An infra-caused termination: leaves the streak untouched.
    Unchanged,
}

/// `claim_and_plan` already advanced `next_run_at`, so only the failure path recomputes it here,
/// anchored on completion time, to apply backoff.
async fn apply_schedule_bookkeeping(
    txn: &impl ConnectionTrait,
    schedule_id: i64,
    accounting: FailureAccounting,
) -> anyhow::Result<()> {
    let Some(s) = schedule_entity::Entity::find_by_id(schedule_id)
        .one(txn)
        .await?
    else {
        return Ok(());
    };

    let consecutive_failures = match accounting {
        FailureAccounting::Counted => s.consecutive_failures + 1,
        FailureAccounting::Reset => 0,
        FailureAccounting::Unchanged => s.consecutive_failures,
    };
    let counted = matches!(accounting, FailureAccounting::Counted);
    let next_run_at = if counted {
        match serde_json::from_value::<ScheduleSpec>(s.spec.clone()) {
            Ok(spec) => compute_next(
                &spec,
                &s.timezone,
                Utc::now(),
                consecutive_failures,
                s.id.cast_unsigned(),
            )
            .inspect_err(|e| {
                tracing::error!(schedule_id, error = %e, "lifecycle: compute_next failed at finalize");
            })
            .ok(),
            Err(e) => {
                tracing::error!(schedule_id, error = %e, "lifecycle: unparseable spec at finalize, leaving next_run_at untouched");
                None
            }
        }
    } else {
        None
    };
    let auto_pause = counted && consecutive_failures >= auto_pause_threshold();

    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE conveyor_schedules \
         SET consecutive_failures = $1, next_run_at = COALESCE($2, next_run_at), \
             auto_paused_at = CASE WHEN $3 THEN now() ELSE auto_paused_at END, \
             updated_at = now() \
         WHERE id = $4",
        [
            consecutive_failures.into(),
            next_run_at.into(),
            auto_pause.into(),
            schedule_id.into(),
        ],
    ))
    .await?;

    if auto_pause {
        SCHEDULE_AUTO_PAUSED.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            schedule_id,
            consecutive_failures,
            "lifecycle: schedule auto-paused after repeated failures"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use sea_orm::Statement;

    /// `CONVEYOR_AUTO_PAUSE_THRESHOLD` is a process-global env var; tests overriding it must hold
    /// this lock until restored. `tokio::sync::Mutex` because the guard is held across `.await`.
    static AUTO_PAUSE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn outcome_new_leaves_result_ref_none() {
        let outcome = Outcome::new(TaskState::Failed, Some("boom".to_string()));
        assert_eq!(outcome.status, TaskState::Failed);
        assert_eq!(outcome.error, Some("boom".to_string()));
        assert_eq!(outcome.result_ref, None);
    }

    #[tokio::test]
    async fn start_claims_pending_and_rejects_a_second_claim() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id =
            TaskId(insert_task(&db, "toy", Some(unique_entity_id()), None, None, "pending").await);

        assert!(start(&db, id).await.unwrap(), "first claim must succeed");
        assert!(
            !start(&db, id).await.unwrap(),
            "second claim must be rejected"
        );
    }

    #[tokio::test]
    async fn finalize_is_idempotent_and_auto_pauses_the_schedule_at_threshold() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _env_guard = AUTO_PAUSE_ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_AUTO_PAUSE_THRESHOLD", "3");
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now(),
            2, // one failure away from the threshold
        )
        .await;
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    schedule_id: Some(schedule_id),
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let auto_paused_before = SCHEDULE_AUTO_PAUSED.load(Ordering::Relaxed);

        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Failed, Some("boom".to_string())),
        )
        .await
        .unwrap();

        // A second finalize must be a no-op, not double-count the failure.
        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Failed, Some("boom again".to_string())),
        )
        .await
        .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.consecutive_failures, 3);
        assert!(reloaded.auto_paused_at.is_some());
        assert_eq!(
            SCHEDULE_AUTO_PAUSED.load(Ordering::Relaxed),
            auto_paused_before + 1,
            "auto-pausing must bump the counter exactly once, including across the idempotent \
             second finalize"
        );
        std::env::remove_var("CONVEYOR_AUTO_PAUSE_THRESHOLD");
    }

    /// A task the sweeper times out because its queue message was lost must leave the failure
    /// streak untouched.
    #[tokio::test]
    async fn finalize_pending_timed_out_does_not_count_toward_auto_pause() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let seeded_failures = auto_pause_threshold() - 1; // one genuine failure away from the threshold
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now(),
            seeded_failures,
        )
        .await;
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    schedule_id: Some(schedule_id),
                    status: "pending",
                    ..Default::default()
                },
            )
            .await,
        );

        finalize_pending(
            &db,
            &valkey,
            task_id,
            Outcome::new(
                TaskState::TimedOut,
                Some("never picked up off the queue".to_string()),
            ),
        )
        .await
        .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.consecutive_failures, seeded_failures,
            "an infra-caused timeout must leave an already-accumulated failure streak exactly where it found it"
        );
        assert!(
            reloaded.auto_paused_at.is_none(),
            "an infra-caused timeout alone must never auto-pause the schedule"
        );
    }

    /// A real failure through ordinary `finalize` still counts toward auto-pause.
    #[tokio::test]
    async fn finalize_failed_still_counts_toward_auto_pause() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _env_guard = AUTO_PAUSE_ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_AUTO_PAUSE_THRESHOLD", "4");
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now(),
            auto_pause_threshold() - 1,
        )
        .await;
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    schedule_id: Some(schedule_id),
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );

        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Failed, Some("boom".to_string())),
        )
        .await
        .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.consecutive_failures, auto_pause_threshold());
        assert!(
            reloaded.auto_paused_at.is_some(),
            "a genuine failure crossing the threshold must still auto-pause"
        );
        std::env::remove_var("CONVEYOR_AUTO_PAUSE_THRESHOLD");
    }

    /// A successful finalize leaves `next_run_at` as `claim_and_plan` set it, not re-anchored on
    /// completion time.
    #[tokio::test]
    async fn interval_next_run_at_is_not_recomputed_on_a_successful_finalize() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let claim_time_next_run_at = Utc::now() + chrono::Duration::seconds(9000);
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            claim_time_next_run_at,
            0,
        )
        .await;
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    schedule_id: Some(schedule_id),
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );

        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Success, None),
        )
        .await
        .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let got = reloaded
            .next_run_at
            .map(|t| t.with_timezone(&Utc))
            .expect("next_run_at must still be set");
        assert_eq!(
            got.timestamp_millis(),
            claim_time_next_run_at.timestamp_millis(),
            "a successful finalize must not recompute next_run_at for an interval schedule"
        );
    }

    /// The failure/backoff path still recomputes `next_run_at` at finalize.
    #[tokio::test]
    async fn interval_next_run_at_is_recomputed_with_backoff_on_a_failed_finalize() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let claim_time_next_run_at = Utc::now() + chrono::Duration::seconds(9000);
        let schedule_id = insert_schedule(
            &db,
            "toy_ok",
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            claim_time_next_run_at,
            0,
        )
        .await;
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    schedule_id: Some(schedule_id),
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );

        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Failed, Some("boom".to_string())),
        )
        .await
        .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let got = reloaded
            .next_run_at
            .map(|t| t.with_timezone(&Utc))
            .expect("next_run_at must still be set");
        assert_ne!(
            got.timestamp_millis(),
            claim_time_next_run_at.timestamp_millis(),
            "a failed finalize must recompute next_run_at with backoff, not leave the claim-time value"
        );
    }

    #[tokio::test]
    async fn park_then_fan_out_resumes_the_waiter_through_the_queue() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "target",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let waiter_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "waiter",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );

        park(&db, waiter_id, target_id).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT waiting_on_task_id, lease_expires_at FROM conveyor_tasks WHERE id = $1",
                [waiter_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_task_id").unwrap();
        let lease: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "lease_expires_at").unwrap();
        assert_eq!(waiting_on, Some(target_id.0));
        assert!(lease.is_none(), "parking must clear the lease");

        finalize(
            &db,
            &valkey,
            target_id,
            Outcome::new(TaskState::Success, None),
        )
        .await
        .unwrap();

        let key = queue::queue_key("waiter");
        let popped = queue::blpop(&valkey, &key, 2.0)
            .await
            .unwrap()
            .expect("a resume envelope must have been enqueued for the waiter");
        assert_eq!(popped.task_id, waiter_id);
        assert!(popped.resume);
        let payload: ResumePayload = serde_json::from_value(popped.payload).unwrap();
        assert_eq!(payload.done_task_id, target_id);
        assert_eq!(payload.done_task_status, "success");

        let waiter_lease: Option<sea_orm::prelude::DateTimeWithTimeZone> = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT lease_expires_at FROM conveyor_tasks WHERE id = $1",
                [waiter_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "lease_expires_at")
            .unwrap();
        assert!(
            waiter_lease.is_some(),
            "a fanned-out waiter must carry a real lease again, so a crash before its resume \
             push reaches the queue leaves it reclaimable instead of stranded"
        );
    }

    /// A post-commit fan-out failure must not surface as an `Err` from a finalize that already
    /// committed.
    #[tokio::test]
    async fn finalize_still_returns_ok_when_the_post_commit_fan_out_push_fails() {
        let Some((db, _valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "target",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let waiter_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "waiter",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        park(&db, waiter_id, target_id).await.unwrap();

        // A Redis client on a dead port, built lazily, so the post-commit push fails.
        let dead_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let dead_valkey = dead_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");

        let result = finalize(
            &db,
            &dead_valkey,
            target_id,
            Outcome::new(TaskState::Success, None),
        )
        .await;
        assert!(
            result.is_ok(),
            "an already-committed finalize must not surface a best-effort fan-out failure as Err"
        );

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [target_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        assert_eq!(status, "success");
    }

    /// A claimed-but-unqueued waiter carries a lease, so `sweep_expired_leases` reclaims it rather
    /// than leaving it stranded.
    #[tokio::test]
    async fn a_waiter_whose_resume_push_fails_is_left_reclaimable_by_the_lease_sweep() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "target",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let waiter_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "waiter",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        park(&db, waiter_id, target_id).await.unwrap();

        // Same dead-Redis trick: the fan-out push fails but the claim UPDATE must have committed.
        let dead_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let dead_valkey = dead_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");

        finalize(
            &db,
            &dead_valkey,
            target_id,
            Outcome::new(TaskState::Success, None),
        )
        .await
        .unwrap();

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT waiting_on_task_id, lease_expires_at FROM conveyor_tasks WHERE id = $1",
                [waiter_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_task_id").unwrap();
        let lease: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "lease_expires_at").unwrap();
        assert_eq!(
            waiting_on, None,
            "the claim UPDATE must still have detached the waiter even though its push failed"
        );
        assert!(
            lease.is_some(),
            "a waiter left stranded by a failed resume push must still carry a lease — this is \
             what makes it reclaimable instead of stranded"
        );

        // Force-expire the lease and run the real sweep against live Redis to prove the waiter is
        // reclaimed.
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE conveyor_tasks SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
            [waiter_id.0.into()],
        ))
        .await
        .unwrap();
        crate::sweeper::sweep_expired_leases(&db, &valkey).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT status FROM conveyor_tasks WHERE id = $1",
                [waiter_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        assert_eq!(
            status, "failed",
            "the lease sweep must reclaim the stranded waiter once its lease expires"
        );
    }

    /// One failed push in a batch must not strand the other waiters; each still gets its own resume
    /// envelope.
    #[tokio::test]
    async fn one_failed_push_does_not_starve_the_rest_of_the_batch() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "target",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let waiter_a = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "waiter_a",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        let waiter_b = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "waiter_b",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        park(&db, waiter_a, target_id).await.unwrap();
        park(&db, waiter_b, target_id).await.unwrap();

        finalize(
            &db,
            &valkey,
            target_id,
            Outcome::new(TaskState::Success, None),
        )
        .await
        .unwrap();

        let popped_a = queue::blpop(&valkey, &queue::queue_key("waiter_a"), 2.0)
            .await
            .unwrap()
            .expect("waiter_a must have gotten its own resume envelope");
        let popped_b = queue::blpop(&valkey, &queue::queue_key("waiter_b"), 2.0)
            .await
            .unwrap()
            .expect("waiter_b must have gotten its own resume envelope");
        assert_eq!(popped_a.task_id, waiter_a);
        assert_eq!(popped_b.task_id, waiter_b);
    }

    /// `finalize` updates the Redis `job:<task_id>` record to the terminal status.
    #[tokio::test]
    async fn finalize_writes_the_terminal_redis_job_status() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_id = TaskId(
            insert_task_full(
                &db,
                InsertTask {
                    task_type: "toy_ok",
                    status: "running",
                    ..Default::default()
                },
            )
            .await,
        );
        crate::queue::set_job_status(&valkey, task_id, "running", None)
            .await
            .unwrap();

        finalize(
            &db,
            &valkey,
            task_id,
            Outcome::new(TaskState::Failed, Some("boom".to_string())),
        )
        .await
        .unwrap();

        let status = crate::queue::get_job_status(&valkey, task_id)
            .await
            .unwrap()
            .expect("a job status record must exist after finalize");
        assert_eq!(status.status, "failed");
        assert_eq!(status.error.as_deref(), Some("boom"));
    }
}
