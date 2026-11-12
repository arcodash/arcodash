//! `execute_query` handler. Results are written as gzip JSON objects to the organization's storage
//! (`query_results.data_location` points at them); connectors that opt into auto-limit get `LIMIT max_rows + 1` appended and the sentinel row is trimmed and flagged as
//! `data.truncated`.
//!
//! Cancellation: the Redis flag `job:<id>:cancel` is checked before start and polled while the
//! connector runs; observing it drops the connector future and persists nothing. The target DB is
//! not told to stop, and a cancel racing a terminal write may lose to it.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::Utc;
use redis::aio::ConnectionManager;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Statement, TransactionTrait,
};
use serde_json::Value;

use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::connectors::{classify_run_query_error, format_error_chain, ConnectorErrorKind};
#[cfg(test)]
use arcodash_core::entities::query;
use arcodash_core::entities::{data_source, query_result};
use arcodash_core::entitlements::WorkGate;
use arcodash_core::events::record_event;
use arcodash_core::hash::gen_query_hash;
use arcodash_core::mailer::Mailer;
use arcodash_core::query_exec::ExecuteQueryPayload;
use arcodash_core::queue::{self, JobStatus, Queue};
use arcodash_core::result_store::save_result;
use arcodash_core::secrets;

use crate::run_lifecycle::{self, RunOutcome, RunStatus};

pub use arcodash_core::mailer::{build_mailer, send_email, send_html_email};

/// Everything `handle` needs that isn't part of the `JobMessage::ExecuteQuery` payload itself.
#[derive(Clone)]
pub struct ExecuteQueryContext {
    pub db: DatabaseConnection,
    pub redis: ConnectionManager,
    pub connectors: Arc<ConnectorRegistry>,
    /// Shared with `ConnectorRegistry::with_db` so the orphan-upload sweep reuses the client.
    pub store: Arc<arcodash_core::storage::ObjectStore>,
    pub encryption_key: [u8; 32],
    pub queue: Arc<dyn Queue>,
    pub mailer: Option<Arc<Mailer>>,
    pub mail_from: String,
    /// Frontend origin; used for the digest email's unsubscribe link.
    pub base_url: String,
    /// `QUERY_TIMEOUT_SECS` — hard cap on how long a single `connector.run_query` call may run.
    pub query_timeout_secs: u64,
    /// `MAX_QUERY_ROWS` — auto-`LIMIT` cap applied to unbounded `SELECT`/`WITH` queries.
    pub max_query_rows: u32,
    /// Re-enqueues notification retries via the conveyor when active; `OnceLock` because the
    /// conveyor's handler holds this same ctx, so it can only be set afterwards.
    pub conveyor: std::sync::OnceLock<Arc<arcodash_conveyor::Conveyor>>,
    /// Provider runtime for AI jobs; `None` leaves them as no-ops.
    pub ai: Option<Arc<crate::ai_sink::AiRuntime>>,
    /// Per-org gating of background work and AI features.
    pub entitlements: Arc<dyn arcodash_core::entitlements::OrgEntitlements>,
    /// Resolves an organization's object storage.
    pub storage: Arc<dyn arcodash_core::org_storage::OrgStorageResolver>,
}

/// Run the `execute_query` flow end to end, writing job status to Redis at each step. Errors returned
/// are only those from before a job-status write was possible; everything else becomes a FAILURE
/// status.
///
/// `job.run_id` is set when driven by a `runs` row (deadline scheduler engine); `repoint_published`
/// moves `queries.published_latest_query_data_id` instead of `latest_query_data_id` (never both);
/// `max_rows` is clamped to `ExecuteQueryContext::max_query_rows`.
pub async fn handle(ctx: &ExecuteQueryContext, job: ExecuteQueryPayload) {
    if let Err(e) = handle_inner(ctx, &job).await {
        tracing::error!(job_id = %job.job_id, error = %e, "execute_query job failed before a terminal status could be recorded");
    }
}

/// Distinguishes a whole-run timeout (`TimedOut`) from other failures (`Failed`) for
/// `finalize_run`.
enum RunFailure {
    Timeout(u64),
    Error(anyhow::Error),
}

/// Whether `handle_inner` finished normally or was cancelled (conveyor reports
/// `TaskOutcome::Cancelled`).
enum HandleOutcome {
    Completed,
    Cancelled,
}

async fn handle_inner(
    ctx: &ExecuteQueryContext,
    job: &ExecuteQueryPayload,
) -> anyhow::Result<HandleOutcome> {
    if let WorkGate::Blocked { reason } = ctx.entitlements.scheduled_work(&ctx.db, job.org_id).await
    {
        tracing::info!(org_id = job.org_id, reason, "skipping scheduled work");
        // A run-backed execution must not stay `pending` while the org is blocked (it would wedge
        // `uq_runs_inflight`); `finalize_run` accepts `pending` directly.
        if let Some(run_id) = job.run_id {
            run_lifecycle::finalize_run(
                ctx,
                run_id,
                RunOutcome::terminal(RunStatus::Skipped, Some(reason.to_string())),
            )
            .await?;
        }
        return Ok(HandleOutcome::Completed);
    }

    let pause_refusal = data_source::Entity::find_by_id(job.data_source_id)
        .one(&ctx.db)
        .await?
        .and_then(|ds| ds.pause_refusal());
    if let Some(msg) = pause_refusal {
        // Skipped, not Failed: a long pause must not drive schedules into auto-pause via
        // consecutive_failures.
        if let Some(run_id) = job.run_id {
            run_lifecycle::finalize_run(
                ctx,
                run_id,
                RunOutcome::terminal(RunStatus::Skipped, Some(msg)),
            )
            .await?;
        } else {
            // Backend already 409s this; reachable only via a pause race after its check.
            set_job_status(ctx, job, JobStatus::Failure, None, Some(msg)).await?;
        }
        return Ok(HandleOutcome::Completed);
    }

    let heartbeat = match job.run_id {
        Some(run_id) => {
            if !run_lifecycle::claim_pending_run(&ctx.db, run_id).await? {
                // Lost the pending->running CAS (duplicate delivery or swept run); the winner owns
                // the lifecycle.
                return Ok(HandleOutcome::Completed);
            }
            Some(run_lifecycle::spawn_heartbeat(ctx.db.clone(), run_id))
        }
        None => None,
    };

    // Pre-start cancel check, before STARTED, so a cancelled job never reads back as STARTED.
    if queue::is_cancelled(&ctx.redis, &job.job_id).await? {
        finalize_cancelled(ctx, job, heartbeat).await?;
        return Ok(HandleOutcome::Cancelled);
    }

    set_job_status(ctx, job, JobStatus::Started, None, None).await?;

    // Captured before the dial: health writes need the attempt's start time so a slower concurrent
    // attempt can't win the recency race.
    let health_checked_at = Utc::now();

    // The per-statement connector cap is always on; deadline-engine runs also get a whole-run
    // wall-clock cap.
    let outcome: Result<Option<RunQueryOutcome>, RunFailure> = if job.run_id.is_some() {
        let timeout_secs = run_lifecycle::run_timeout_secs();
        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            run_query(ctx, job, heartbeat.as_ref()),
        )
        .await
        {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(RunFailure::Error(e)),
            Err(_) => Err(RunFailure::Timeout(timeout_secs)),
        }
    } else {
        run_query(ctx, job, heartbeat.as_ref())
            .await
            .map_err(RunFailure::Error)
    };

    // A cancel can race a genuine finish and wins; `unwrap_or(false)` is deliberate so a Redis blip
    // never turns an outcome into a cancel.
    let outcome = match outcome.transpose() {
        Some(o)
            if !queue::is_cancelled(&ctx.redis, &job.job_id)
                .await
                .unwrap_or(false) =>
        {
            o
        }
        _ => {
            finalize_cancelled(ctx, job, heartbeat).await?;
            return Ok(HandleOutcome::Cancelled);
        }
    };

    match outcome {
        Ok(result) => {
            if result.superseded {
                tracing::info!(
                    job_id = %job.job_id,
                    query_result_id = result.query_result_id,
                    "execute_query: a concurrent execution already holds a fresher result, cache pointer left untouched"
                );
            }
            crate::health::QUERY_RUNS_SUCCESS.fetch_add(1, Ordering::Relaxed);
            // Any successful run_query means the connector connected: mark health OK.
            write_data_source_health(
                &ctx.db,
                job.data_source_id,
                job.org_id,
                health_checked_at,
                "ok",
                None,
            )
            .await;
            set_job_status(
                ctx,
                job,
                JobStatus::Success,
                Some(result.query_result_id),
                None,
            )
            .await?;
            audit(
                ctx,
                job,
                "query.executed",
                Some("query_result"),
                Some(result.query_result_id),
                serde_json::json!({ "job_id": job.job_id, "data_source_id": job.data_source_id }),
            )
            .await;

            drop(heartbeat);
            if let Some(run_id) = job.run_id {
                run_lifecycle::finalize_run_with_retry(
                    ctx,
                    run_id,
                    RunOutcome {
                        row_count: Some(result.row_count),
                        query_result_id: Some(result.query_result_id),
                        ..RunOutcome::terminal(RunStatus::Success, None)
                    },
                )
                .await?;
            }
        }
        Err(failure) => {
            // `format_error_chain` keeps the full context chain so the FAILURE `error` is
            // actionable.
            let (message, status) = match &failure {
                RunFailure::Timeout(secs) => {
                    (format!("run exceeded {secs}s timeout"), RunStatus::TimedOut)
                }
                RunFailure::Error(e) => (format_error_chain(e), RunStatus::Failed),
            };
            // A timeout is an expected operational outcome, not a defect worth `error!` volume.
            if status == RunStatus::TimedOut {
                crate::health::QUERY_RUNS_TIMED_OUT.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    job_id = %job.job_id, run_id = ?job.run_id, query_id = ?job.query_id,
                    org_id = job.org_id, data_source_id = job.data_source_id,
                    error = %message,
                    "execute_query: query execution failed"
                );
            } else {
                crate::health::QUERY_RUNS_FAILED.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    job_id = %job.job_id, run_id = ?job.run_id, query_id = ?job.query_id,
                    org_id = job.org_id, data_source_id = job.data_source_id,
                    error = %message,
                    "execute_query: query execution failed"
                );
            }
            // Only a classified connect-level failure marks the data source unhealthy; timeouts and
            // SQL errors say nothing about reachability.
            if matches!(&failure, RunFailure::Error(e) if classify_run_query_error(e) == ConnectorErrorKind::Connect)
            {
                write_data_source_health(
                    &ctx.db,
                    job.data_source_id,
                    job.org_id,
                    health_checked_at,
                    "failing",
                    Some(&message),
                )
                .await;
            }
            set_job_status(ctx, job, JobStatus::Failure, None, Some(message.clone())).await?;

            // Persist the failure on the saved query row (ad-hoc runs have none). Raw UPDATE
            // guarded on `last_execution_at` moving forward so a slower, older execution can't
            // overwrite a newer outcome; 0 rows is a silent no-op.
            if let Some(query_id) = job.query_id {
                ctx.db
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE queries SET last_execution_status = 'Failed', \
                         last_execution_at = $1, last_execution_error = $2 \
                         WHERE id = $3 AND (last_execution_at IS NULL OR last_execution_at <= $1)",
                        [Utc::now().into(), message.clone().into(), query_id.into()],
                    ))
                    .await?;
            }

            audit(
                ctx,
                job,
                "execute_query_failed",
                Some("query"),
                job.query_id,
                serde_json::json!({ "job_id": job.job_id, "error": message }),
            )
            .await;

            drop(heartbeat);
            if let Some(run_id) = job.run_id {
                run_lifecycle::finalize_run(
                    ctx,
                    run_id,
                    RunOutcome::terminal(status, Some(message)),
                )
                .await?;
            }
        }
    }

    Ok(HandleOutcome::Completed)
}

