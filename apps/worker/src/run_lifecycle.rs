//! Shared run-state machine: `pending -> running -> success|failed|timed_out|cancelled` (+
//! `skipped`). Every terminal transition funnels through [`finalize_run`], so bookkeeping happens
//! exactly once.

use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use redis::AsyncCommands;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, QueryFilter,
    Statement, TransactionTrait,
};
use uuid::Uuid;

use arcodash_core::entities::{query, run, schedule};
use arcodash_core::queue::{self, JobMessage, JobStatus, QueueName};
use arcodash_core::schedule_spec::{compute_next, ScheduleSpec};

use crate::handlers::execute_query::ExecuteQueryContext;

/// Stable per-process identity written to `runs.worker_id`; re-exported from
/// `arcodash_core::worker_identity`.
pub use arcodash_core::worker_identity::worker_id;

/// `RUN_TIMEOUT_SECS` — whole-run wall-clock cap.
pub fn run_timeout_secs() -> u64 {
    arcodash_core::config::env_parsed("RUN_TIMEOUT_SECS", 1800)
}

/// `RUN_LEASE_SECS` — how long a `running` row's lease lasts before the sweeper reclaims it.
pub fn run_lease_secs() -> i64 {
    arcodash_core::config::env_parsed("RUN_LEASE_SECS", 90)
}

/// `AUTO_PAUSE_THRESHOLD` — consecutive schedule failures before `auto_paused_at` is set.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the threshold is a small failure count; out-of-range env values keep `as` semantics"
)]
pub fn auto_pause_threshold() -> i32 {
    arcodash_core::config::env_parsed::<i64>("AUTO_PAUSE_THRESHOLD", 10) as i32
}

/// Claim a `pending` run via `pending->running` CAS; `Ok(false)` means it was already claimed, so
/// drop the job.
pub async fn start_run(db: &impl ConnectionTrait, run_id: i64) -> anyhow::Result<bool> {
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE runs \
             SET status = 'running', started_at = now(), worker_id = $1, \
                 lease_expires_at = now() + ($2 || ' seconds')::interval \
             WHERE id = $3 AND status = 'pending'",
            [
                worker_id().into(),
                run_lease_secs().to_string().into(),
                run_id.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Retries for a lost `start_run` CAS while the inserting transaction may not yet be visible; the
/// stale-pending sweeper is the backstop.
const CLAIM_VISIBILITY_RETRIES: usize = 3;
const CLAIM_VISIBILITY_DELAY: Duration = Duration::from_millis(300);

/// Claim a `pending` run, tolerating a not-yet-visible insert. `Ok(false)`: already claimed or
/// gone; drop the job. The CAS stays the sole arbiter.
pub async fn claim_pending_run(db: &impl ConnectionTrait, run_id: i64) -> anyhow::Result<bool> {
    for attempt in 0..=CLAIM_VISIBILITY_RETRIES {
        if start_run(db, run_id).await? {
            return Ok(true);
        }
        let status: Option<String> = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status FROM runs WHERE id = $1",
                [run_id.into()],
            ))
            .await?
            .map(|r| r.try_get::<String>("", "status"))
            .transpose()?;
        match status.as_deref() {
            None | Some("pending") if attempt < CLAIM_VISIBILITY_RETRIES => {
                tokio::time::sleep(CLAIM_VISIBILITY_DELAY).await;
            }
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// Spawn a heartbeat guard extending `lease_expires_at` every `run_lease_secs/3` while the run
/// stays `running`; dropping the guard stops renewal.
pub fn spawn_heartbeat(db: DatabaseConnection, run_id: i64) -> arcodash_conveyor::HeartbeatGuard {
    let period = Duration::from_secs((run_lease_secs() / 3).max(1).cast_unsigned());
    arcodash_conveyor::HeartbeatGuard::spawn(period, move || {
        let db = db.clone();
        async move {
            // `waiting_on_run_id IS NULL` stops a renewal from resurrecting a lease on a parked run
            // (governed by the waiting-run sweep). A 0-row match correctly reports the heartbeat as
            // lost.
            let res = db
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE runs SET lease_expires_at = now() + ($1 || ' seconds')::interval \
                     WHERE id = $2 AND status = 'running' AND waiting_on_run_id IS NULL",
                    [run_lease_secs().to_string().into(), run_id.into()],
                ))
                .await?;
            Ok(res.rows_affected())
        }
    })
}

/// The terminal status a run finalizes into. `Skipped` is normally born terminal in `plan_run`;
/// only `enqueue_run`'s never-published guard reaches it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Success,
    Failed,
    TimedOut,
    Cancelled,
    Skipped,
}

impl RunStatus {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    const fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }

    /// Redis job-status mapping; `Skipped` reports as `Cancelled` because the wire format has no
    /// skipped variant (the one lossy mapping).
    const fn as_job_status(self) -> JobStatus {
        match self {
            Self::Success => JobStatus::Success,
            Self::Failed | Self::TimedOut => JobStatus::Failure,
            Self::Cancelled | Self::Skipped => JobStatus::Cancelled,
        }
    }
}

#[derive(Clone)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub row_count: Option<i64>,
    pub error: Option<String>,
    /// Query runs: the `query_results` row produced. Monitor runs: the row evaluated.
    pub query_result_id: Option<i64>,
    /// Monitor runs only: age (seconds) of the evaluated data at check time.
    pub data_age_secs: Option<i64>,
    /// Monitor runs only: whether the checked condition was met.
    pub condition_met: Option<bool>,
}

impl RunOutcome {
    /// A terminal outcome carrying only a status and an optional error.
    pub const fn terminal(status: RunStatus, error: Option<String>) -> Self {
        Self {
            status,
            row_count: None,
            error,
            query_result_id: None,
            data_age_secs: None,
            condition_met: None,
        }
    }
}