/// Record data source health (`"ok"`/`"failing"`) reactively from whether the connector connected.
/// Errors are logged, never propagated. `checked_at` is the attempt's start time and doubles as a
/// recency guard so an older observation can't overwrite a newer one.
async fn write_data_source_health(
    db: &DatabaseConnection,
    data_source_id: i64,
    org_id: i64,
    checked_at: chrono::DateTime<Utc>,
    state: &str,
    error: Option<&str>,
) {
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE data_sources SET health_state = $1, health_error = $2, \
             health_checked_at = $3 WHERE id = $4 AND org_id = $5 \
             AND (health_checked_at IS NULL OR health_checked_at <= $3)",
            [
                state.into(),
                error.map(std::string::ToString::to_string).into(),
                checked_at.into(),
                data_source_id.into(),
                org_id.into(),
            ],
        ))
        .await;
    if let Err(e) = result {
        tracing::warn!(data_source_id, org_id, state, error = %e, "failed to write data source health");
    }
}

/// Terminal cancellation path: write `Cancelled` to Redis, audit it, abort any heartbeat, and
/// finalize the `runs` row.
async fn finalize_cancelled(
    ctx: &ExecuteQueryContext,
    job: &ExecuteQueryPayload,
    heartbeat: Option<arcodash_conveyor::HeartbeatGuard>,
) -> anyhow::Result<()> {
    set_job_status(ctx, job, JobStatus::Cancelled, None, None).await?;
    audit(
        ctx,
        job,
        "cancel_query_execution",
        Some("query"),
        job.query_id,
        serde_json::json!({ "job_id": job.job_id }),
    )
    .await;
    drop(heartbeat);
    if let Some(run_id) = job.run_id {
        // Redis already committed to `Cancelled`; retry the finalize so the row doesn't stay
        // `running` until lease expiry.
        run_lifecycle::finalize_run_with_retry(
            ctx,
            run_id,
            RunOutcome::terminal(RunStatus::Cancelled, None),
        )
        .await?;
    }
    Ok(())
}

async fn set_job_status(
    ctx: &ExecuteQueryContext,
    job: &ExecuteQueryPayload,
    status: JobStatus,
    query_result_id: Option<i64>,
    error: Option<String>,
) -> anyhow::Result<()> {
    queue::set_job_status(
        &ctx.redis,
        &job.job_id,
        job.org_id,
        Some(job.user_id),
        status,
        query_result_id,
        error,
    )
    .await
}

/// Best-effort audit event for `job`: a failure is logged, never propagated.
async fn audit(
    ctx: &ExecuteQueryContext,
    job: &ExecuteQueryPayload,
    action: &str,
    object_type: Option<&str>,
    object_id: Option<i64>,
    details: Value,
) {
    if let Err(e) = record_event(
        &ctx.db,
        job.org_id,
        Some(job.user_id),
        action,
        object_type,
        object_id,
        details,
    )
    .await
    {
        tracing::warn!(job_id = %job.job_id, error = %e, "execute_query: failed to record {action} audit event");
    }
}

/// How often `poll_until_cancelled` re-checks the Redis cancellation flag while a query runs.
const CANCEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Polls the cancellation flag until it reads true. Redis errors are logged and treated as not
/// cancelled. Uses the shared `ctx.redis` since `EXISTS` is non-blocking and blocking commands are
/// kept off it.
async fn poll_until_cancelled(redis: &ConnectionManager, job_id: &str) {
    loop {
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
        match queue::is_cancelled(redis, job_id).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => tracing::warn!(job_id, error = %e, "cancellation poll failed; retrying"),
        }
        // Also refresh the status record's TTL so a long run doesn't expire it mid-flight (which
        // would make Cancel a no-op).
        if let Err(e) = queue::refresh_job_status_ttl(redis, job_id).await {
            tracing::warn!(job_id, error = %e, "job status TTL refresh failed; retrying");
        }
    }
}

/// Conveyor entry point for `execute_query`/`execute_query_scheduled`: a thin wrapper around
/// `handle_inner`, registered under both task type names.
pub struct ExecuteQueryHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait::async_trait]
impl arcodash_conveyor::TypedTaskHandler for ExecuteQueryHandler {
    type Payload = arcodash_core::query_exec::ExecuteQueryPayload;

    async fn run(
        &self,
        _task: arcodash_conveyor::TaskContext,
        job: Self::Payload,
    ) -> arcodash_conveyor::TaskResult {
        match handle_inner(&self.ctx, &job).await {
            Ok(HandleOutcome::Completed) => {
                Ok(arcodash_conveyor::TaskOutcome::Success { result_ref: None })
            }
            Ok(HandleOutcome::Cancelled) => Ok(arcodash_conveyor::TaskOutcome::Cancelled),
            Err(e) => Ok(arcodash_conveyor::TaskOutcome::Failed {
                error: format!("{e:#}"),
            }),
        }
    }
}

/// What a completed run reports: the `query_results` row, its row count, and whether a
/// later-started execution already holds a fresher snapshot (then `query_result_id` is that row).
struct RunQueryOutcome {
    query_result_id: i64,
    row_count: i64,
    superseded: bool,
}

/// Steps 3-6: load the data source, run the query, persist the result, and bump
/// `queries.latest_query_data_id`. Returns `Ok(None)` on mid-query cancellation (nothing
/// persisted).
///
/// `heartbeat` is checked via `HeartbeatGuard::lost` before persisting; if the lease was lost, this
/// bails rather than writing for a run that may no longer be authoritative.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "a single result set's row count fits the i32 `queries.row_count` column and i64"
)]
async fn run_query(
    ctx: &ExecuteQueryContext,
    job: &ExecuteQueryPayload,
    heartbeat: Option<&arcodash_conveyor::HeartbeatGuard>,
) -> anyhow::Result<Option<RunQueryOutcome>> {
    // Step 3: load data source, decrypt options, resolve connector. The org check stops a
    // buggy/compromised producer from running one org's query against another's connection.
    let ds = data_source::Entity::find_by_id(job.data_source_id)
        .one(&ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("data source {} not found", job.data_source_id))?;
    if ds.org_id != job.org_id {
        tracing::error!(
            job_org_id = job.org_id,
            data_source_id = job.data_source_id,
            data_source_org_id = ds.org_id,
            "execute_query: job's data_source_id belongs to a different org — refusing to run"
        );
        anyhow::bail!(
            "data source {} does not belong to org {}",
            job.data_source_id,
            job.org_id
        );
    }

    // Resolved before querying so a warehouse run is never spent on a result that can't be stored.
    let storage = ctx.storage.resolve(&ctx.db, job.org_id).await?;

    let effective_max_rows = job
        .max_rows
        .unwrap_or(ctx.max_query_rows)
        .min(ctx.max_query_rows);
    // Snapshot is taken at statement start, so this is the correct "as-of" key for ordering
    // concurrent executions.
    let started_at = Utc::now();
    let (result, truncated) = tokio::select! {
        r = run_sql_once(
            ctx,
            &ds,
            job.user_id,
            &job.query_text,
            effective_max_rows,
            ctx.query_timeout_secs,
        ) => r?,
        () = poll_until_cancelled(&ctx.redis, &job.job_id) => return Ok(None),
    };
    let row_count = result.rows.len();

    if heartbeat.is_some_and(arcodash_conveyor::HeartbeatGuard::lost) {
        tracing::warn!(
            job_id = %job.job_id,
            run_id = ?job.run_id,
            "execute_query: heartbeat lease lost mid-execution, refusing to persist"
        );
        anyhow::bail!(
            "run lease lost mid-execution (reclaimed or finalized by another caller); refusing \
             to persist a result for a run this worker may no longer own"
        );
    }

    // Step 5: upsert query_results (find-then-update/insert). A transaction-scoped advisory lock on
    // (org, data source, query_hash) serializes concurrent executions so they can't both INSERT;
    // chosen over a unique index to avoid a data-risky migration.
    let query_hash = gen_query_hash(&job.query_text, &job.parameters);
    let data = serde_json::json!({
        "columns": result.columns,
        "rows": result.rows,
        "truncated": truncated,
    });
    let retrieved_at = Utc::now();

    let txn = ctx.db.begin().await?;
    let lock_key = format!("{}:{}:{}", job.org_id, job.data_source_id, query_hash);
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [lock_key.into()],
    ))
    .await?;

    let existing = query_result::Entity::find()
        .filter(query_result::Column::OrgId.eq(job.org_id))
        .filter(query_result::Column::DataSourceId.eq(job.data_source_id))
        .filter(query_result::Column::QueryHash.eq(query_hash.clone()))
        .one(&txn)
        .await?;

    // Compare the existing row's snapshot time to this run's start (not completion) so a slower,
    // earlier-started run can't clobber a fresher one regardless of commit order.
    let mut superseded = false;
    let saved = match existing {
        Some(existing) if existing.retrieved_at.with_timezone(&Utc) >= started_at => {
            superseded = true;
            existing
        }
        Some(existing) => {
            let stored = save_result(&storage, job.data_source_id, &query_hash, &data).await?;
            let mut active: query_result::ActiveModel = existing.into();
            active.query = Set(job.query_text.clone());
            active.data = Set(None);
            active.data_location = Set(Some(stored.location));
            active.data_size_bytes = Set(Some(stored.size_bytes));
            active.runtime = Set(Some(result.runtime_seconds));
            active.retrieved_at = Set(retrieved_at.into());
            active.last_used_at = Set(retrieved_at.into());
            active.update(&txn).await?
        }
        None => {
            let stored = save_result(&storage, job.data_source_id, &query_hash, &data).await?;
            let active = query_result::ActiveModel {
                org_id: Set(job.org_id),
                data_source_id: Set(job.data_source_id),
                query_hash: Set(query_hash),
                query: Set(job.query_text.clone()),
                data: Set(None),
                data_location: Set(Some(stored.location)),
                data_size_bytes: Set(Some(stored.size_bytes)),
                runtime: Set(Some(result.runtime_seconds)),
                retrieved_at: Set(retrieved_at.into()),
                last_used_at: Set(retrieved_at.into()),
                ..Default::default()
            };
            active.insert(&txn).await?
        }
    };
    txn.commit().await?;

    // Step 6: repoint the query's pointer/status for saved-query runs (published pointer for
    // published content, else draft; never both). One guarded statement so the pointer and
    // `last_execution_at` can't disagree on the winner. A superseded run skips this: `saved` is the
    // other run's row.
    if !superseded {
        if let Some(query_id) = job.query_id {
            let pointer_column = if job.repoint_published {
                "published_latest_query_data_id"
            } else {
                "latest_query_data_id"
            };
            ctx.db
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    format!(
                        "UPDATE queries SET {pointer_column} = $1, last_execution_status = 'Success', \
                         last_execution_at = $2, row_count = $3, last_execution_error = NULL \
                         WHERE id = $4 AND (last_execution_at IS NULL OR last_execution_at <= $2)"
                    ),
                    [
                        saved.id.into(),
                        retrieved_at.into(),
                        (row_count as i32).into(),
                        query_id.into(),
                    ],
                ))
                .await?;
        }
    }

    Ok(Some(RunQueryOutcome {
        query_result_id: saved.id,
        row_count: row_count as i64,
        superseded,
    }))
}

/// Resolve the connector, run `sql` under a hard wall-clock cap, and trim the auto-limit
/// sentinel row. Returns the result and whether it was truncated to `max_rows`.
pub async fn run_sql_once(
    ctx: &ExecuteQueryContext,
    ds: &data_source::Model,
    user_id: i64,
    sql: &str,
    max_rows: u32,
    timeout_secs: u64,
) -> anyhow::Result<(arcodash_core::connectors::QueryExecutionResult, bool)> {
    let connector = ctx
        .connectors
        .get(&ds.r#type)
        .ok_or_else(|| anyhow::anyhow!("unknown data source type '{}'", ds.r#type))?;

    // query_results reads other queries' cached results with no other tenant boundary (the worker's
    // DB role bypasses RLS), so inject the caller's org_id/user_id for it to filter on.
    let options = arcodash_core::connectors::query_results::inject_caller(
        decrypt_options(&ctx.encryption_key, ds.encrypted_options.as_deref())?,
        &ds.r#type,
        ds.org_id,
        user_id,
    );

    // Connectors that opt out of auto-limit (e.g. mssql) are untouched. `tokio::time::timeout` is
    // the authoritative cap; DB statement timeouts are a backstop.
    let auto_limited = connector.supports_auto_limit();
    let query_text = if auto_limited {
        arcodash_core::connectors::apply_auto_limit(sql, max_rows)
    } else {
        sql.to_string()
    };
    let mut result =
        run_with_timeout(connector.as_ref(), &options, &query_text, timeout_secs).await?;

    // `LIMIT max_rows + 1` is a sentinel: getting that many rows back means truncation. Opted-out
    // connectors never truncate.
    let truncated = auto_limited && result.rows.len() > max_rows as usize;
    if truncated {
        result.rows.truncate(max_rows as usize);
    }
    Ok((result, truncated))
}

/// Decrypt `data_sources.encrypted_options` and parse as JSON.
pub fn decrypt_options(
    encryption_key: &[u8; 32],
    encrypted: Option<&[u8]>,
) -> anyhow::Result<Value> {
    let Some(bytes) = encrypted else {
        return Ok(Value::Object(serde_json::Map::default()));
    };
    let plaintext = secrets::decrypt(encryption_key, bytes)?;
    Ok(serde_json::from_slice(&plaintext)?)
}

/// Run `connector.run_query` under a hard wall-clock cap; a timeout becomes an `Err`.
pub async fn run_with_timeout(
    connector: &dyn arcodash_core::connectors::QueryRunner,
    options: &Value,
    query_text: &str,
    timeout_secs: u64,
) -> anyhow::Result<arcodash_core::connectors::QueryExecutionResult> {
    tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        connector.run_query(options, query_text),
    )
    .await
    .map_err(|_| anyhow::anyhow!("query exceeded {timeout_secs}s timeout"))?
}

#[cfg(test)]
mod test_helpers {
    use std::sync::{Arc, Mutex};

    use arcodash_core::connectors::{
        connect_err, ConnectorErrorKind, NetworkTarget, QueryExecutionResult, QueryRunner,
        TableSchema,
    };
    use arcodash_core::query_exec::ExecuteQueryPayload;
    use async_trait::async_trait;
    use serde_json::Value;

    /// `QueryRunner` stub: after `delay_ms`, returns `rows` rows (`{"n": i}`) or fails with
    /// `fail`, recording the query text and options of its last call.
    pub(super) struct StubConnector {
        pub type_name: &'static str,
        pub rows: usize,
        pub auto_limit: bool,
        pub delay_ms: u64,
        pub fail: Option<ConnectorErrorKind>,
        pub seen_query: Arc<Mutex<Option<String>>>,
        pub seen_opts: Arc<Mutex<Option<Value>>>,
    }

    impl StubConnector {
        pub(super) fn new(type_name: &'static str, rows: usize) -> Self {
            Self {
                type_name,
                rows,
                auto_limit: true,
                delay_ms: 0,
                fail: None,
                seen_query: Default::default(),
                seen_opts: Default::default(),
            }
        }
    }

    #[async_trait]
    impl QueryRunner for StubConnector {
        fn type_name(&self) -> &'static str {
            self.type_name
        }

        fn configuration_schema(&self) -> Value {
            serde_json::json!({})
        }

        fn network_targets(&self, _opts: &Value) -> Vec<NetworkTarget> {
            vec![]
        }

        async fn test_connection(&self, _opts: &Value) -> anyhow::Result<()> {
            Ok(())
        }

        async fn get_schema(&self, _opts: &Value) -> anyhow::Result<Vec<TableSchema>> {
            Ok(vec![])
        }

        async fn run_query(
            &self,
            opts: &Value,
            query: &str,
        ) -> anyhow::Result<QueryExecutionResult> {
            *self.seen_query.lock().unwrap() = Some(query.to_string());
            *self.seen_opts.lock().unwrap() = Some(opts.clone());
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            match self.fail {
                Some(ConnectorErrorKind::Connect) => {
                    Err(connect_err(anyhow::anyhow!("connection refused")))
                }
                Some(ConnectorErrorKind::Statement) => {
                    Err(anyhow::anyhow!("relation \"foo\" does not exist"))
                }
                None => Ok(QueryExecutionResult {
                    columns: vec![],
                    rows: (0..self.rows)
                        .map(|i| serde_json::json!({ "n": i }))
                        .collect(),
                    runtime_seconds: 0.0,
                }),
            }
        }

        fn supports_auto_limit(&self) -> bool {
            self.auto_limit
        }
    }

    /// An interactive `SELECT 1` job with no saved query, run or row cap.
    pub(super) fn test_job(
        job_id: impl Into<String>,
        org_id: i64,
        data_source_id: i64,
    ) -> ExecuteQueryPayload {
        ExecuteQueryPayload {
            job_id: job_id.into(),
            org_id,
            data_source_id,
            query_text: "SELECT 1".to_string(),
            parameters: serde_json::json!({}),
            user_id: 1,
            query_id: None,
            scheduled: false,
            run_id: None,
            repoint_published: false,
            max_rows: None,
        }
    }
}

#[cfg(test)]
mod timeout_test {
    use super::run_with_timeout;
    use super::test_helpers::StubConnector;

    fn slow_connector() -> StubConnector {
        StubConnector {
            delay_ms: 50,
            ..StubConnector::new("slow-test-stub", 0)
        }
    }

    #[tokio::test]
    async fn elapsed_timeout_produces_an_error() {
        let connector = slow_connector();
        let result = run_with_timeout(&connector, &serde_json::json!({}), "SELECT 1", 0).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn fast_query_within_timeout_succeeds() {
        let connector = slow_connector();
        let result = run_with_timeout(&connector, &serde_json::json!({}), "SELECT 1", 5).await;
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod cancellation_integration_test {
    //! Pre-start cancellation against real Postgres + Redis (skips unless
    //! `TEST_DATABASE_URL`/`TEST_REDIS_URL` are set). Calls `handle` directly because racing real
    //! processes is flaky; a nonexistent `data_source_id` ensures `Cancelled` can't come from
    //! `run_query` succeeding.
    use super::test_helpers::test_job;
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_before_dispatch_short_circuits_before_run_query() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");

        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let job_id = format!("manual-cancel-repro-{}", uuid::Uuid::new_v4());

        queue::request_cancellation(&redis, &job_id).await.unwrap();
        let flag = queue::is_cancelled(&redis, &job_id).await.unwrap();
        assert!(
            flag,
            "cancellation flag must read back true right after setting it"
        );

        let ctx = crate::scheduler::test_support::test_ctx(db, redis.clone());
        // Nonexistent org and data source on purpose: run_query must fail loudly.
        let job = test_job(job_id.clone(), 999_999_999, 999_999_999);

        // Concurrent poller: if the pre-start check ever moved after the STARTED write, the job
        // would transiently read Started. Needs a multi-thread runtime for real interleaving.
        let observed_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poller = {
            let redis = redis.clone();
            let job_id = job_id.clone();
            let observed_started = observed_started.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(Some(s)) = queue::get_job_status(&redis, &job_id).await {
                        if s.status == JobStatus::Started {
                            observed_started.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            })
        };

        handle(&ctx, job).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.await.unwrap();

        assert!(
            !observed_started.load(std::sync::atomic::Ordering::Relaxed),
            "a pre-cancelled job must never transiently report STARTED"
        );

        let status = queue::get_job_status(&redis, &job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.status,
            JobStatus::Cancelled,
            "expected the pre-start cancellation check to short-circuit before run_query \
             (which would have failed anyway on the nonexistent data source, proving this \
             wasn't accidentally masked by a run_query error path)"
        );
        assert!(status.query_result_id.is_none());
    }
}

#[cfg(test)]
mod mid_flight_cancel_tests {
    //! `poll_until_cancelled` in isolation, plus the mid-flight `select!` against a long-running
    //! connector future: it must return early and leave nothing in `query_results`/`queries`.
    use std::time::Duration;

    use super::test_helpers::test_job;
    use super::*;
    use crate::scheduler::test_support::{
        seed_data_source, seed_org, seed_query, seed_run, seed_target_pg_data_source, test_ctx,
    };

    #[tokio::test]
    async fn poll_until_cancelled_returns_once_the_flag_is_set() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let job_id = format!("poll-until-cancelled-{}", uuid::Uuid::new_v4());

        let poller = {
            let redis = redis.clone();
            let job_id = job_id.clone();
            tokio::spawn(async move { poll_until_cancelled(&redis, &job_id).await })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        queue::request_cancellation(&redis, &job_id).await.unwrap();

        tokio::time::timeout(Duration::from_secs(3), poller)
            .await
            .expect("poll_until_cancelled did not return within 3s of the flag being set")
            .unwrap();
    }

    /// The job status TTL must be refreshed while a query runs, else Cancel no-ops for long
    /// queries.
    #[tokio::test]
    async fn poll_until_cancelled_refreshes_the_job_status_ttl_while_the_query_runs() {
        use redis::AsyncCommands;

        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let job_id = format!("poll-ttl-refresh-{}", uuid::Uuid::new_v4());
        let key = format!("job:{job_id}");

        queue::set_job_status(&redis, &job_id, 1, None, JobStatus::Started, None, None)
            .await
            .unwrap();
        let mut conn = redis.clone();
        let _: bool = conn.expire(&key, 2).await.unwrap();
        let low_ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!(low_ttl <= 2, "the manual EXPIRE must have taken effect");

        let poller = {
            let redis = redis.clone();
            let job_id = job_id.clone();
            tokio::spawn(async move { poll_until_cancelled(&redis, &job_id).await })
        };

        tokio::time::sleep(Duration::from_millis(3_500)).await;
        let survived_ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!(
            survived_ttl > 2,
            "the job status record must survive past its original low TTL and be pushed back \
             up while the poller is still running, got ttl={survived_ttl}"
        );
        assert!(
            queue::get_job_status(&redis, &job_id)
                .await
                .unwrap()
                .is_some(),
            "the record itself must still exist, not have expired mid-flight"
        );

        queue::request_cancellation(&redis, &job_id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), poller)
            .await
            .expect("poll_until_cancelled did not return within 3s of the flag being set")
            .unwrap();

        let _: () = conn.del(&key).await.unwrap();
    }

    #[tokio::test]
    async fn mid_flight_cancel_writes_cancelled_and_persists_nothing() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let encryption_key = [7u8; 32];
        let org = seed_org(
            &db,
            &format!("org-midflight-cancel-{}", uuid::Uuid::new_v4()),
        )
        .await;
        let Some(ds) = seed_target_pg_data_source(&db, org.id, &encryption_key).await else {
            eprintln!("skipping: TEST_TARGET_DATABASE_URL not set");
            return;
        };
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        let query = seed_query(&db, org.id, ds.id).await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.encryption_key = encryption_key;

        let job_id = format!("mid-flight-cancel-{}", uuid::Uuid::new_v4());
        let job = ExecuteQueryPayload {
            query_text: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(20)) s".to_string(),
            query_id: Some(query.id),
            ..test_job(job_id.clone(), org.id, ds.id)
        };

        let job_handle = tokio::spawn(async move {
            handle(&ctx, job).await;
        });

        tokio::time::sleep(Duration::from_secs(1)).await;
        queue::request_cancellation(&redis, &job_id).await.unwrap();

        // The 10s deadline is the proof: pg_sleep(20) can't have finished, so the future was
        // dropped.
        tokio::time::timeout(Duration::from_secs(10), job_handle)
            .await
            .expect("handle did not return within 10s — the connector future was not dropped")
            .unwrap();

        let status = queue::get_job_status(&redis, &job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.status, JobStatus::Cancelled);
        assert!(status.query_result_id.is_none());

        let result_count = query_result::Entity::find()
            .filter(query_result::Column::DataSourceId.eq(ds.id))
            .all(&db)
            .await
            .unwrap()
            .len();
        assert_eq!(
            result_count, 0,
            "mid-flight cancellation must persist no query_results row"
        );

        let refreshed = query::Entity::find_by_id(query.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            refreshed.latest_query_data_id, query.latest_query_data_id,
            "latest_query_data_id must not move on a cancelled run"
        );
    }

    /// `finalize_cancelled` must use the retrying finalize: a pre-start cancel still reaches
    /// `runs.status = 'cancelled'`.
    #[tokio::test]
    async fn cancelled_run_is_finalized_through_the_retrying_path() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let org = seed_org(&db, &format!("org-cancel-retry-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
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

        let ctx = test_ctx(db.clone(), redis.clone());
        let job_id = format!("cancel-retry-{}", uuid::Uuid::new_v4());
        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            run_id: Some(run_id),
            ..test_job(job_id.clone(), org.id, ds.id)
        };

        queue::request_cancellation(&redis, &job_id).await.unwrap();

        handle_inner(&ctx, &job).await.unwrap();

        let reloaded_run = arcodash_core::entities::run::Entity::find_by_id(run_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_run.status, "cancelled",
            "a pre-start cancellation must finalize the run as cancelled"
        );
    }
}