/// `cas_statuses` is a literal `IN (...)` list, never caller data (Postgres can't bind it as one
/// parameter).
async fn finalize_run_matching(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    outcome: RunOutcome,
    cas_statuses: &'static str,
) -> anyhow::Result<bool> {
    let txn = ctx.db.begin().await?;

    let rows = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "UPDATE runs \
                 SET status = $1, finished_at = now(), lease_expires_at = NULL, \
                     duration_ms = CASE WHEN started_at IS NOT NULL \
                                        THEN EXTRACT(EPOCH FROM (now() - started_at)) * 1000 \
                                        ELSE NULL END, \
                     row_count = $2, error = $3, query_result_id = $4, data_age_secs = $5, \
                     condition_met = $6 \
                 WHERE id = $7 AND status IN {cas_statuses} \
                 RETURNING entity_type, entity_id, schedule_id, org_id, job_id"
            ),
            [
                outcome.status.as_db_str().into(),
                outcome.row_count.into(),
                outcome.error.clone().into(),
                outcome.query_result_id.into(),
                outcome.data_age_secs.into(),
                outcome.condition_met.into(),
                run_id.into(),
            ],
        ))
        .await?;

    let Some(row) = rows.into_iter().next() else {
        // Already finalized by another caller (handler vs sweeper race) — nothing left to do.
        txn.commit().await?;
        return Ok(false);
    };
    let entity_type: String = row.try_get("", "entity_type")?;
    let entity_id: i64 = row.try_get("", "entity_id")?;
    let org_id: i64 = row.try_get("", "org_id")?;
    let job_id: Option<String> = row.try_get("", "job_id")?;

    if let Some(schedule_id) = row.try_get::<Option<i64>>("", "schedule_id")? {
        apply_schedule_bookkeeping(&txn, schedule_id, outcome.status.is_failure()).await?;
    }

    prune_run_history(&txn, &entity_type, entity_id).await?;

    txn.commit().await?;

    // Mirror the terminal status to the Redis `job:<job_id>` record so polling clients see it even
    // when the handler never wrote STARTED. Best-effort: a Redis failure must not fail an
    // already-committed finalize.
    if let Some(job_id) = &job_id {
        // `None`: `set_job_status` carries forward the user_id from the STARTED write.
        if let Err(e) = queue::set_job_status(
            &ctx.redis,
            job_id,
            org_id,
            None,
            outcome.status.as_job_status(),
            outcome.query_result_id,
            outcome.error,
        )
        .await
        {
            tracing::error!(run_id, job_id, error = %e, "run_lifecycle: failed to write terminal job status to redis");
        }
    }

    // The coalesce key is per (entity_type, entity_id), not this run's schedule, so always check
    // it. Best-effort: the transaction already committed.
    if let Err(e) = plan_coalesce_followup(ctx, &entity_type, entity_id).await {
        tracing::error!(run_id, error = %e, "run_lifecycle: failed to plan the coalesced follow-up run");
    }

    // Wake monitor runs parked on a finished query run. Best-effort: `sweep_waiting_runs` recovers
    // them on failure.
    if entity_type == "query" {
        if let Err(e) = fan_out_waiters(ctx, run_id, outcome.status).await {
            tracing::error!(run_id, error = %e, "run_lifecycle: failed to fan out to waiting runs");
        }
    }

    if let Err(e) = arcodash_core::redis_util::publish_sched_wake(&ctx.redis).await {
        tracing::warn!(run_id, error = %e, "scheduler wake publish failed; the loop falls back to its poll cap");
    }

    Ok(true)
}

/// Terminal transition for one run. Idempotent: the UPDATE only matches `running`/`pending` rows;
/// returns `true` iff THIS call moved the row to terminal. Not fenced on `worker_id` (unlike
/// `delivery_lifecycle::finalize`): parked monitor runs and the lease sweeper legitimately finalize
/// runs they never leased.
pub async fn finalize_run(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    outcome: RunOutcome,
) -> anyhow::Result<bool> {
    finalize_run_matching(ctx, run_id, outcome, "('running', 'pending')").await
}

/// Like [`finalize_run`] but only matches a `pending` row, so the abandoned-pending sweep never
/// terminalizes a row a handler already claimed.
pub async fn finalize_pending_run(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    outcome: RunOutcome,
) -> anyhow::Result<bool> {
    finalize_run_matching(ctx, run_id, outcome, "('pending')").await
}