#[cfg(test)]
mod conveyor_dispatch_tests {
    //! Conveyor dispatch of `execute_query` preserves fast/long tier isolation and shutdown drain
    //! grace. Needs real Postgres + Redis + target Postgres (skips without
    //! `TEST_DATABASE_URL`/`TEST_REDIS_URL`/`TEST_TARGET_DATABASE_URL`).
    use std::future::Future;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arcodash_conveyor::{Conveyor, ConveyorBuilder};
    use arcodash_core::entities::query_result;
    use arcodash_core::query_exec::{enqueue_execute_query, ExecuteQueryPayload};
    use arcodash_core::queue::{self as core_queue, JobStatus, ValkeyQueue};
    use redis::AsyncCommands;
    use sea_orm::{ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement};
    use tokio::sync::watch;

    use super::test_helpers::test_job;
    use super::{ExecuteQueryContext, ExecuteQueryHandler};
    use crate::scheduler::test_support::*;

    /// Same key as the conveyor scheduler's leader election (its constant is crate-private).
    const SCHEDULER_LEADER_LOCK_KEY: &str = "arcodash:conveyor:scheduler:leader";

    /// Runs `conveyor`, then `body` on its own task so a panicking assertion can't skip cleanup;
    /// then signals shutdown, drains the conveyor loop, and clears the scheduler leader lock (its TTL outlives a
    /// clean shutdown). Returns the drain duration.
    async fn run_guarded<F>(
        conveyor: Arc<Conveyor>,
        redis: redis::aio::ConnectionManager,
        drain_deadline: Duration,
        body: F,
    ) -> Duration
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = tokio::spawn(async move { conveyor.run(shutdown_rx).await });
        let outcome = tokio::spawn(body).await;

        let shutdown_start = Instant::now();
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(drain_deadline, run_handle).await;
        let elapsed = shutdown_start.elapsed();

        let mut conn = redis;
        let _: redis::RedisResult<()> = conn.del(SCHEDULER_LEADER_LOCK_KEY).await;

        match outcome {
            Ok(()) => elapsed,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    fn build_test_conveyor(
        ctx: Arc<ExecuteQueryContext>,
        redis_url: String,
        fast_concurrency: usize,
        long_concurrency: usize,
        long_drain_secs: u64,
    ) -> Arc<Conveyor> {
        ConveyorBuilder::new(ctx.db.clone(), ctx.redis.clone(), redis_url)
            .register("execute_query", ExecuteQueryHandler { ctx: ctx.clone() })
            .register(
                "execute_query_scheduled",
                ExecuteQueryHandler { ctx: ctx.clone() },
            )
            .tier("query-fast", &["execute_query"], fast_concurrency)
            .tier_with_drain(
                "query-long",
                &["execute_query_scheduled"],
                long_concurrency,
                long_drain_secs,
            )
            .build()
    }

    /// Writes the `Pending` job status (as the backend does) and enqueues through `conveyor`.
    async fn enqueue_pending(
        ctx: &ExecuteQueryContext,
        conveyor: &Arc<Conveyor>,
        payload: ExecuteQueryPayload,
    ) {
        core_queue::set_job_status(
            &ctx.redis,
            &payload.job_id,
            payload.org_id,
            None,
            JobStatus::Pending,
            None,
            None,
        )
        .await
        .unwrap();
        enqueue_execute_query(
            &ctx.db,
            &ValkeyQueue::new(ctx.redis.clone()),
            Some(conveyor),
            payload,
        )
        .await
        .unwrap();
    }

    async fn wait_for_terminal(
        redis: &redis::aio::ConnectionManager,
        job_id: &str,
        deadline: Duration,
    ) -> core_queue::JobStatusRecord {
        let start = Instant::now();
        loop {
            if let Some(status) = core_queue::get_job_status(redis, job_id).await.unwrap() {
                if status.status.is_terminal() {
                    return status;
                }
            }
            assert!(
                start.elapsed() < deadline,
                "job {job_id} never reached a terminal status within {deadline:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Shared setup: target-Postgres-backed context, org and data source; returns `None` if the
    /// real-infra env vars are unset.
    async fn setup(suffix: &str) -> Option<(Arc<ExecuteQueryContext>, String, i64, i64)> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let encryption_key = [21u8; 32];
        let org = seed_org(
            &db,
            &format!("org-conveyor-{suffix}-{}", uuid::Uuid::new_v4()),
        )
        .await;
        let ds = seed_target_pg_data_source(&db, org.id, &encryption_key).await?;
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

        exec_sql(
            &db,
            "DELETE FROM conveyor_tasks WHERE resource_key = $1",
            [ds.id.to_string().into()],
        )
        .await;

        let mut ctx = test_ctx(db, redis);
        ctx.encryption_key = encryption_key;
        Some((Arc::new(ctx), redis_url, org.id, ds.id))
    }

    #[tokio::test]
    async fn fast_tier_query_dispatches_and_completes_through_conveyor() {
        let Some((ctx, redis_url, org_id, ds_id)) = setup("fast").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 4, 1, 5);
        run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(10),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
                let job_id = format!("conveyor-fast-{}", uuid::Uuid::new_v4());
                let start = Instant::now();
                enqueue_pending(&ctx, &conveyor, test_job(job_id.clone(), org_id, ds_id)).await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                let status = wait_for_terminal(&ctx.redis, &job_id, Duration::from_secs(10)).await;
                assert_eq!(status.status, JobStatus::Success);
                eprintln!(
                    "fast conveyor query completed {:?} after enqueue",
                    start.elapsed()
                );
            },
        )
        .await;
    }

    /// A run cancelled mid-flight under conveyor dispatch finalizes both `runs` and
    /// `conveyor_tasks` as `cancelled`.
    #[tokio::test]
    async fn cancelled_run_finalizes_both_the_run_and_the_conveyor_task_as_cancelled() {
        let Some((ctx, redis_url, org_id, ds_id)) = setup("cancel").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        let query = seed_query(&ctx.db, org_id, ds_id).await;
        let run_id = seed_run(
            &ctx.db,
            org_id,
            "query",
            query.id,
            None,
            Some(ds_id),
            "pending",
            None,
        )
        .await;

        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 4, 1, 5);
        run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(10),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
                let job_id = format!("conveyor-cancel-{}", uuid::Uuid::new_v4());
                enqueue_pending(
                    &ctx,
                    &conveyor,
                    ExecuteQueryPayload {
                        query_text: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(20)) s"
                            .to_string(),
                        query_id: Some(query.id),
                        run_id: Some(run_id),
                        ..test_job(job_id.clone(), org_id, ds_id)
                    },
                )
                .await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                tokio::time::sleep(Duration::from_secs(1)).await;
                core_queue::request_cancellation(&ctx.redis, &job_id)
                    .await
                    .unwrap();

                let status = wait_for_terminal(&ctx.redis, &job_id, Duration::from_secs(10)).await;
                assert_eq!(status.status, JobStatus::Cancelled);

                let reloaded_run = arcodash_core::entities::run::Entity::find_by_id(run_id)
                    .one(&ctx.db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(reloaded_run.status, "cancelled");

                let task_row = ctx
                    .db
                    .query_one_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "SELECT status FROM conveyor_tasks WHERE resource_key = $1 \
                         ORDER BY id DESC LIMIT 1",
                        [ds_id.to_string().into()],
                    ))
                    .await
                    .unwrap()
                    .expect("a conveyor_tasks row for this data source");
                let task_status: String = task_row.try_get("", "status").unwrap();
                assert_eq!(task_status, "cancelled");
            },
        )
        .await;
    }

    /// The pause gate in `handle_inner` is exercised through a real conveyor loop; `SELECT 1` would
    /// succeed if the gate were bypassed.
    #[tokio::test]
    async fn paused_data_source_is_refused_by_the_worker_through_real_conveyor_dispatch() {
        let Some((ctx, redis_url, org_id, ds_id)) = setup("paused").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        exec_sql(
            &ctx.db,
            "UPDATE data_sources SET paused_at = now(), pause_reason = 'maintenance window' \
                 WHERE id = $1",
            [ds_id.into()],
        )
        .await;

        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 4, 1, 5);
        run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(10),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
                let job_id = format!("conveyor-paused-{}", uuid::Uuid::new_v4());
                enqueue_pending(&ctx, &conveyor, test_job(job_id.clone(), org_id, ds_id)).await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                let status = wait_for_terminal(&ctx.redis, &job_id, Duration::from_secs(10)).await;
                assert_eq!(
                    status.status,
                    JobStatus::Failure,
                    "a paused source dispatched through the real conveyor path must be refused"
                );
                assert_eq!(
                    status.error.as_deref(),
                    Some("this data source is paused: maintenance window")
                );

                let results = query_result::Entity::find()
                    .filter(query_result::Column::OrgId.eq(org_id))
                    .all(&ctx.db)
                    .await
                    .unwrap();
                assert!(
                    results.is_empty(),
                    "a paused source's query must never actually execute"
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn long_tier_slow_query_dispatches_and_actually_completes() {
        // Proves long-tier dispatch runs a `pg_sleep` query to completion; drain-survival is
        // covered by `graceful_drain_tests`.
        let Some((ctx, redis_url, org_id, ds_id)) = setup("long").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 4, 1, 8);
        run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(15),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
                let job_id = format!("conveyor-long-{}", uuid::Uuid::new_v4());
                enqueue_pending(
                    &ctx,
                    &conveyor,
                    ExecuteQueryPayload {
                        query_text: "SELECT pg_sleep(4)".to_string(),
                        scheduled: true,
                        ..test_job(job_id.clone(), org_id, ds_id)
                    },
                )
                .await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                // Must reach Started promptly (dispatched onto the long tier), then Success only
                // after pg_sleep(4) elapses.
                let start_deadline = Duration::from_secs(10);
                let start = Instant::now();
                loop {
                    if let Some(status) = core_queue::get_job_status(&ctx.redis, &job_id)
                        .await
                        .unwrap()
                    {
                        if status.status == JobStatus::Started {
                            break;
                        }
                    }
                    assert!(
                        start.elapsed() < start_deadline,
                        "long-tier query never reached Started — not dispatched onto the long tier"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }

                let status = wait_for_terminal(&ctx.redis, &job_id, Duration::from_secs(15)).await;
                assert_eq!(status.status, JobStatus::Success);
                assert!(
                    start.elapsed() >= Duration::from_secs(4),
                    "completed in {:?} — pg_sleep(4) must not have actually run to completion",
                    start.elapsed()
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn saturated_long_tier_does_not_block_fast_tier_dispatch_through_conveyor() {
        let Some((ctx, redis_url, org_id, ds_id)) = setup("starve").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        // fast=1, long=1: two separate permit pools; a single shared semaphore would not
        // demonstrate isolation.
        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 1, 1, 5);
        let drain_elapsed = run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(10),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");

                // Saturate the long tier two jobs deep; the default resource cap (4) covers all 3
                // tasks, so only tier isolation is under test.
                for _ in 0..2 {
                    let job_id = format!("conveyor-starve-long-{}", uuid::Uuid::new_v4());
                    enqueue_pending(&ctx, &conveyor, ExecuteQueryPayload { query_text: "SELECT pg_sleep(8)".to_string(), scheduled: true, ..test_job(job_id, org_id, ds_id) }).await;
                }

                tokio::time::sleep(Duration::from_millis(500)).await;

                let fast_job_id = format!("conveyor-starve-fast-{}", uuid::Uuid::new_v4());
                let start = Instant::now();
                enqueue_pending(&ctx, &conveyor, test_job(fast_job_id.clone(), org_id, ds_id)).await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                // The fast job must dispatch quickly; under a shared semaphore it would stall
                // behind the long jobs.
                let pickup_deadline = Duration::from_secs(2);
                loop {
                    let status = core_queue::get_job_status(&ctx.redis, &fast_job_id)
                        .await
                        .unwrap()
                        .unwrap();
                    if status.status != JobStatus::Pending {
                        break;
                    }
                    assert!(
                        start.elapsed() < pickup_deadline,
                        "fast-tier job was never dispatched within {pickup_deadline:?} while the \
                         long tier was saturated — head-of-line blocking regression through \
                         conveyor"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let pickup_elapsed = start.elapsed();

                let status =
                    wait_for_terminal(&ctx.redis, &fast_job_id, Duration::from_secs(5)).await;
                assert_eq!(status.status, JobStatus::Success);
                eprintln!(
                    "fast job dispatched in {pickup_elapsed:?} while the long tier was saturated (conveyor path)"
                );
            },
        )
        .await;

        // Long-tier jobs are still mid pg_sleep(8); the tier's drain grace (5s) bounds how long
        // shutdown waited.
        assert!(
            drain_elapsed < Duration::from_secs(8),
            "shutdown took {drain_elapsed:?} — should be capped by the long tier's 5s drain \
             grace, not the full 8s pg_sleep"
        );
    }

    #[tokio::test]
    async fn resource_cap_limits_concurrent_executions_on_the_same_data_source() {
        let Some((ctx, redis_url, org_id, ds_id)) = setup("rescap").await else {
            eprintln!("skipping: real-infra env vars not set");
            return;
        };
        // Cap this data source at 1 concurrent run so the second enqueue must stay pending.
        exec_sql(
            &ctx.db,
            "UPDATE data_sources SET max_concurrent_runs = 1 WHERE id = $1",
            [ds_id.into()],
        )
        .await;

        let conveyor = build_test_conveyor(ctx.clone(), redis_url, 4, 4, 5);
        run_guarded(
            conveyor.clone(),
            ctx.redis.clone(),
            Duration::from_secs(10),
            async move {
                std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");

                let job_a = format!("conveyor-rescap-a-{}", uuid::Uuid::new_v4());
                enqueue_pending(
                    &ctx,
                    &conveyor,
                    ExecuteQueryPayload {
                        query_text: "SELECT pg_sleep(3)".to_string(),
                        ..test_job(job_a.clone(), org_id, ds_id)
                    },
                )
                .await;

                let claim_deadline = Duration::from_secs(3);
                let claim_start = Instant::now();
                loop {
                    let status = core_queue::get_job_status(&ctx.redis, &job_a)
                        .await
                        .unwrap();
                    if matches!(status.map(|s| s.status), Some(JobStatus::Started)) {
                        break;
                    }
                    assert!(
                        claim_start.elapsed() < claim_deadline,
                        "job A never started"
                    );
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }

                let job_b = format!("conveyor-rescap-b-{}", uuid::Uuid::new_v4());
                enqueue_pending(&ctx, &conveyor, test_job(job_b.clone(), org_id, ds_id)).await;
                std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

                // B must stay Pending while A holds the data source's only permit, proving
                // `enqueue_execute_query` wires resource_key/resource_cap; the generic cap
                // mechanism is tested in the conveyor crate.
                tokio::time::sleep(Duration::from_millis(500)).await;
                let b_status = core_queue::get_job_status(&ctx.redis, &job_b)
                    .await
                    .unwrap();
                assert_eq!(
                    b_status.map(|s| s.status),
                    Some(JobStatus::Pending),
                    "B must stay parked at the data source's cap while A is still running"
                );
            },
        )
        .await;
    }
}

#[cfg(test)]
mod auto_limit_truncation_tests {
    //! `run_query` must trim the auto-limit sentinel row, set `data.truncated`, and leave opted-out
    //! connectors' rows untouched.
    use super::test_helpers::{test_job, StubConnector};
    use super::*;
    use crate::scheduler::test_support::{
        acquire_shared_test_db_lock, seed_data_source_of_type, seed_org, setup,
    };
    use sea_orm::ActiveValue::Set;

    fn ctx_with_registry(
        db: sea_orm::DatabaseConnection,
        redis: redis::aio::ConnectionManager,
        registry: ConnectorRegistry,
        max_query_rows: u32,
    ) -> ExecuteQueryContext {
        ExecuteQueryContext {
            max_query_rows,
            ..crate::scheduler::test_support::ctx_with_registry(db, redis, registry)
        }
    }

    /// Reads a freshly written result back from the context's object storage.
    async fn stored_result(ctx: &ExecuteQueryContext, qr: &query_result::Model) -> Value {
        let storage = ctx.storage.resolve(&ctx.db, qr.org_id).await.unwrap();
        let loc = qr.data_location.as_deref().expect("data_location set");
        let bytes = storage
            .get(arcodash_core::org_storage::StorageArea::Results, loc)
            .await
            .unwrap();
        arcodash_core::result_store::decode_result(&bytes).unwrap()
    }

    fn job(org_id: i64, data_source_id: i64) -> ExecuteQueryPayload {
        test_job(
            format!("auto-limit-test-{}", uuid::Uuid::new_v4()),
            org_id,
            data_source_id,
        )
    }

    fn ctx_with_store(
        ctx: ExecuteQueryContext,
        store: Arc<dyn arcodash_core::org_storage::BlobStore>,
    ) -> ExecuteQueryContext {
        ExecuteQueryContext {
            storage: Arc::new(
                arcodash_core::test_support::blob::StaticResolver::with_store(
                    arcodash_core::org_storage::StorageMode::ArcoDash,
                    store,
                    "orgs/test/",
                ),
            ),
            ..ctx
        }
    }

    async fn result_rows(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        ds_id: i64,
    ) -> Vec<query_result::Model> {
        query_result::Entity::find()
            .filter(query_result::Column::OrgId.eq(org_id))
            .filter(query_result::Column::DataSourceId.eq(ds_id))
            .all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn execute_query_stores_object_and_null_inline_data() {
        use arcodash_core::test_support::blob::MemoryBlobStore;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("store-obj-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("store-obj-stub", 3)));
        let ds = seed_data_source_of_type(&db, org.id, "store-obj-stub").await;
        let mem = Arc::new(MemoryBlobStore::default());
        let ctx = ctx_with_store(
            ctx_with_registry(db.clone(), redis, registry, 100),
            mem.clone(),
        );

        let outcome = run_query(&ctx, &job(org.id, ds.id), None)
            .await
            .unwrap()
            .expect("not cancelled");
        let saved = query_result::Entity::find_by_id(outcome.query_result_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(saved.data.is_none());
        let loc = saved.data_location.clone().expect("data_location set");
        assert!(saved.data_size_bytes.unwrap() > 0);
        assert_eq!(saved.last_used_at, saved.retrieved_at);
        assert_eq!(mem.keys(), [format!("orgs/test/results/{loc}")]);
        let data = stored_result(&ctx, &saved).await;
        assert_eq!(data["rows"].as_array().unwrap().len(), 3);
        assert_eq!(data["truncated"], false);
    }

    #[tokio::test]
    async fn execute_query_put_failure_persists_no_row() {
        use arcodash_core::test_support::blob::FailingBlobStore;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("store-fail-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("store-fail-stub", 2)));
        let ds = seed_data_source_of_type(&db, org.id, "store-fail-stub").await;
        let store = Arc::new(FailingBlobStore::new("AccessDenied"));
        let ctx = ctx_with_store(
            ctx_with_registry(db.clone(), redis, registry, 100),
            store.clone(),
        );

        let Err(err) = run_query(&ctx, &job(org.id, ds.id), None).await else {
            panic!("a failed put must fail the run");
        };
        assert!(err.to_string().contains("AccessDenied"), "{err}");
        assert_eq!(store.puts(), 1);
        assert!(result_rows(&db, org.id, ds.id).await.is_empty());
    }

    #[tokio::test]
    async fn superseded_run_does_not_overwrite_newer_object() {
        use arcodash_core::test_support::blob::MemoryBlobStore;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("store-super-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("store-super-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "store-super-stub").await;
        let j = job(org.id, ds.id);
        let future = sea_orm::prelude::DateTimeWithTimeZone::from(
            chrono::Utc::now() + chrono::Duration::seconds(5),
        );
        query_result::ActiveModel {
            org_id: Set(org.id),
            data_source_id: Set(ds.id),
            query_hash: Set(gen_query_hash(&j.query_text, &j.parameters)),
            query: Set("newer".to_string()),
            data: Set(None),
            data_location: Set(Some("newer.json.gz".to_string())),
            data_size_bytes: Set(Some(1)),
            runtime: Set(Some(0.0)),
            retrieved_at: Set(future),
            last_used_at: Set(future),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let mem = Arc::new(MemoryBlobStore::default());
        let ctx = ctx_with_store(
            ctx_with_registry(db.clone(), redis, registry, 100),
            mem.clone(),
        );

        let outcome = run_query(&ctx, &j, None)
            .await
            .unwrap()
            .expect("not cancelled");
        assert!(outcome.superseded);
        assert_eq!(mem.puts(), 0);
        let rows = result_rows(&db, org.id, ds.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].data_location.as_deref(), Some("newer.json.gz"));
    }

    /// Seed a minimal `queries` row for a job to bind to.
    async fn seed_query(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        data_source_id: i64,
        user_id: i64,
    ) -> query::Model {
        query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(data_source_id),
            name: Set(format!("bound-job-test-{}", uuid::Uuid::new_v4())),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(gen_query_hash("SELECT 1", &Value::Null)),
            api_key: Set(uuid::Uuid::new_v4().to_string()),
            user_id: Set(user_id),
            last_modified_by_id: Set(Some(user_id)),
            latest_query_data_id: Set(None),
            is_archived: Set(false),
            options: Set(serde_json::json!({})),
            tags: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// A job naming a data source from a different org must be refused: the worker itself is the
    /// enforcement boundary.
    #[tokio::test]
    async fn run_query_refuses_a_data_source_belonging_to_a_different_org() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Held for the test: a concurrent backend-suite `TestApp::cleanup()` truncating
        // `data_sources` would make the seeded row vanish.
        let _db_lock = acquire_shared_test_db_lock(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .await;
        let owner_org =
            seed_org(&db, &format!("cross-org-ds-owner-{}", uuid::Uuid::new_v4())).await;
        let attacker_org = seed_org(
            &db,
            &format!("cross-org-ds-attacker-{}", uuid::Uuid::new_v4()),
        )
        .await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("cross-org-stub", 1)));
        let ds = seed_data_source_of_type(&db, owner_org.id, "cross-org-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        let err = run_query(&ctx, &job(attacker_org.id, ds.id), None)
            .await
            .map(|_| ())
            .expect_err("a data source belonging to a different org must be refused");
        assert!(
            err.to_string().contains("does not belong to org"),
            "error must name the mismatch, not read as a generic failure: {err}"
        );
    }

    /// `write_data_source_health`'s `UPDATE` must not touch another org's row.
    #[tokio::test]
    async fn write_data_source_health_does_not_touch_a_different_orgs_row() {
        let Some((db, _redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _db_lock = acquire_shared_test_db_lock(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .await;
        let owner_org = seed_org(&db, &format!("health-owner-{}", uuid::Uuid::new_v4())).await;
        let attacker_org =
            seed_org(&db, &format!("health-attacker-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source_of_type(&db, owner_org.id, "cross-org-health-stub").await;

        write_data_source_health(
            &db,
            ds.id,
            attacker_org.id,
            chrono::Utc::now(),
            "failing",
            Some("boom"),
        )
        .await;

        let reloaded = data_source::Entity::find_by_id(ds.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            reloaded.health_state, "failing",
            "a write scoped to a different org must not change this data source's health"
        );
    }

    #[tokio::test]
    async fn sentinel_row_over_cap_is_trimmed_and_flagged_truncated() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("auto-limit-over-{}", uuid::Uuid::new_v4())).await;
        let max_rows: u32 = 5;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new(
            "auto-limit-over-stub",
            (max_rows + 1) as usize,
        )));
        let ds = seed_data_source_of_type(&db, org.id, "auto-limit-over-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, max_rows);

        let outcome = run_query(&ctx, &job(org.id, ds.id), None)
            .await
            .unwrap()
            .expect("not cancelled");
        assert_eq!(
            outcome.row_count, max_rows as i64,
            "the returned row_count must reflect the trimmed value, not the sentinel-inflated one"
        );

        let saved = query_result::Entity::find_by_id(outcome.query_result_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let data = stored_result(&ctx, &saved).await;
        assert_eq!(data["truncated"], true);
        assert_eq!(
            data["rows"].as_array().unwrap().len(),
            max_rows as usize,
            "stored rows must truncate to exactly max_rows"
        );
    }

    /// Runs a fixed-rows connector returning `returned` rows under cap `cap` with the job asking
    /// for `job_max_rows`; yields stored `(row_count, truncated)`.
    async fn run_with_job_max_rows(
        tag: &str,
        cap: u32,
        job_max_rows: Option<u32>,
        returned: usize,
    ) -> (i64, bool) {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return (-1, false);
        };
        let org = seed_org(&db, &format!("{tag}-{}", uuid::Uuid::new_v4())).await;
        let type_name: &'static str = Box::leak(format!("{tag}-stub").into_boxed_str());
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new(type_name, returned)));
        let ds = seed_data_source_of_type(&db, org.id, type_name).await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, cap);
        let mut j = job(org.id, ds.id);
        j.max_rows = job_max_rows;
        let outcome = run_query(&ctx, &j, None)
            .await
            .unwrap()
            .expect("not cancelled");
        let saved = query_result::Entity::find_by_id(outcome.query_result_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let truncated = stored_result(&ctx, &saved).await["truncated"] == true;
        (outcome.row_count, truncated)
    }

    #[tokio::test]
    async fn job_max_rows_below_server_cap_trims_to_job_value() {
        let (rows, truncated) = run_with_job_max_rows("job-max-low", 100, Some(20), 21).await;
        if rows == -1 {
            return;
        }
        assert_eq!(rows, 20);
        assert!(truncated);
    }

    #[tokio::test]
    async fn job_max_rows_above_server_cap_is_clamped() {
        let (rows, truncated) = run_with_job_max_rows("job-max-high", 5, Some(5000), 6).await;
        if rows == -1 {
            return;
        }
        assert_eq!(rows, 5, "a job cannot raise the server cap");
        assert!(truncated);
    }

    #[tokio::test]
    async fn rows_at_or_under_cap_are_not_truncated() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("auto-limit-under-{}", uuid::Uuid::new_v4())).await;
        let max_rows: u32 = 5;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new(
            "auto-limit-under-stub",
            max_rows as usize,
        )));
        let ds = seed_data_source_of_type(&db, org.id, "auto-limit-under-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, max_rows);

        let outcome = run_query(&ctx, &job(org.id, ds.id), None)
            .await
            .unwrap()
            .expect("not cancelled");
        assert_eq!(outcome.row_count, max_rows as i64);

        let saved = query_result::Entity::find_by_id(outcome.query_result_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let data = stored_result(&ctx, &saved).await;
        assert_eq!(data["truncated"], false);
        assert_eq!(data["rows"].as_array().unwrap().len(), max_rows as usize);
    }

    #[tokio::test]
    async fn connector_opted_out_of_auto_limit_is_never_truncated_even_over_cap() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("auto-limit-optout-{}", uuid::Uuid::new_v4())).await;
        let max_rows: u32 = 5;
        // This connector opts out of auto-limit: mssql results must never be trimmed.
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            auto_limit: false,
            ..StubConnector::new("auto-limit-optout-stub", (max_rows + 1) as usize)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "auto-limit-optout-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, max_rows);

        let outcome = run_query(&ctx, &job(org.id, ds.id), None)
            .await
            .unwrap()
            .expect("not cancelled");
        assert_eq!(
            outcome.row_count,
            (max_rows + 1) as i64,
            "an opted-out connector's rows must survive untouched, sentinel-sized or not"
        );

        let saved = query_result::Entity::find_by_id(outcome.query_result_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let data = stored_result(&ctx, &saved).await;
        assert_eq!(data["truncated"], false);
        assert_eq!(
            data["rows"].as_array().unwrap().len(),
            (max_rows + 1) as usize
        );
    }

    #[tokio::test]
    async fn execute_query_skips_blocked_org() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("blocked-eq-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            auto_limit: false,
            ..StubConnector::new("blocked-eq-stub", 1)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "blocked-eq-stub").await;
        let mut ctx = ctx_with_registry(db.clone(), redis, registry, 100);
        ctx.entitlements = Arc::new(crate::scheduler::test_support::BlockedByTest);

        handle_inner(&ctx, &job(org.id, ds.id)).await.unwrap();

        let count = query_result::Entity::find()
            .filter(query_result::Column::DataSourceId.eq(ds.id))
            .all(&db)
            .await
            .unwrap()
            .len();
        assert_eq!(count, 0, "a blocked org's scheduled query must not execute");
    }

    #[tokio::test]
    async fn unrestricted_entitlements_never_skip() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("unrestricted-eq-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            auto_limit: false,
            ..StubConnector::new("unrestricted-eq-stub", 1)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "unrestricted-eq-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        handle_inner(&ctx, &job(org.id, ds.id)).await.unwrap();

        let count = query_result::Entity::find()
            .filter(query_result::Column::DataSourceId.eq(ds.id))
            .all(&db)
            .await
            .unwrap()
            .len();
        assert_eq!(count, 1, "default entitlements must not skip any run");
    }

    /// A promoted editor run (`query_id: Some`, `repoint_published: false`) must, on success, write
    /// all five execution-state columns and move only the draft pointer.
    #[tokio::test]
    async fn bound_job_success_writes_all_five_state_columns_on_draft_pointer_only() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("bound-success-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            auto_limit: false,
            ..StubConnector::new("bound-success-stub", 2)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "bound-success-stub").await;
        let q = seed_query(&db, org.id, ds.id, 1).await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        let mut promoted = job(org.id, ds.id);
        promoted.query_id = Some(q.id);
        promoted.repoint_published = false;

        let outcome = run_query(&ctx, &promoted, None)
            .await
            .unwrap()
            .expect("not cancelled");

        let updated = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.latest_query_data_id,
            Some(outcome.query_result_id),
            "draft pointer must move to the new result"
        );
        assert!(
            updated.published_latest_query_data_id.is_none(),
            "repoint_published: false must never touch the published pointer"
        );
        assert_eq!(updated.last_execution_status, "Success");
        assert!(updated.last_execution_at.is_some());
        assert_eq!(updated.row_count, Some(2));
        assert!(updated.last_execution_error.is_none());
    }

    /// A bound job that fails sets `Failed`, the error and `last_execution_at` without moving
    /// either pointer.
    #[tokio::test]
    async fn bound_job_failure_writes_failed_trio_and_moves_no_pointer() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("bound-failure-{}", uuid::Uuid::new_v4())).await;
        // No connector registered for this type, so `run_query` fails and drives the failure arm.
        let registry = ConnectorRegistry::new();
        let ds = seed_data_source_of_type(&db, org.id, "bound-failure-stub").await;
        let q = seed_query(&db, org.id, ds.id, 1).await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        let mut promoted = job(org.id, ds.id);
        promoted.query_id = Some(q.id);
        promoted.repoint_published = false;

        handle_inner(&ctx, &promoted).await.unwrap();

        let updated = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.last_execution_status, "Failed");
        assert!(updated.last_execution_at.is_some());
        assert!(updated.last_execution_error.is_some());
        assert!(updated.latest_query_data_id.is_none());
        assert!(updated.published_latest_query_data_id.is_none());
    }

    async fn ds_health(db: &sea_orm::DatabaseConnection, id: i64) -> (String, Option<String>) {
        let ds = data_source::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        (ds.health_state, ds.health_error)
    }

    #[tokio::test]
    async fn successful_run_marks_data_source_health_ok() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("health-ok-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            auto_limit: false,
            ..StubConnector::new("health-ok-stub", 1)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "health-ok-stub").await;
        assert_eq!(
            ds_health(&db, ds.id).await.0,
            "unknown",
            "a never-used data source starts Unknown"
        );
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        handle_inner(&ctx, &job(org.id, ds.id)).await.unwrap();

        let (state, error) = ds_health(&db, ds.id).await;
        assert_eq!(state, "ok");
        assert!(error.is_none());
    }

    /// The `query_results` connector authorizes on `org_id` and `user_id`; the run must reach it
    /// with both.
    #[tokio::test]
    async fn query_results_run_receives_org_id_and_user_id() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Held for the test: a concurrent backend-suite `TestApp::cleanup()` truncating
        // `data_sources` would make the seeded row vanish.
        let _db_lock = acquire_shared_test_db_lock(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .await;
        let org = seed_org(&db, &format!("qr-inject-{}", uuid::Uuid::new_v4())).await;
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            seen_opts: captured.clone(),
            ..StubConnector::new("query_results", 0)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "query_results").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        let mut j = job(org.id, ds.id);
        j.user_id = 4242;
        handle_inner(&ctx, &j).await.unwrap();

        let opts = captured
            .lock()
            .unwrap()
            .clone()
            .expect("run_query was never called");
        assert_eq!(opts["org_id"], serde_json::json!(org.id));
        assert_eq!(opts["user_id"], serde_json::json!(4242));
    }

    #[tokio::test]
    async fn connect_level_failure_marks_data_source_health_failing() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("health-failing-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            fail: Some(arcodash_core::connectors::ConnectorErrorKind::Connect),
            ..StubConnector::new("health-connect-fail-stub", 0)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "health-connect-fail-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        handle_inner(&ctx, &job(org.id, ds.id)).await.unwrap();

        let (state, error) = ds_health(&db, ds.id).await;
        assert_eq!(state, "failing");
        assert!(error.unwrap().contains("connection refused"));
    }

    /// A statement-level failure (bad SQL, missing table) must not mark the data source unhealthy.
    #[tokio::test]
    async fn statement_level_failure_leaves_data_source_health_untouched() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("health-statement-{}", uuid::Uuid::new_v4())).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector {
            fail: Some(arcodash_core::connectors::ConnectorErrorKind::Statement),
            ..StubConnector::new("health-statement-fail-stub", 0)
        }));
        let ds = seed_data_source_of_type(&db, org.id, "health-statement-fail-stub").await;
        let ctx = ctx_with_registry(db.clone(), redis, registry, 100);

        handle_inner(&ctx, &job(org.id, ds.id)).await.unwrap();

        let (state, error) = ds_health(&db, ds.id).await;
        assert_eq!(
            state, "unknown",
            "a bad-query failure must never mark a data source unhealthy"
        );
        assert!(error.is_none());
    }

    /// Seeds an org + data source of the stub type and builds a context around `connector`.
    async fn run_sql_once_fixture(
        connector: StubConnector,
    ) -> Option<(ExecuteQueryContext, data_source::Model)> {
        let (db, redis) = setup().await?;
        let org = seed_org(&db, &format!("run-sql-once-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source_of_type(&db, org.id, "run-sql-once-stub").await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(connector));
        Some((ctx_with_registry(db, redis, registry, 100), ds))
    }

    #[tokio::test]
    async fn run_sql_once_applies_auto_limit_and_truncates() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let Some((ctx, ds)) = run_sql_once_fixture(StubConnector {
            auto_limit: true,
            delay_ms: 0,
            seen_query: seen.clone(),
            ..StubConnector::new("run-sql-once-stub", 4)
        })
        .await
        else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let (result, truncated) = run_sql_once(&ctx, &ds, 1, "SELECT n FROM t", 3, 5)
            .await
            .unwrap();

        assert!(truncated);
        assert_eq!(result.rows.len(), 3);
        let sent = seen.lock().unwrap().clone().unwrap();
        assert!(
            sent.to_uppercase().contains("LIMIT 4"),
            "sentinel LIMIT missing: {sent}"
        );
    }

    #[tokio::test]
    async fn run_sql_once_skips_limit_when_unsupported() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let Some((ctx, ds)) = run_sql_once_fixture(StubConnector {
            auto_limit: false,
            delay_ms: 0,
            seen_query: seen.clone(),
            ..StubConnector::new("run-sql-once-stub", 4)
        })
        .await
        else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let (result, truncated) = run_sql_once(&ctx, &ds, 1, "SELECT n FROM t", 3, 5)
            .await
            .unwrap();

        assert!(!truncated);
        assert_eq!(result.rows.len(), 4);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("SELECT n FROM t"));
    }

    #[tokio::test]
    async fn run_sql_once_times_out() {
        let Some((ctx, ds)) = run_sql_once_fixture(StubConnector {
            auto_limit: true,
            delay_ms: 3_000,
            seen_query: Arc::new(std::sync::Mutex::new(None)),
            ..StubConnector::new("run-sql-once-stub", 1)
        })
        .await
        else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let err = run_sql_once(&ctx, &ds, 1, "SELECT 1", 10, 0)
            .await
            .map(|_| ())
            .expect_err("a 0s cap must elapse");
        assert!(err.to_string().contains("timeout"), "{err}");
    }
}