/// Bounded attempts for [`finalize_run_with_retry`], so a transient DB blip doesn't leave a decided
/// outcome for a later sweep to misread.
const FINALIZE_RETRY_ATTEMPTS: u32 = 3;
const FINALIZE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Retries [`finalize_run`] on DB-level error; propagates its `bool` (a zero-row match is not
/// retried).
pub async fn finalize_run_with_retry(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    outcome: RunOutcome,
) -> anyhow::Result<bool> {
    let mut attempt = 1;
    loop {
        match finalize_run(ctx, run_id, outcome.clone()).await {
            Ok(won) => return Ok(won),
            Err(e) => {
                tracing::warn!(run_id, attempt, error = %e, "run_lifecycle: finalize_run failed, retrying");
                if attempt == FINALIZE_RETRY_ATTEMPTS {
                    return Err(e);
                }
                tokio::time::sleep(FINALIZE_RETRY_DELAY).await;
                attempt += 1;
            }
        }
    }
}

/// Grace window for a detached waiter's resume to reach its finalize (queue wait plus evaluation).
const RESUME_GRACE_SECS: i64 = 600;

/// CAS the wait link to NULL, stamping a grace lease so a crashed resume is recovered as `failed`
/// by `sweep_expired_leases`. Returns true iff THIS caller cleared it; exactly one of inline check,
/// finalizer fan-out and sweeper wins per waiter.
pub async fn detach_cas(
    db: &impl ConnectionTrait,
    run_id: i64,
    target: i64,
) -> anyhow::Result<bool> {
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE runs SET waiting_on_run_id = NULL, \
                 lease_expires_at = now() + ($1 || ' seconds')::interval \
             WHERE id = $2 AND waiting_on_run_id = $3 AND status = 'running'",
            [
                RESUME_GRACE_SECS.to_string().into(),
                run_id.into(),
                target.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Wake every monitor run waiting on the just-finalized query run; the claim UPDATE clears the link
/// atomically so each waiter resumes once (contends with [`detach_cas`]). If the process dies
/// between the UPDATE and the enqueue, the grace lease lets `sweep_expired_leases` recover it;
/// don't reorder enqueue-before-clear (double-enqueues).
async fn fan_out_waiters(
    ctx: &ExecuteQueryContext,
    query_run_id: i64,
    status: RunStatus,
) -> anyhow::Result<()> {
    let claimed = ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE runs SET waiting_on_run_id = NULL, \
                 lease_expires_at = now() + ($1 || ' seconds')::interval \
             WHERE waiting_on_run_id = $2 AND status = 'running' \
             RETURNING id, org_id",
            [RESUME_GRACE_SECS.to_string().into(), query_run_id.into()],
        ))
        .await?;

    for row in claimed {
        let waiter_id: i64 = row.try_get("", "id")?;
        let org_id: i64 = row.try_get("", "org_id")?;
        ctx.queue
            .enqueue(
                QueueName::Default,
                JobMessage::ResumeMonitorEvaluation {
                    org_id,
                    run_id: waiter_id,
                    query_run_id,
                    query_run_status: status.as_db_str().to_string(),
                },
            )
            .await?;
    }
    Ok(())
}

/// Success resets the failure counter; failure/timeout increments it (auto-pausing at
/// `auto_pause_threshold()`) and recomputes `next_run_at` with backoff. On success an `Interval`
/// schedule keeps the `next_run_at` set by `claim_and_plan` to avoid cadence drift; `Cron`/`At`
/// recompute.
async fn apply_schedule_bookkeeping(
    txn: &impl ConnectionTrait,
    schedule_id: i64,
    failed: bool,
) -> anyhow::Result<()> {
    let Some(s) = schedule::Entity::find_by_id(schedule_id).one(txn).await? else {
        return Ok(());
    };
    let spec: ScheduleSpec = match serde_json::from_value(s.spec.clone()) {
        Ok(spec) => spec,
        Err(e) => {
            tracing::error!(schedule_id, error = %e, "run_lifecycle: unparseable spec at finalize, leaving next_run_at untouched");
            return Ok(());
        }
    };

    let consecutive_failures = if failed {
        s.consecutive_failures + 1
    } else {
        0
    };
    let next_run_at = if !failed && matches!(spec, ScheduleSpec::Interval { .. }) {
        None
    } else {
        compute_next(&spec, &s.timezone, Utc::now(), consecutive_failures, s.id.cast_unsigned())
            .inspect_err(|e| {
                tracing::error!(schedule_id, error = %e, "run_lifecycle: compute_next failed at finalize");
            })
            .ok()
    };
    let auto_pause = failed && consecutive_failures >= auto_pause_threshold();

    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE schedules \
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
        tracing::warn!(
            schedule_id,
            consecutive_failures,
            "run_lifecycle: schedule auto-paused after repeated failures"
        );
    }

    Ok(())
}

/// Keep only the newest 10 terminal runs per entity.
async fn prune_run_history(
    txn: &impl ConnectionTrait,
    entity_type: &str,
    entity_id: i64,
) -> anyhow::Result<()> {
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM runs WHERE entity_type = $1 AND entity_id = $2 \
         AND status NOT IN ('pending', 'running') \
         AND id NOT IN ( \
             SELECT id FROM runs WHERE entity_type = $1 AND entity_id = $2 \
             ORDER BY id DESC LIMIT 10 \
         )",
        [entity_type.into(), entity_id.into()],
    ))
    .await?;
    Ok(())
}

fn coalesce_key(entity_type: &str, entity_id: i64) -> String {
    format!("coalesce:{entity_type}:{entity_id}")
}

/// Atomically `GETDEL` the coalesced-tick key (set via `SETNX` in `plan_run`) so racing finalizers
/// can't both plan a follow-up.
async fn plan_coalesce_followup(
    ctx: &ExecuteQueryContext,
    entity_type: &str,
    entity_id: i64,
) -> anyhow::Result<()> {
    let mut conn = ctx.redis.clone();
    let Some(stored_tick) = conn
        .get_del::<_, Option<String>>(coalesce_key(entity_type, entity_id))
        .await?
    else {
        return Ok(());
    };
    let Ok(tick) = chrono::DateTime::parse_from_rfc3339(&stored_tick) else {
        tracing::warn!(entity_type, entity_id, %stored_tick, "run_lifecycle: unparseable coalesce tick, dropping");
        return Ok(());
    };
    let tick = tick.with_timezone(&Utc);

    // GETDEL already consumed the key, so an error past here permanently loses the follow-up; log
    // loudly, don't retry (re-writing would race the next tick's SETNX).
    plan_coalesce_followup_after_consume(ctx, entity_type, entity_id, tick)
        .await
        .inspect_err(|e| {
            tracing::error!(
                entity_type,
                entity_id,
                %tick,
                error = %e,
                "run_lifecycle: consumed a coalesced tick but failed to plan its follow-up run — tick lost"
            );
        })
}

async fn plan_coalesce_followup_after_consume(
    ctx: &ExecuteQueryContext,
    entity_type: &str,
    entity_id: i64,
    tick: chrono::DateTime<Utc>,
) -> anyhow::Result<()> {
    // Reuse the entity's schedule row so the follow-up looks like a normal scheduled run.
    let Some(s) = schedule::Entity::find()
        .filter(schedule::Column::EntityType.eq(entity_type))
        .filter(schedule::Column::EntityId.eq(entity_id))
        .one(&ctx.db)
        .await?
    else {
        return Ok(());
    };

    let data_source_id: Option<i64> = if entity_type == "query" {
        query::Entity::find_by_id(entity_id)
            .one(&ctx.db)
            .await?
            .map(|q| q.published_data_source_id.unwrap_or(q.data_source_id))
    } else {
        None
    };

    let job_id = Uuid::new_v4().to_string();
    let rows = ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, data_source_id, \
                               scheduled_for, triggered_by, status, job_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'schedule', 'pending', $7, now()) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
            [
                s.org_id.into(),
                entity_type.into(),
                entity_id.into(),
                s.id.into(),
                data_source_id.into(),
                tick.into(),
                job_id.into(),
            ],
        ))
        .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(());
    };
    let new_id: i64 = row.try_get("", "id")?;
    let Some(run) = run::Entity::find_by_id(new_id).one(&ctx.db).await? else {
        return Ok(());
    };

    crate::scheduler::deadline::try_enqueue_run(ctx, &run).await?;
    Ok(())
}

/// Enqueue a planned `runs` row: query runs via `enqueue_execute_query`, monitor runs to `Default`
/// as `EvaluateMonitor`. Returns whether the row landed on a queue (`false` if conveyor parked it
/// or it was skip-finalized); only `true` stamps `enqueued_at`.
pub async fn enqueue_run(ctx: &ExecuteQueryContext, run: &run::Model) -> anyhow::Result<bool> {
    let job_id = run
        .job_id
        .clone()
        .context("run row has no job_id at enqueue time")?;

    match run.entity_type.as_str() {
        "query" => {
            let q = query::Entity::find_by_id(run.entity_id)
                .one(&ctx.db)
                .await?
                .with_context(|| format!("query {} not found for run {}", run.entity_id, run.id))?;

            // Once a query has ever been published its frozen published text runs; only
            // never-published queries run their draft.
            let (data_source_id, query_text, repoint_published) = if q.published_change_id.is_some()
            {
                (
                    q.published_data_source_id.unwrap_or(q.data_source_id),
                    q.published_query.clone().unwrap_or_else(|| q.query.clone()),
                    q.unpublished_at.is_none(),
                )
            } else if run.triggered_by == "manual" {
                (q.data_source_id, q.query.clone(), false)
            } else {
                // Scheduled tick on a never-published query is a defensive path: finalize as a skip
                // rather than leak draft SQL. Boxed to break the finalize_run <-> enqueue_run
                // recursive future.
                Box::pin(finalize_run(
                    ctx,
                    run.id,
                    RunOutcome::terminal(
                        RunStatus::Skipped,
                        Some("query has no published version to run".to_string()),
                    ),
                ))
                .await?;
                return Ok(false);
            };

            queue::set_job_status(
                &ctx.redis,
                &job_id,
                run.org_id,
                Some(q.user_id),
                JobStatus::Pending,
                None,
                None,
            )
            .await?;
            let queued = arcodash_core::query_exec::enqueue_execute_query(
                &ctx.db,
                ctx.queue.as_ref(),
                ctx.conveyor.get().map(std::convert::AsRef::as_ref),
                arcodash_core::query_exec::ExecuteQueryPayload {
                    job_id,
                    org_id: run.org_id,
                    data_source_id,
                    query_text,
                    parameters: serde_json::Value::Null,
                    user_id: q.user_id,
                    query_id: Some(q.id),
                    scheduled: true,
                    run_id: Some(run.id),
                    repoint_published,
                    max_rows: None,
                },
            )
            .await?;
            if queued {
                stamp_enqueued(ctx, run.id).await?;
            }
            Ok(queued)
        }
        "monitor" => {
            ctx.queue
                .enqueue(
                    QueueName::Default,
                    JobMessage::EvaluateMonitor {
                        org_id: run.org_id,
                        monitor_id: run.entity_id,
                        run_id: Some(run.id),
                    },
                )
                .await?;
            stamp_enqueued(ctx, run.id).await?;
            Ok(true)
        }
        other => anyhow::bail!("unknown entity_type {other:?} on run {}", run.id),
    }
}

/// Mark a row as actually queued; the concurrency cap counts it alongside `running`. `COALESCE`
/// keeps the first-pushed timestamp.
async fn stamp_enqueued(ctx: &ExecuteQueryContext, run_id: i64) -> anyhow::Result<()> {
    ctx.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE runs SET enqueued_at = COALESCE(enqueued_at, now()) WHERE id = $1",
            [run_id.into()],
        ))
        .await?;
    Ok(())
}

#[cfg(test)]
mod job_status_mapping_tests {
    //! Pure tests for `RunStatus::as_job_status`.
    use super::*;

    #[test]
    fn maps_every_run_status_to_its_job_status() {
        assert_eq!(RunStatus::Success.as_job_status(), JobStatus::Success);
        assert_eq!(RunStatus::Failed.as_job_status(), JobStatus::Failure);
        assert_eq!(RunStatus::TimedOut.as_job_status(), JobStatus::Failure);
        assert_eq!(RunStatus::Cancelled.as_job_status(), JobStatus::Cancelled);
        assert_eq!(
            RunStatus::Skipped.as_job_status(),
            JobStatus::Cancelled,
            "Skipped has no dedicated JobStatus variant — it must map to Cancelled, the closest \
             \"no result, not an error\" status, not silently fall back to something misleading"
        );
    }
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::test_support::setup;
    use crate::scheduler::test_support::*;
    use redis::AsyncCommands;
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;
    use sea_orm::Statement;

    /// Matches on job_id, not run_id, which is reused across test runs.
    async fn find_and_remove_execute_query_for_job(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        job_id: &str,
    ) -> Option<queue::JobMessage> {
        find_and_remove_job(redis, key, |job| {
            matches!(job, queue::JobMessage::ExecuteQuery { job_id: jid, .. } if jid == job_id)
        })
        .await
    }

    async fn find_and_remove_resume_for_waiter(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        waiter_id: i64,
    ) -> Option<queue::JobMessage> {
        find_and_remove_job(redis, key, |job| {
            matches!(job, queue::JobMessage::ResumeMonitorEvaluation { run_id, .. } if *run_id == waiter_id)
        })
        .await
    }