#[cfg(test)]
mod entitlement_gate_tests {
    //! A run-backed execution whose org is blocked must finalize its `runs` row `skipped` (with
    //! schedule bookkeeping), not leave it `pending` and wedge `uq_runs_inflight`.
    use super::test_helpers::test_job;
    use super::*;
    use crate::scheduler::test_support::setup;
    use crate::scheduler::test_support::*;
    use sea_orm::EntityTrait;

    #[tokio::test]
    async fn blocked_org_finalizes_a_run_backed_execution_as_skipped() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-entitlement-gate-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let schedule = seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind": "interval", "every_secs": 300}),
            "skip",
            chrono::Utc::now() - chrono::Duration::seconds(30),
            2, // one failure away from auto-pause, and must reset to 0, not increment
        )
        .await;
        let run_id = seed_run(
            &db,
            org.id,
            "query",
            q.id,
            Some(schedule.id),
            Some(ds.id),
            "pending",
            None,
        )
        .await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.entitlements = Arc::new(BlockedByTest);
        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            scheduled: true,
            run_id: Some(run_id),
            ..test_job(format!("entitlement-gate-{suffix}"), org.id, ds.id)
        };

        handle_inner(&ctx, &job).await.unwrap();

        let reloaded_run = arcodash_core::entities::run::Entity::find_by_id(run_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_run.status, "skipped",
            "a blocked run-backed execution must finalize skipped, not stay pending"
        );
        assert_eq!(reloaded_run.error.as_deref(), Some("blocked by test"));

        // uq_runs_inflight is freed: a fresh pending row for the same entity is insertable.
        let fresh_run_id = seed_run(
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
        assert_ne!(fresh_run_id, run_id);

        let reloaded_schedule = arcodash_core::entities::schedule::Entity::find_by_id(schedule.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_schedule.consecutive_failures, 0,
            "a skip is not a failure — consecutive_failures must reset, not increment"
        );
        // Left as seeded: a skip doesn't recompute `next_run_at` (`claim_and_plan` already advanced
        // it); the invariant under test is `consecutive_failures` resetting to 0.
        assert_eq!(
            reloaded_schedule.next_run_at.unwrap(),
            schedule.next_run_at.unwrap(),
            "a skipped Interval finalize must leave next_run_at exactly as claim_and_plan set it, \
             not recompute it again"
        );
    }
}

#[cfg(test)]
mod pause_gate_tests {
    //! The worker-side chokepoint for `data_sources.paused_at`: a paused source never reaches the
    //! connector, and a run-backed job finalizes `skipped`.
    use super::test_helpers::{test_job, StubConnector};
    use super::*;
    use crate::scheduler::test_support::setup;
    use crate::scheduler::test_support::*;

    async fn pause(db: &sea_orm::DatabaseConnection, ds_id: i64, reason: Option<&str>) {
        exec_sql(
            db,
            "UPDATE data_sources SET paused_at = now(), pause_reason = $1 WHERE id = $2",
            [reason.into(), ds_id.into()],
        )
        .await;
    }

    #[tokio::test]
    async fn run_backed_job_against_a_paused_source_finalizes_skipped_without_running() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-pause-gate-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        pause(&db, ds.id, Some("maintenance window")).await;
        let q = seed_query(&db, org.id, ds.id).await;
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

        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("pg", 1)));
        let ctx = ExecuteQueryContext {
            connectors: Arc::new(registry),
            ..test_ctx(db.clone(), redis.clone())
        };
        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            scheduled: true,
            run_id: Some(run_id),
            ..test_job(format!("pause-gate-{suffix}"), org.id, ds.id)
        };

        handle_inner(&ctx, &job).await.unwrap();

        let reloaded_run = arcodash_core::entities::run::Entity::find_by_id(run_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_run.status, "skipped",
            "a paused source's run-backed execution must finalize skipped, not stay pending"
        );
        assert_eq!(
            reloaded_run.error.as_deref(),
            Some("this data source is paused: maintenance window")
        );

        let results = query_result::Entity::find()
            .filter(query_result::Column::OrgId.eq(org.id))
            .all(&db)
            .await
            .unwrap();
        assert!(
            results.is_empty(),
            "a paused source's query must never actually execute"
        );

        let reloaded_ds = data_source::Entity::find_by_id(ds.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_ds.health_state, "unknown",
            "health must be untouched"
        );
        assert!(reloaded_ds.health_checked_at.is_none());

        let fresh_run_id = seed_run(
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
        assert_ne!(
            fresh_run_id, run_id,
            "uq_runs_inflight must be freed by the skip"
        );
    }

    #[tokio::test]
    async fn interactive_job_against_a_paused_source_writes_a_job_failure_without_running() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-pause-gate-interactive-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        pause(&db, ds.id, None).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let job = test_job(format!("pause-gate-interactive-{suffix}"), org.id, ds.id);

        handle_inner(&ctx, &job).await.unwrap();

        let status = queue::get_job_status(&redis, &job.job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.status, JobStatus::Failure);
        assert_eq!(status.error.as_deref(), Some("this data source is paused"));

        let results = query_result::Entity::find()
            .filter(query_result::Column::OrgId.eq(org.id))
            .all(&db)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    /// Guards against the check inverting: an unpaused source must run normally.
    #[tokio::test]
    async fn run_backed_job_against_an_unpaused_source_runs_normally() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-pause-gate-active-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
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

        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("pg", 1)));
        let ctx = ExecuteQueryContext {
            connectors: Arc::new(registry),
            ..test_ctx(db.clone(), redis.clone())
        };

        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            scheduled: true,
            run_id: Some(run_id),
            ..test_job(format!("pause-gate-active-{suffix}"), org.id, ds.id)
        };

        handle_inner(&ctx, &job).await.unwrap();

        let reloaded_run = arcodash_core::entities::run::Entity::find_by_id(run_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded_run.status, "success");
    }
}