    fn success(row_count: i64) -> RunOutcome {
        RunOutcome {
            row_count: Some(row_count),
            ..RunOutcome::terminal(RunStatus::Success, None)
        }
    }

    /// Seeds an org, data source and draft query plus one query run in `status`.
    async fn seed_query_run(
        db: &DatabaseConnection,
        label: &str,
        status: &str,
    ) -> (query::Model, i64) {
        let org = seed_org(db, &format!("org-{label}-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(db, org.id).await;
        let q = seed_query(db, org.id, ds.id).await;
        let run_id = seed_run(db, org.id, "query", q.id, None, Some(ds.id), status, None).await;
        (q, run_id)
    }

    /// Seeds a running query run on a published query plus `waiters` monitor runs parked on it, each
    /// on its own monitor (`uq_runs_inflight` allows one `running` row per entity). Returns the
    /// query, the query run id and the waiter run ids.
    async fn seed_parked_waiters(
        db: &DatabaseConnection,
        label: &str,
        waiters: usize,
    ) -> (query::Model, i64, Vec<i64>) {
        let org = seed_org(db, &format!("org-{label}-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(db, org.id).await;
        let q = seed_published_query(db, org.id, ds.id).await;
        let qrun_id = seed_run(
            db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "running",
            None,
        )
        .await;
        let mut waiter_ids = Vec::with_capacity(waiters);
        for _ in 0..waiters {
            let mon = seed_monitor(
                db,
                org.id,
                q.id,
                q.user_id,
                serde_json::json!({"column": "value", "op": ">", "value": 50.0}),
            )
            .await;
            let waiter_id =
                seed_run(db, org.id, "monitor", mon.id, None, None, "running", None).await;
            exec_sql(
                db,
                "UPDATE runs SET waiting_on_run_id = $1 WHERE id = $2",
                [qrun_id.into(), waiter_id.into()],
            )
            .await;
            waiter_ids.push(waiter_id);
        }
        (q, qrun_id, waiter_ids)
    }

    async fn set_job_id(db: &DatabaseConnection, run_id: i64, job_id: &str) {
        exec_sql(
            db,
            "UPDATE runs SET job_id = $1 WHERE id = $2",
            [job_id.into(), run_id.into()],
        )
        .await;
    }

    #[tokio::test]
    async fn finalize_run_prunes_history_down_to_the_last_10() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-prune-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;

        for _ in 0..12 {
            exec_sql(
                &db,
                "INSERT INTO runs (org_id, entity_type, entity_id, triggered_by, status, created_at) \
                 VALUES ($1, 'query', $2, 'manual', 'success', now())",
                [org.id.into(), q.id.into()],
            )
            .await;
        }
        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "running",
            None,
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(&ctx, run_id, success(0)).await.unwrap();

        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM runs WHERE entity_type='query' AND entity_id=$1",
                [q.id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            10,
            "history must be pruned to the newest 10 runs"
        );
    }

    /// A never-published query's run is finalized as `skipped`, not enqueued.
    #[tokio::test]
    async fn enqueue_run_skips_and_finalizes_never_published_query_as_no_op() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, run_id) = seed_query_run(&db, "enqrun-draft", "pending").await;
        let job_id = uuid::Uuid::new_v4().to_string();
        set_job_id(&db, run_id, &job_id).await;
        // Needs a non-manual run (seed_run defaults to manual).
        exec_sql(
            &db,
            "UPDATE runs SET triggered_by = 'schedule' WHERE id = $1",
            [run_id.into()],
        )
        .await;

        let key = queue::QueueName::ScheduledQueries.queue_key();

        let ctx = test_ctx(db.clone(), redis.clone());
        let run = load_run(&db, run_id).await;
        enqueue_run(&ctx, &run).await.unwrap();

        assert!(
            find_and_remove_execute_query_for_job(&redis, &key, &job_id)
                .await
                .is_none(),
            "a never-published query must not be enqueued by the deadline engine"
        );

        let reloaded = load_run(&db, run_id).await;
        assert_eq!(
            reloaded.status, "skipped",
            "the run must be finalized (not left pending) so it doesn't wedge the deadline loop"
        );
        assert_eq!(
            reloaded.error.as_deref(),
            Some("query has no published version to run"),
            "the skip reason should be recorded on the finalized run"
        );
    }

    /// A published query with a pending draft edit enqueues the PUBLISHED text with
    /// `repoint_published: true`.
    #[tokio::test]
    async fn enqueue_run_runs_published_content_with_repoint_published_true() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-enqrun-pub-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;

        let mut active: query::ActiveModel = q.clone().into();
        active.query = Set("SELECT 'draft-edit'".to_string());
        active.update(&db).await.unwrap();

        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "pending",
            None,
        )
        .await;
        let job_id = uuid::Uuid::new_v4().to_string();
        set_job_id(&db, run_id, &job_id).await;

        let key = queue::QueueName::ScheduledQueries.queue_key();

        let ctx = test_ctx(db.clone(), redis.clone());
        let run = load_run(&db, run_id).await;
        enqueue_run(&ctx, &run).await.unwrap();

        let job = find_and_remove_execute_query_for_job(&redis, &key, &job_id)
            .await
            .expect("expected an enqueued job");
        match job {
            queue::JobMessage::ExecuteQuery {
                query_text,
                repoint_published,
                ..
            } => {
                assert_eq!(
                    query_text, "SELECT 1",
                    "must enqueue the published text, not the pending draft edit"
                );
                assert!(
                    repoint_published,
                    "a published query's deadline-engine run must repoint the published pointer"
                );
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    /// A manual run on a never-published query runs the draft's live content.
    #[tokio::test]
    async fn enqueue_run_runs_draft_content_for_a_manual_run_on_an_unpublished_query() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-enqrun-manual-draft-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;

        let mut active: query::ActiveModel = q.clone().into();
        active.query = Set("SELECT 'draft-only'".to_string());
        active.update(&db).await.unwrap();

        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "pending",
            None,
        )
        .await;
        let job_id = uuid::Uuid::new_v4().to_string();
        set_job_id(&db, run_id, &job_id).await;

        let key = queue::QueueName::ScheduledQueries.queue_key();

        let ctx = test_ctx(db.clone(), redis.clone());
        let run = load_run(&db, run_id).await;
        enqueue_run(&ctx, &run).await.unwrap();

        let job = find_and_remove_execute_query_for_job(&redis, &key, &job_id)
            .await
            .expect("a manual run on an unpublished draft must still be enqueued");
        match job {
            queue::JobMessage::ExecuteQuery {
                query_text,
                repoint_published,
                ..
            } => {
                assert_eq!(
                    query_text, "SELECT 'draft-only'",
                    "must enqueue the draft's live content"
                );
                assert!(
                    !repoint_published,
                    "there is no published pointer to repoint on an unpublished draft"
                );
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }

        let reloaded = load_run(&db, run_id).await;
        assert_ne!(
            reloaded.status, "skipped",
            "a manual run on an unpublished draft must not be skipped"
        );
    }

    /// A manual run on a published-then-unpublished query runs the frozen published text, not the
    /// draft.
    #[tokio::test]
    async fn enqueue_run_runs_frozen_published_content_for_a_manual_run_on_an_unpublished_query() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-enqrun-manual-unpub-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;

        exec_sql(
            &db,
            "UPDATE queries SET unpublished_at = now(), query = $1 WHERE id = $2",
            ["SELECT 'draft-edit-after-unpublish'".into(), q.id.into()],
        )
        .await;

        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "pending",
            None,
        )
        .await;
        let job_id = uuid::Uuid::new_v4().to_string();
        set_job_id(&db, run_id, &job_id).await;

        let key = queue::QueueName::ScheduledQueries.queue_key();

        let ctx = test_ctx(db.clone(), redis.clone());
        let run = load_run(&db, run_id).await;
        enqueue_run(&ctx, &run).await.unwrap();

        let job = find_and_remove_execute_query_for_job(&redis, &key, &job_id)
            .await
            .expect("a manual run on an unpublished (previously-published) query must be enqueued");
        match job {
            queue::JobMessage::ExecuteQuery {
                query_text,
                repoint_published,
                ..
            } => {
                assert_eq!(
                    query_text, "SELECT 1",
                    "must enqueue the frozen published text, not the post-unpublish draft edit"
                );
                assert!(
                    !repoint_published,
                    "the published pointer must not move — the query is no longer live"
                );
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    /// Finalizer fan-out clears and resumes every monitor run parked on the finalized query run.
    #[tokio::test]
    async fn finalize_run_fans_out_to_every_waiter_parked_on_it() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, qrun_id, waiter_ids) = seed_parked_waiters(&db, "fanout", 5).await;

        let key = queue::QueueName::Default.queue_key();

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(&ctx, qrun_id, success(1)).await.unwrap();

        for waiter_id in &waiter_ids {
            let row = query_row(
                &db,
                "SELECT waiting_on_run_id FROM runs WHERE id = $1",
                [(*waiter_id).into()],
            )
            .await;
            let waiting_on: Option<i64> = row.try_get("", "waiting_on_run_id").unwrap();
            assert_eq!(waiting_on, None, "every waiter must be cleared");
        }

        for waiter_id in &waiter_ids {
            let job = find_and_remove_resume_for_waiter(&redis, &key, *waiter_id)
                .await
                .unwrap_or_else(|| {
                    panic!("expected a ResumeMonitorEvaluation for waiter {waiter_id}")
                });
            match job {
                queue::JobMessage::ResumeMonitorEvaluation { query_run_id, .. } => {
                    assert_eq!(
                        query_run_id, qrun_id,
                        "every resume must carry the query run id"
                    );
                }
                other => panic!("expected ResumeMonitorEvaluation, got {other:?}"),
            }
            assert!(
                find_and_remove_resume_for_waiter(&redis, &key, *waiter_id)
                    .await
                    .is_none(),
                "exactly one resume for waiter {waiter_id}, no more"
            );
        }
    }

    async fn lease_expires_at(
        db: &DatabaseConnection,
        run_id: i64,
    ) -> Option<sea_orm::prelude::DateTimeWithTimeZone> {
        query_row(
            db,
            "SELECT lease_expires_at FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await
        .try_get("", "lease_expires_at")
        .unwrap()
    }

    /// Dropping the `HeartbeatGuard` must stop renewal for good.
    #[tokio::test]
    async fn heartbeat_guard_drop_stops_the_renewal_without_an_explicit_abort() {
        let Some((db, _redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, run_id) = seed_query_run(&db, "run-heartbeat", "running").await;

        // Shrinks the heartbeat period; process-global env var, restored below (not parallel-safe).
        std::env::set_var("RUN_LEASE_SECS", "3");
        let heartbeat = spawn_heartbeat(db.clone(), run_id);

        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        drop(heartbeat);

        let lease_at_drop = lease_expires_at(&db, run_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
        let lease_well_after_drop = lease_expires_at(&db, run_id).await;
        std::env::remove_var("RUN_LEASE_SECS");

        assert_eq!(
            lease_at_drop, lease_well_after_drop,
            "lease_expires_at must not advance after the guard is dropped — a still-advancing \
             lease means the heartbeat task kept running instead of being aborted"
        );
    }

    /// `HeartbeatGuard::lost()` must flip once the lease is gone (0-row renew).
    #[tokio::test]
    async fn heartbeat_guard_reports_lost_once_the_run_is_finalized_out_from_under_it() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, run_id) = seed_query_run(&db, "run-heartbeat-lost", "running").await;

        std::env::set_var("RUN_LEASE_SECS", "3");
        let heartbeat = spawn_heartbeat(db.clone(), run_id);
        assert!(
            !heartbeat.lost(),
            "a freshly claimed run's heartbeat must not start out lost"
        );

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(&ctx, run_id, success(0)).await.unwrap();

        let start = std::time::Instant::now();
        while !heartbeat.lost() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "heartbeat.lost() must flip once the next renewal tick observes 0 rows affected"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::env::remove_var("RUN_LEASE_SECS");
    }

    /// `finalize_run` writes the terminal status to the Redis `job:<job_id>` record, not just the
    /// `runs` row.
    #[tokio::test]
    async fn finalize_run_writes_the_terminal_redis_job_status() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-finalize-redis-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let qr = seed_query_result(
            &db,
            org.id,
            ds.id,
            chrono::Utc::now(),
            serde_json::json!({}),
        )
        .await;
        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            None,
            Some(ds.id),
            "running",
            None,
        )
        .await;
        let job_id = format!("finalize-redis-{suffix}");
        set_job_id(&db, run_id, &job_id).await;
        // Handler writes STARTED first; prove finalize overwrites it.
        queue::set_job_status(
            &redis,
            &job_id,
            org.id,
            None,
            JobStatus::Started,
            None,
            None,
        )
        .await
        .unwrap();

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(
            &ctx,
            run_id,
            RunOutcome {
                status: RunStatus::Success,
                row_count: Some(3),
                error: None,
                query_result_id: Some(qr.id),
                data_age_secs: None,
                condition_met: None,
            },
        )
        .await
        .unwrap();