#[cfg(test)]
mod lease_fence_tests {
    //! Once a run's heartbeat observes the lease is gone, `run_query` must refuse to persist. The
    //! test flips the run's status mid-flight instead of waiting out a real lease expiry.
    use super::test_helpers::{test_job, StubConnector};
    use super::*;
    use crate::scheduler::test_support::ctx_with_registry;
    use crate::scheduler::test_support::setup;
    use crate::scheduler::test_support::*;
    use arcodash_core::connectors::registry::ConnectorRegistry;

    #[tokio::test]
    async fn run_query_refuses_to_persist_once_the_heartbeat_reports_the_lease_lost() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-lease-fence-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("lease-fence-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "lease-fence-test-stub").await;
        let run_id = seed_run(
            &db,
            org.id,
            "query",
            ds.id,
            None,
            Some(ds.id),
            "running",
            None,
        )
        .await;

        std::env::set_var("RUN_LEASE_SECS", "3");
        let lost_before =
            arcodash_conveyor::HEARTBEAT_LEASE_LOST.load(std::sync::atomic::Ordering::Relaxed);
        let heartbeat = run_lifecycle::spawn_heartbeat(db.clone(), run_id);

        // Flip the run to failed under the heartbeat (simulating a sweeper reclaim) so the next
        // renewal matches 0 rows.
        exec_sql(
            &db,
            "UPDATE runs SET status = 'failed' WHERE id = $1",
            [run_id.into()],
        )
        .await;

        let start = std::time::Instant::now();
        while !heartbeat.lost() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "heartbeat must observe the lease loss within one renewal period"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::env::remove_var("RUN_LEASE_SECS");
        let lost_after =
            arcodash_conveyor::HEARTBEAT_LEASE_LOST.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            lost_after > lost_before,
            "HeartbeatGuard::lost() firing must increment the process-wide counter"
        );

        let ctx = ctx_with_registry(db.clone(), redis, registry);
        let job = ExecuteQueryPayload {
            run_id: Some(run_id),
            ..test_job(format!("lease-fence-{suffix}"), org.id, ds.id)
        };

        let result = run_query(&ctx, &job, Some(&heartbeat)).await;
        assert!(
            result.is_err(),
            "run_query must refuse to persist once the heartbeat reports the lease lost"
        );

        let persisted = query_result::Entity::find()
            .filter(query_result::Column::DataSourceId.eq(ds.id))
            .all(&db)
            .await
            .unwrap();
        assert!(
            persisted.is_empty(),
            "no query_results row must be written once the lease was lost mid-execution"
        );
    }
}

#[cfg(test)]
mod write_guard_tests {
    //! The advisory lock and the guarded last_execution_* write protect against out-of-order
    //! concurrent executions of the same query.
    use super::test_helpers::{test_job, StubConnector};
    use super::*;
    use crate::scheduler::test_support::ctx_with_registry;
    use crate::scheduler::test_support::setup;
    use crate::scheduler::test_support::*;
    use arcodash_core::connectors::registry::ConnectorRegistry;
    use sea_orm::ActiveValue::Set;

    /// The advisory lock must serialize concurrent executions of the same (org, data source,
    /// query_hash) so they can't both INSERT.
    #[tokio::test]
    async fn concurrent_executions_of_the_same_query_never_produce_duplicate_result_rows() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-advisory-lock-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("write-guard-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "write-guard-test-stub").await;

        let ctx = Arc::new(ctx_with_registry(db.clone(), redis, registry));
        let job = Arc::new(test_job(format!("advisory-lock-{suffix}"), org.id, ds.id));

        let (ctx1, job1) = (ctx.clone(), job.clone());
        let (ctx2, job2) = (ctx.clone(), job.clone());
        let (r1, r2) = tokio::join!(run_query(&ctx1, &job1, None), run_query(&ctx2, &job2, None),);
        r1.unwrap();
        r2.unwrap();

        let query_hash = gen_query_hash(&job.query_text, &job.parameters);
        let rows = query_result::Entity::find()
            .filter(query_result::Column::OrgId.eq(org.id))
            .filter(query_result::Column::DataSourceId.eq(ds.id))
            .filter(query_result::Column::QueryHash.eq(query_hash))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "two concurrent executions of the same query must never both INSERT — the advisory \
             lock must serialize them into one find-then-update/insert at a time"
        );
    }

    /// An execution finishing after a newer outcome was recorded must not downgrade
    /// `last_execution_status`/`_at`.
    #[tokio::test]
    async fn a_stale_execution_does_not_overwrite_a_newer_recorded_status() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-write-guard-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("write-guard-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "write-guard-test-stub").await;
        let q = seed_query(&db, org.id, ds.id).await;

        // Simulate a newer execution having already recorded its outcome (timestamped in the
        // future).
        let future = chrono::Utc::now() + chrono::Duration::minutes(10);
        exec_sql(
            &db,
            "UPDATE queries SET last_execution_status = 'Success', last_execution_at = $1 WHERE id = $2",
            [future.into(), q.id.into()],
        )
        .await;

        let ctx = ctx_with_registry(db.clone(), redis, registry);
        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            ..test_job(format!("write-guard-{suffix}"), org.id, ds.id)
        };

        run_query(&ctx, &job, None)
            .await
            .unwrap()
            .expect("not cancelled");

        let reloaded = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.last_execution_at.unwrap().timestamp(),
            future.timestamp(),
            "the guarded write must not move last_execution_at backward past an already-recorded \
             newer outcome"
        );
        assert_eq!(
            reloaded.last_execution_status, "Success",
            "status must stay whatever the newer execution recorded, not be overwritten"
        );
    }

    /// An older execution of different text (a different `query_results` row) finishing late must
    /// move neither the pointer nor the status.
    #[tokio::test]
    async fn an_older_execution_does_not_repoint_a_query_past_a_newer_one() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-repoint-race-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("write-guard-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "write-guard-test-stub").await;
        let q = seed_query(&db, org.id, ds.id).await;
        let ctx = ctx_with_registry(db.clone(), redis, registry);

        let newer_job = ExecuteQueryPayload {
            query_text: "SELECT 'newer'".to_string(),
            query_id: Some(q.id),
            ..test_job(format!("repoint-race-newer-{suffix}"), org.id, ds.id)
        };
        let newer_outcome = run_query(&ctx, &newer_job, None)
            .await
            .unwrap()
            .expect("not cancelled");

        let after_newer = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_newer.latest_query_data_id,
            Some(newer_outcome.query_result_id)
        );

        // The older execution's "now" must land strictly before the newer one's.
        let pushed_forward = chrono::Utc::now() + chrono::Duration::minutes(10);
        exec_sql(
            &db,
            "UPDATE queries SET last_execution_at = $1 WHERE id = $2",
            [pushed_forward.into(), q.id.into()],
        )
        .await;

        // The older execution, with different text (distinct query_hash), finalizes last.
        let older_job = ExecuteQueryPayload {
            query_text: "SELECT 'older'".to_string(),
            query_id: Some(q.id),
            ..test_job(format!("repoint-race-older-{suffix}"), org.id, ds.id)
        };
        let older_outcome = run_query(&ctx, &older_job, None)
            .await
            .unwrap()
            .expect("not cancelled");
        assert_ne!(
            older_outcome.query_result_id, newer_outcome.query_result_id,
            "different query text must produce a distinct query_results row"
        );

        let reloaded = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.latest_query_data_id,
            Some(newer_outcome.query_result_id),
            "the pointer must stay on the newer execution's result, not repoint to the late-\
             finishing older one"
        );
        assert_eq!(
            reloaded.last_execution_at.unwrap().timestamp(),
            pushed_forward.timestamp(),
            "last_execution_at must stay at the newer execution's (pushed-forward) timestamp"
        );
    }

    /// A slower execution that started before an existing result was retrieved must not overwrite
    /// it even though it commits later.
    #[tokio::test]
    async fn a_slower_execution_does_not_overwrite_a_newer_cached_result() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-supersede-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("write-guard-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "write-guard-test-stub").await;
        let q = seed_query(&db, org.id, ds.id).await;

        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            ..test_job(format!("supersede-{suffix}"), org.id, ds.id)
        };
        let query_hash = gen_query_hash(&job.query_text, &job.parameters);
        let future_retrieved_at = sea_orm::prelude::DateTimeWithTimeZone::from(
            chrono::Utc::now() + chrono::Duration::seconds(5),
        );
        let seeded = query_result::ActiveModel {
            org_id: Set(org.id),
            data_source_id: Set(ds.id),
            query_hash: Set(query_hash),
            query: Set("existing fresher result".to_string()),
            data: Set(Some(serde_json::json!({"rows": [{"n": 999}]}))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.0)),
            retrieved_at: Set(future_retrieved_at),
            last_used_at: Set(future_retrieved_at),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let ctx = ctx_with_registry(db.clone(), redis, registry);
        run_query(&ctx, &job, None)
            .await
            .unwrap()
            .expect("not cancelled");

        let reloaded = query_result::Entity::find_by_id(seeded.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.data, seeded.data,
            "a slower execution's data must not overwrite the already-fresher cached result"
        );
        assert_eq!(
            reloaded.retrieved_at.timestamp(),
            future_retrieved_at.timestamp(),
            "retrieved_at must not be reset to this execution's own (earlier-started) time"
        );

        let reloaded_query = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_query.latest_query_data_id, None,
            "a superseded run must not repoint the query at its own stale result"
        );
        assert_eq!(
            reloaded_query.last_execution_at, None,
            "a superseded run must not advance last_execution_at either"
        );
    }

    /// The common case: an execution that started after the existing row was last retrieved
    /// still overwrites it.
    #[tokio::test]
    async fn a_normal_sequential_rerun_still_updates_the_cached_result() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-sequential-rerun-{suffix}")).await;
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(StubConnector::new("write-guard-test-stub", 1)));
        let ds = seed_data_source_of_type(&db, org.id, "write-guard-test-stub").await;
        let q = seed_query(&db, org.id, ds.id).await;
        let ctx = ctx_with_registry(db.clone(), redis, registry);

        let job = ExecuteQueryPayload {
            query_id: Some(q.id),
            ..test_job(format!("sequential-rerun-{suffix}"), org.id, ds.id)
        };

        let first = run_query(&ctx, &job, None)
            .await
            .unwrap()
            .expect("not cancelled");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let second = run_query(&ctx, &job, None)
            .await
            .unwrap()
            .expect("not cancelled");

        assert_eq!(
            first.query_result_id, second.query_result_id,
            "the same query keeps updating the same query_results row"
        );
        assert!(!second.superseded);

        let reloaded_query = query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_query.latest_query_data_id,
            Some(second.query_result_id),
            "a normal sequential rerun must still repoint the query at the latest result"
        );
    }
}