        let status = queue::get_job_status(&redis, &job_id)
            .await
            .unwrap()
            .expect("a job status record must exist after finalize");
        assert_eq!(status.status, JobStatus::Success);
        assert_eq!(status.query_result_id, Some(qr.id));
        assert_eq!(status.org_id, org.id);
    }

    /// Gate finalize from `pending` (no STARTED write) must still land the Redis record.
    #[tokio::test]
    async fn finalize_run_writes_redis_status_even_when_no_started_write_ever_happened() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (_q, run_id) = seed_query_run(&db, "finalize-redis-skip", "pending").await;
        let job_id = format!("finalize-redis-skip-{suffix}");
        set_job_id(&db, run_id, &job_id).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(
            &ctx,
            run_id,
            RunOutcome::terminal(
                RunStatus::Skipped,
                Some("blocked by entitlements".to_string()),
            ),
        )
        .await
        .unwrap();

        let status = queue::get_job_status(&redis, &job_id)
            .await
            .unwrap()
            .expect("a job status record must exist even though STARTED was never written");
        assert_eq!(
            status.status,
            JobStatus::Cancelled,
            "Skipped's lossy mapping to Cancelled (the closest non-error terminal status)"
        );
    }

    /// The first finalize returns `true`; a racing second with a different outcome returns `false`
    /// and leaves the first outcome.
    #[tokio::test]
    async fn finalize_run_reports_whether_its_own_cas_won() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, run_id) = seed_query_run(&db, "finalize-cas", "running").await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let won = finalize_run(&ctx, run_id, success(1)).await.unwrap();
        assert!(won, "the first finalize on a running run must win the CAS");

        let won_again = finalize_run(
            &ctx,
            run_id,
            RunOutcome::terminal(RunStatus::Failed, Some("must never land".to_string())),
        )
        .await
        .unwrap();
        assert!(
            !won_again,
            "a second finalize on an already-terminal run must lose the CAS"
        );

        let row = query_row(
            &db,
            "SELECT status, error FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await;
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(
            status, "success",
            "the row must still carry the winning finalize's outcome"
        );
        assert_eq!(
            error, None,
            "the losing finalize's error must never have landed"
        );
    }

    /// A monitor run's heartbeat keeps its lease alive; once parked, the next renewal must leave
    /// the lease NULL.
    #[tokio::test]
    async fn heartbeat_protects_a_running_monitor_run_and_never_resurrects_a_parked_lease() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-monitor-heartbeat-{suffix}")).await;
        // No real monitor row needed; `org.id` as entity id avoids `uq_runs_inflight` collisions.
        let run_id = seed_run(&db, org.id, "monitor", org.id, None, None, "running", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET started_at = now(), \
             lease_expires_at = now() + interval '3 seconds' WHERE id = $1",
            [run_id.into()],
        )
        .await;

        // RUN_LEASE_SECS=3 gives a 1s renewal period.
        std::env::set_var("RUN_LEASE_SECS", "3");
        let heartbeat = spawn_heartbeat(db.clone(), run_id);

        tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        crate::scheduler::sweeper::sweep_expired_leases(&ctx).await;

        let status_after_sweep = run_status(&db, run_id).await;
        assert_eq!(
            status_after_sweep, "running",
            "a live heartbeat must keep renewing the lease so the sweeper never reclaims it"
        );

        // Park exactly as `attach_or_trigger_and_park` does: clear the lease, set the wait link.
        exec_sql(
            &db,
            "UPDATE runs SET waiting_on_run_id = $1, lease_expires_at = NULL WHERE id = $2",
            [999_999_999i64.into(), run_id.into()],
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let lease_after_park = lease_expires_at(&db, run_id).await;
        std::env::remove_var("RUN_LEASE_SECS");
        drop(heartbeat);

        assert!(
            lease_after_park.is_none(),
            "a heartbeat renewal firing after the run is parked must never resurrect \
             lease_expires_at — the parked row holds no lease by construction"
        );
    }

    /// Forces a real Redis error in the coalesce `GETDEL` by leaving a list under the key
    /// (WRONGTYPE).
    async fn poison_coalesce_key(
        redis: &redis::aio::ConnectionManager,
        entity_type: &str,
        entity_id: i64,
    ) {
        let mut conn = redis.clone();
        let key = coalesce_key(entity_type, entity_id);
        let _: () = conn.del(&key).await.unwrap();
        let _: i64 = conn.rpush(&key, "not-a-string-value").await.unwrap();
    }

    /// A `GETDEL` failure in `plan_coalesce_followup` must not abort the waiter fan-out.
    #[tokio::test]
    async fn a_coalesce_lookup_failure_does_not_block_the_waiter_fan_out() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (q, qrun_id, waiters) = seed_parked_waiters(&db, "coalesce-fail", 1).await;
        let waiter_id = waiters[0];

        poison_coalesce_key(&redis, "query", q.id).await;

        let key = queue::QueueName::Default.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());
        let result = finalize_run(&ctx, qrun_id, success(1)).await;

        assert!(
            result.unwrap(),
            "finalize_run must still report success once the transaction committed, even though \
             the coalesce lookup failed"
        );

        let row = query_row(
            &db,
            "SELECT waiting_on_run_id FROM runs WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_run_id").unwrap();
        assert_eq!(
            waiting_on, None,
            "the waiter must still be detached despite the coalesce failure"
        );
        let job = find_and_remove_resume_for_waiter(&redis, &key, waiter_id)
            .await
            .expect(
                "a ResumeMonitorEvaluation must still be enqueued despite the coalesce failure",
            );
        assert!(matches!(
            job,
            queue::JobMessage::ResumeMonitorEvaluation { .. }
        ));

        let mut conn = redis.clone();
        let _: () = conn.del(coalesce_key("query", q.id)).await.unwrap();
    }

    /// `finalize_run` stays `Ok` when best-effort post-commit Redis work fails.
    #[tokio::test]
    async fn finalize_run_returns_ok_once_the_transaction_commits() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (q, run_id) = seed_query_run(&db, "finalize-ok", "running").await;

        poison_coalesce_key(&redis, "query", q.id).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let result = finalize_run(&ctx, run_id, success(0)).await;
        assert!(
            result.is_ok(),
            "a committed finalize must return Ok even when post-commit Redis work fails: {result:?}"
        );
        assert!(result.unwrap());

        let mut conn = redis.clone();
        let _: () = conn.del(coalesce_key("query", q.id)).await.unwrap();
    }

    /// A successful `Interval` run leaves `next_run_at` as `claim_and_plan` set it.
    #[tokio::test]
    async fn finalize_run_success_does_not_recompute_next_run_at_for_an_interval_schedule() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-interval-nodrift-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        // An arbitrary instant that is not now + every_secs.
        let claim_time_next_run_at = Utc::now() + chrono::Duration::seconds(9000);
        let sched = seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind": "interval", "every_secs": 300}),
            "skip",
            claim_time_next_run_at,
            0,
        )
        .await;
        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            Some(sched.id),
            Some(ds.id),
            "running",
            None,
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        finalize_run(&ctx, run_id, success(1)).await.unwrap();

        let reloaded = schedule::Entity::find_by_id(sched.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let got = reloaded
            .next_run_at
            .map(|t| t.with_timezone(&Utc))
            .expect("next_run_at must still be set");
        assert!(
            (got - claim_time_next_run_at).num_milliseconds().abs() < 1000,
            "a successful Interval finalize must leave next_run_at untouched: expected \
             ~{claim_time_next_run_at}, got {got}"
        );
    }

    async fn lease_and_link(
        db: &DatabaseConnection,
        run_id: i64,
    ) -> (Option<i64>, Option<sea_orm::prelude::DateTimeWithTimeZone>) {
        let row = query_row(
            db,
            "SELECT waiting_on_run_id, lease_expires_at FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await;
        (
            row.try_get("", "waiting_on_run_id").unwrap(),
            row.try_get("", "lease_expires_at").unwrap(),
        )
    }

    /// Winning CAS clears the link and stamps a grace lease; a losing CAS leaves the row untouched.
    #[tokio::test]
    async fn detach_cas_stamps_a_grace_lease_only_on_the_winning_call() {
        let Some((db, _redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (_q, target_id, waiters) = seed_parked_waiters(&db, "detach-cas", 1).await;
        let waiter_id = waiters[0];

        let lost = detach_cas(&db, waiter_id, target_id + 1).await.unwrap();
        assert!(!lost, "a CAS against the wrong target must not win");
        let (waiting_on, lease) = lease_and_link(&db, waiter_id).await;
        assert_eq!(
            waiting_on,
            Some(target_id),
            "a losing CAS must leave the row untouched"
        );
        assert_eq!(lease, None);

        let won = detach_cas(&db, waiter_id, target_id).await.unwrap();
        assert!(won);
        let (waiting_on, lease) = lease_and_link(&db, waiter_id).await;
        assert_eq!(waiting_on, None);
        assert!(
            lease.is_some_and(|t| t.with_timezone(&Utc) > Utc::now()),
            "the winning CAS must stamp a future grace lease"
        );
    }

    /// `fan_out_waiters` stamps a grace lease on every waiter it claims.
    #[tokio::test]
    async fn fan_out_waiters_stamps_a_grace_lease_on_every_claimed_waiter() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Distinct monitors: each waiter needs its own inflight slot.
        let (_q, target_id, waiters) = seed_parked_waiters(&db, "fanout-lease", 2).await;
        let (waiter_a, waiter_b) = (waiters[0], waiters[1]);

        let key = QueueName::Default.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());
        fan_out_waiters(&ctx, target_id, RunStatus::Success)
            .await
            .unwrap();

        for waiter_id in [waiter_a, waiter_b] {
            let (waiting_on, lease) = lease_and_link(&db, waiter_id).await;
            assert_eq!(waiting_on, None);
            assert!(
                lease.is_some_and(|t| t.with_timezone(&Utc) > Utc::now()),
                "every claimed waiter must get its own future grace lease"
            );
            let job = find_and_remove_resume_for_waiter(&redis, &key, waiter_id)
                .await
                .expect("a resume must be enqueued for each claimed waiter");
            assert!(matches!(
                job,
                queue::JobMessage::ResumeMonitorEvaluation { .. }
            ));
        }
    }
}
