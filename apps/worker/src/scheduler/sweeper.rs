//! Deadline-loop sweeps: reclaim runs whose worker crashed, and retry runs that never reached a
//! queue.

use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, QueryFilter,
    Statement, TransactionTrait,
};

use arcodash_core::entities::{data_source_upload, monitor, run};
use arcodash_core::queue::{JobMessage, QueueName};

use crate::delivery_lifecycle;
use crate::handlers::evaluate_monitor::{evaluate_and_finalize, resolve_monitor_result};
use crate::handlers::execute_query::ExecuteQueryContext;
use crate::run_lifecycle::{self, RunOutcome, RunStatus};

use super::deadline::{self, abandoned_pending_secs, STALE_PENDING_SECS};

/// Caps rows per sweep per cycle so a backlog can't make the deadline tick unbounded.
pub fn sweep_batch_limit() -> i64 {
    arcodash_core::config::env_parsed("SWEEP_BATCH_LIMIT", 500)
}

/// Load one sweep batch, or log and give up this cycle (counted uniformly as a failed fetch).
async fn load_batch(
    db: &DatabaseConnection,
    stmt: Statement,
    what: &str,
) -> Option<Vec<sea_orm::QueryResult>> {
    match db.query_all_raw(stmt).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            crate::health::SWEEP_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(error = %e, what, "sweeper: failed to load a sweep batch");
            None
        }
    }
}

/// Bumps `counter` and returns 1 when a sweep acted on a row, else 0.
fn tally(counter: &std::sync::atomic::AtomicU64, acted: bool) -> usize {
    if acted {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    usize::from(acted)
}

/// Fails `running` runs whose heartbeat stopped renewing `lease_expires_at`, vacating their
/// `uq_runs_inflight` slot. Excludes runs parked on `waiting_on_run_id` (see
/// [`sweep_waiting_runs`]). Returns the number reclaimed.
pub async fn sweep_expired_leases(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM runs \
             WHERE status = 'running' AND lease_expires_at < now() AND waiting_on_run_id IS NULL \
             ORDER BY id LIMIT $1",
            [sweep_batch_limit().into()],
        ),
        "expired-lease runs",
    )
    .await
    else {
        return 0;
    };

    let mut reclaimed = 0usize;
    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            continue;
        };
        tracing::warn!(run_id = id, "sweeper: reclaiming run with an expired lease");
        let outcome = RunOutcome::terminal(RunStatus::Failed, Some("worker lost".to_string()));
        match run_lifecycle::finalize_run(ctx, id, outcome).await {
            Ok(acted) => reclaimed += tally(&crate::health::SWEEPER_RECLAIMED_EXPIRED_LEASE, acted),
            Err(e) => {
                tracing::error!(run_id = id, error = %e, "sweeper: failed to finalize an expired-lease run");
            }
        }
    }
    reclaimed
}

/// Finalizes as `timed_out` a `pending` run whose `enqueued_at` is older than
/// [`abandoned_pending_secs`] (its queue message was lost). Keyed on `enqueued_at`, not
/// `created_at`: never-enqueued rows belong to [`requeue_stale_pending`]. Goes through
/// [`run_lifecycle::finalize_run`] for the full terminal treatment (slot release, job status,
/// fan-out).
pub async fn fail_abandoned_pending(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, entity_type, entity_id, data_source_id, enqueued_at FROM runs \
             WHERE status = 'pending' AND enqueued_at IS NOT NULL \
               AND enqueued_at < now() - ($1 || ' seconds')::interval \
             ORDER BY id LIMIT $2",
            [abandoned_pending_secs().into(), sweep_batch_limit().into()],
        ),
        "abandoned pending runs",
    )
    .await
    else {
        return 0;
    };

    let mut reclaimed = 0usize;
    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            tracing::error!("sweeper: malformed abandoned-pending run row, id column failed to decode, skipping");
            continue;
        };
        let (Ok(entity_type), Ok(entity_id), Ok(data_source_id), Ok(enqueued_at)) = (
            row.try_get::<String>("", "entity_type"),
            row.try_get::<i64>("", "entity_id"),
            row.try_get::<Option<i64>>("", "data_source_id"),
            row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "enqueued_at"),
        ) else {
            tracing::error!(
                run_id = id,
                "sweeper: malformed abandoned-pending run row, skipping"
            );
            continue;
        };
        let age_secs = (Utc::now() - enqueued_at.with_timezone(&Utc)).num_seconds();
        tracing::error!(
            run_id = id,
            entity_type = %entity_type,
            entity_id,
            ?data_source_id,
            ?enqueued_at,
            age_secs,
            "sweeper: enqueued but never claimed — its queue message was lost"
        );
        match run_lifecycle::finalize_pending_run(
            ctx,
            id,
            RunOutcome::terminal(
                RunStatus::TimedOut,
                Some(format!(
                    "never picked up off the queue: enqueued {age_secs}s ago, no worker ever claimed it"
                )),
            ),
        )
        .await
        {
            Ok(acted) => {
                reclaimed += tally(&crate::health::SWEEPER_RECLAIMED_ABANDONED_PENDING, acted);
            }
            Err(e) => {
                tracing::error!(run_id = id, error = %e, "sweeper: failed to finalize an abandoned pending run");
            }
        }
    }
    reclaimed
}

/// Fleet-wide cooldown between re-pushes of the same already-enqueued row; keeps duplicate
/// envelopes down while recovery still lands within minutes.
const REQUEUE_COOLDOWN_SECS: u64 = 300;

/// Retries `pending` runs older than [`STALE_PENDING_SECS`] via `try_enqueue_run`, which re-checks
/// the concurrency cap. Already-enqueued rows are gated behind [`REQUEUE_COOLDOWN_SECS`] in Redis
/// to avoid duplicate envelopes; never-enqueued rows retry every sweep.
pub async fn requeue_stale_pending(ctx: &ExecuteQueryContext) -> usize {
    let rows: Vec<run::Model> = match run::Entity::find()
        .from_raw_sql(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT * FROM runs \
             WHERE status = 'pending' \
               AND created_at < now() - ($1 || ' seconds')::interval \
             ORDER BY id LIMIT $2",
            [STALE_PENDING_SECS.into(), sweep_batch_limit().into()],
        ))
        .all(&ctx.db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            crate::health::SWEEP_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(error = %e, "sweeper: failed to load stale pending runs");
            return 0;
        }
    };

    let mut reclaimed = 0usize;
    for run in rows {
        let id = run.id;
        if run.enqueued_at.is_some() {
            let mut conn = ctx.redis.clone();
            let key = format!("arcodash:requeue-cooldown:run:{id}");
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
                    tracing::warn!(run_id = id, error = %e, "sweeper: requeue-cooldown check failed, re-enqueuing anyway");
                }
            }
        }
        match deadline::try_enqueue_run(ctx, &run).await {
            Ok(acted) => reclaimed += tally(&crate::health::SWEEPER_RECLAIMED_STALE_PENDING, acted),
            Err(e) => {
                tracing::warn!(run_id = id, error = %e, "sweeper: failed to re-enqueue a stale pending run");
            }
        }
    }
    reclaimed
}

/// Governs monitor runs parked on `waiting_on_run_id`: re-enqueues the resume if the target already
/// went terminal, fails the run past `MONITOR_WAIT_TIMEOUT`, and detaches at the monitor's own
/// `max_fresh_wait_secs` to evaluate the last result as stale. Exactly one of this sweep, the
/// finalizer's fan-out and the inline re-read wins `detach_cas` for a waiter.
pub async fn sweep_waiting_runs(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT w.id AS waiter_id, w.org_id, w.entity_id AS monitor_id, w.started_at, \
                    w.waiting_on_run_id AS target_id, t.status AS target_status \
             FROM runs w LEFT JOIN runs t ON t.id = w.waiting_on_run_id \
             WHERE w.status = 'running' AND w.waiting_on_run_id IS NOT NULL \
             ORDER BY w.id LIMIT $1",
            [sweep_batch_limit().into()],
        ),
        "waiting runs",
    )
    .await
    else {
        return 0;
    };

    // One batched monitor lookup for the cycle's rows (bounded by `sweep_batch_limit`).
    let monitor_ids: std::collections::HashSet<i64> = rows
        .iter()
        .filter_map(|row| row.try_get::<i64>("", "monitor_id").ok())
        .collect();
    let max_fresh_wait: std::collections::HashMap<i64, i64> = if monitor_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        monitor::Entity::find()
            .filter(monitor::Column::Id.is_in(monitor_ids))
            .all(&ctx.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let options = arcodash_core::models::parse_monitor_options(m.id, &m.options);
                options.max_fresh_wait_secs.map(|secs| (m.id, secs))
            })
            .collect()
    };

    let now = Utc::now();
    let hard_timeout = deadline::monitor_wait_timeout_secs();
    let mut acted = 0usize;

    for row in rows {
        let Ok(waiter_id) = row.try_get::<i64>("", "waiter_id") else {
            tracing::error!(
                "sweeper: malformed waiting-run row, waiter_id column failed to decode, skipping"
            );
            continue;
        };
        let (Ok(org_id), Ok(monitor_id), Ok(target_id)) = (
            row.try_get::<i64>("", "org_id"),
            row.try_get::<i64>("", "monitor_id"),
            row.try_get::<i64>("", "target_id"),
        ) else {
            tracing::error!(waiter_id, "sweeper: malformed waiting-run row, skipping");
            continue;
        };
        // A running row always has started_at; defensive skip if not.
        let Ok(Some(started_at)) =
            row.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "started_at")
        else {
            continue;
        };
        let target_status: Option<String> = row.try_get("", "target_status").unwrap_or(None);
        let waited = (now - started_at.with_timezone(&Utc)).num_seconds().max(0);
        let target_terminal = matches!(
            target_status.as_deref(),
            Some("success" | "failed" | "timed_out" | "cancelled" | "skipped")
        ) || target_status.is_none();

        if target_terminal {
            if run_lifecycle::detach_cas(&ctx.db, waiter_id, target_id)
                .await
                .unwrap_or(false)
            {
                acted += 1;
                let status = target_status.unwrap_or_else(|| "failed".to_string());
                enqueue_resume(ctx, org_id, waiter_id, target_id, &status).await;
            }
            continue;
        }

        if waited >= hard_timeout {
            if run_lifecycle::detach_cas(&ctx.db, waiter_id, target_id)
                .await
                .unwrap_or(false)
            {
                acted += 1;
                match run_lifecycle::finalize_run_with_retry(
                    ctx,
                    waiter_id,
                    RunOutcome::terminal(
                        RunStatus::Failed,
                        Some("timed out waiting for fresh data".to_string()),
                    ),
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!(
                            run_id = waiter_id,
                            "sweeper: hard-timeout waiter already went terminal under a racing finalizer"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            run_id = waiter_id,
                            error = %e,
                            "sweeper: failed to record a hard-timeout waiter as failed"
                        );
                    }
                }
            }
            continue;
        }

        if let Some(&n) = max_fresh_wait.get(&monitor_id) {
            if waited >= n
                && run_lifecycle::detach_cas(&ctx.db, waiter_id, target_id)
                    .await
                    .unwrap_or(false)
            {
                acted += 1;
                stale_fallback_eval(ctx, org_id, monitor_id, waiter_id).await;
            }
        }
    }
    acted
}

async fn enqueue_resume(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    waiter_id: i64,
    query_run_id: i64,
    status: &str,
) {
    if let Err(e) = ctx
        .queue
        .enqueue(
            QueueName::Default,
            JobMessage::ResumeMonitorEvaluation {
                org_id,
                run_id: waiter_id,
                query_run_id,
                query_run_status: status.to_string(),
            },
        )
        .await
    {
        tracing::error!(run_id = waiter_id, error = %e, "sweeper: failed to enqueue ResumeMonitorEvaluation");
    }
}

/// Detach at `max_fresh_wait_secs`: evaluate the last completed result (already stale) instead of
/// waiting.
async fn stale_fallback_eval(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor_id: i64,
    waiter_id: i64,
) {
    if let Err(e) = stale_fallback_eval_inner(ctx, org_id, monitor_id, waiter_id).await {
        tracing::error!(run_id = waiter_id, error = %e, "sweeper: stale-fallback eval failed");
        match run_lifecycle::finalize_run_with_retry(
            ctx,
            waiter_id,
            RunOutcome::terminal(RunStatus::Failed, Some(format!("{e:#}"))),
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    run_id = waiter_id,
                    "sweeper: stale-fallback waiter already went terminal under a racing finalizer"
                );
            }
            Err(e) => {
                tracing::error!(
                    run_id = waiter_id,
                    error = %e,
                    "sweeper: failed to record a stale-fallback eval failure"
                );
            }
        }
    }
}

async fn stale_fallback_eval_inner(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor_id: i64,
    waiter_id: i64,
) -> anyhow::Result<()> {
    // A gone, foreign or silenced monitor has nothing to evaluate.
    let Some(monitor) = monitor::Entity::find_by_id(monitor_id)
        .one(&ctx.db)
        .await?
        .filter(|m| {
            m.org_id == org_id
                && !arcodash_core::models::parse_monitor_options(monitor_id, &m.options)
                    .is_silenced(Utc::now())
        })
    else {
        run_lifecycle::finalize_run(
            ctx,
            waiter_id,
            RunOutcome::terminal(RunStatus::Success, None),
        )
        .await?;
        return Ok(());
    };

    let resolved = resolve_monitor_result(&ctx.db, &monitor, org_id).await?;
    let evaluated = resolved.map(|(id, ts)| (id, (Utc::now() - ts).num_seconds().max(0)));
    evaluate_and_finalize(ctx, org_id, &monitor, waiter_id, evaluated).await
}

/// Backstop for a `running` monitor run with no wait link and no lease, which nothing legitimate
/// produces; finalizes it `failed` to free `uq_runs_inflight`.
pub async fn sweep_stranded_monitor_runs(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM runs \
             WHERE entity_type = 'monitor' AND status = 'running' \
               AND waiting_on_run_id IS NULL AND lease_expires_at IS NULL \
               AND started_at < now() - ($1 || ' seconds')::interval \
             ORDER BY id LIMIT $2",
            [STALE_PENDING_SECS.into(), sweep_batch_limit().into()],
        ),
        "stranded monitor runs",
    )
    .await
    else {
        return 0;
    };

    let mut reclaimed = 0usize;
    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            continue;
        };
        tracing::warn!(
            run_id = id,
            "sweeper: reclaiming a stranded monitor run with no owner"
        );
        match run_lifecycle::finalize_run(
            ctx,
            id,
            RunOutcome::terminal(
                RunStatus::Failed,
                Some("monitor run left with no owner".to_string()),
            ),
        )
        .await
        {
            Ok(acted) => {
                reclaimed += tally(
                    &crate::health::SWEEPER_RECLAIMED_STRANDED_MONITOR_RUN,
                    acted,
                );
            }
            Err(e) => {
                tracing::error!(run_id = id, error = %e, "sweeper: failed to finalize a stranded monitor run");
            }
        }
    }
    reclaimed
}

/// Longer than [`STALE_PENDING_SECS`]: a `retrying` row can legitimately sit untouched for up to
/// 150s between attempts.
const STALE_RETRYING_SECS: i64 = 240;

/// Re-enqueues `notification_deliveries` rows stuck with no live lease: `pending` past
/// [`STALE_PENDING_SECS`] or `retrying` past [`STALE_RETRYING_SECS`]. `retrying` is staled off
/// `COALESCE(started_at, created_at)` so a delivery mid-backoff isn't flagged. Always re-enqueues
/// via the default queue.
pub async fn requeue_stale_pending_notifications(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT nd.id, nd.org_id, nd.monitor_id, nd.subscription_id, nd.destination_id, \
                    nd.state, nd.status, nd.attempt_number, \
                    COALESCE(ms.user_id, m.user_id) AS subscriber_id \
             FROM notification_deliveries nd \
             JOIN monitors m ON m.id = nd.monitor_id \
             LEFT JOIN monitor_subscriptions ms ON ms.id = nd.subscription_id \
             WHERE nd.lease_expires_at IS NULL \
               AND ((nd.status = 'pending' \
                     AND nd.created_at < now() - ($1 || ' seconds')::interval) \
                 OR (nd.status = 'retrying' \
                     AND COALESCE(nd.started_at, nd.created_at) < now() - ($2 || ' seconds')::interval)) \
             ORDER BY nd.id LIMIT $3",
            [
                STALE_PENDING_SECS.into(),
                STALE_RETRYING_SECS.into(),
                sweep_batch_limit().into(),
            ],
        ),
        "stale pending notification deliveries",
    )
    .await
    else {
        return 0;
    };

    let mut reclaimed = 0usize;
    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            tracing::error!(
                "sweeper: malformed stale-pending notification delivery row, id column failed to decode, skipping"
            );
            continue;
        };
        let (
            Ok(org_id),
            Ok(monitor_id),
            Ok(destination_id),
            Ok(state),
            Ok(status),
            Ok(attempt_number),
        ) = (
            row.try_get::<i64>("", "org_id"),
            row.try_get::<i64>("", "monitor_id"),
            row.try_get::<Option<i64>>("", "destination_id"),
            row.try_get::<String>("", "state"),
            row.try_get::<String>("", "status"),
            row.try_get::<i32>("", "attempt_number"),
        )
        else {
            tracing::error!(
                delivery_id = id,
                "sweeper: malformed stale-pending notification delivery row, skipping"
            );
            continue;
        };
        let subscription_id: Option<i64> = row.try_get("", "subscription_id").unwrap_or(None);
        let subscriber_id: Option<i64> = row.try_get("", "subscriber_id").unwrap_or(None);
        // A monitor always has a user_id; skip defensively rather than panic if subscriber_id is
        // NULL.
        let Some(subscriber_id) = subscriber_id else {
            tracing::warn!(
                delivery_id = id,
                "sweeper: cannot recover stale-pending delivery, no subscriber_id resolvable"
            );
            continue;
        };
        // A recovered `retrying` row already recorded the failed attempt, so the next send is one
        // past it; `pending` uses its own (floored at 1).
        let attempt = if status == "retrying" {
            (attempt_number + 1).cast_unsigned()
        } else {
            attempt_number.max(1).cast_unsigned()
        };

        let explanation = if state == "triggered" {
            crate::handlers::explain_alert::latest_explanation(
                &ctx.db,
                ctx.storage.as_ref(),
                org_id,
                monitor_id,
            )
            .await
        } else {
            None
        };
        match ctx
            .queue
            .enqueue(
                QueueName::Default,
                JobMessage::SendNotification {
                    org_id,
                    monitor_id,
                    destination_id: destination_id.unwrap_or(-1),
                    new_state: state,
                    subscriber_id,
                    subscription_id,
                    attempt,
                    run_id: Some(id),
                    observed: None,
                    explanation,
                },
            )
            .await
        {
            Ok(()) => {
                reclaimed += tally(
                    &crate::health::SWEEPER_RECLAIMED_STALE_PENDING_NOTIFICATIONS,
                    true,
                );
            }
            Err(e) => {
                tracing::error!(delivery_id = id, error = %e, "sweeper: failed to re-enqueue stale pending notification delivery");
            }
        }
    }
    reclaimed
}

/// Same recovery for `mail_deliveries` (`pending` and `retrying`); the full `send_mail` payload lives
/// in the row.
pub async fn requeue_stale_pending_mail(ctx: &ExecuteQueryContext) -> usize {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, org_id, task_type, entity_id, to_email, subject, body_html \
             FROM mail_deliveries \
             WHERE lease_expires_at IS NULL \
               AND ((status = 'pending' \
                     AND created_at < now() - ($1 || ' seconds')::interval) \
                 OR (status = 'retrying' \
                     AND COALESCE(started_at, created_at) < now() - ($2 || ' seconds')::interval)) \
             ORDER BY id LIMIT $3",
            [
                STALE_PENDING_SECS.into(),
                STALE_RETRYING_SECS.into(),
                sweep_batch_limit().into(),
            ],
        ),
        "stale pending mail deliveries",
    )
    .await
    else {
        return 0;
    };

    let mut reclaimed = 0usize;
    for row in rows {
        let Ok(id) = row.try_get::<i64>("", "id") else {
            tracing::error!(
                "sweeper: malformed stale-pending mail delivery row, id column failed to decode, skipping"
            );
            continue;
        };
        let (Ok(org_id), Ok(task_type)) = (
            row.try_get::<i64>("", "org_id"),
            row.try_get::<String>("", "task_type"),
        ) else {
            tracing::error!(
                delivery_id = id,
                "sweeper: malformed stale-pending mail delivery row, skipping"
            );
            continue;
        };

        let job = match task_type.as_str() {
            "send_digest" => {
                let Ok(Some(user_id)) = row.try_get::<Option<i64>>("", "entity_id") else {
                    tracing::warn!(
                        delivery_id = id,
                        "sweeper: send_digest row missing entity_id, cannot recover"
                    );
                    continue;
                };
                JobMessage::SendDigest {
                    org_id,
                    user_id,
                    delivery_id: Some(id),
                }
            }
            "send_mail" => {
                let (Ok(Some(to)), Ok(Some(subject)), Ok(Some(body_html))) = (
                    row.try_get::<Option<String>>("", "to_email"),
                    row.try_get::<Option<String>>("", "subject"),
                    row.try_get::<Option<String>>("", "body_html"),
                ) else {
                    tracing::warn!(
                        delivery_id = id,
                        "sweeper: send_mail row missing payload, cannot recover"
                    );
                    continue;
                };
                JobMessage::SendMail {
                    org_id,
                    to,
                    subject,
                    body_html,
                    delivery_id: Some(id),
                }
            }
            other => {
                tracing::error!(
                    delivery_id = id,
                    task_type = other,
                    "sweeper: unknown mail_deliveries task_type, skipping"
                );
                continue;
            }
        };
        match ctx.queue.enqueue(QueueName::Default, job).await {
            Ok(()) => {
                reclaimed += tally(&crate::health::SWEEPER_RECLAIMED_STALE_PENDING_MAIL, true);
            }
            Err(e) => {
                tracing::error!(delivery_id = id, error = %e, "sweeper: failed to re-enqueue stale pending {task_type}");
            }
        }
    }
    reclaimed
}

/// Reclaims `running` delivery rows (either table) whose lease expired back to `pending`, so the
/// stale-pending scans can re-enqueue them.
#[allow(
    clippy::cast_possible_truncation,
    reason = "per-cycle reclaimed row counts fit in usize"
)]
pub async fn reclaim_expired_delivery_leases(ctx: &ExecuteQueryContext) -> usize {
    let mut reclaimed = 0usize;
    for table in [
        delivery_lifecycle::NOTIFICATION_DELIVERIES,
        delivery_lifecycle::MAIL_DELIVERIES,
    ] {
        match delivery_lifecycle::reclaim_expired_leases(&ctx.db, table).await {
            Ok(n) => reclaimed += n as usize,
            Err(e) => {
                tracing::error!(table, error = %e, "sweeper: failed to reclaim expired delivery leases");
            }
        }
    }
    reclaimed
}

/// Age past which an unlinked upload is considered abandoned.
const ORPHAN_UPLOAD_AGE_SECS: i64 = 24 * 3600;

/// A row never linked to a data source and older than [`ORPHAN_UPLOAD_AGE_SECS`].
fn is_orphan_upload(
    data_source_id: Option<i64>,
    created_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    data_source_id.is_none() && (now - created_at).num_seconds() >= ORPHAN_UPLOAD_AGE_SECS
}

/// Runs from `run_aux_ticks` against the admin/pooled db so it sees every org. Deletes the object
/// before the row, so a failed object delete is retried next tick.
pub async fn sweep_orphan_uploads(ctx: &ExecuteQueryContext) {
    let Some(rows) = load_batch(
        &ctx.db,
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, object_key, data_source_id, created_at FROM data_source_uploads \
             WHERE data_source_id IS NULL \
               AND created_at < now() - ($1 || ' seconds')::interval",
            [ORPHAN_UPLOAD_AGE_SECS.into()],
        ),
        "orphaned uploads",
    )
    .await
    else {
        return;
    };

    let now = Utc::now();
    for row in rows {
        let Ok(id) = row.try_get::<uuid::Uuid>("", "id") else {
            tracing::error!(
                "sweeper: malformed orphaned-upload row, id column failed to decode, skipping"
            );
            continue;
        };
        let (Ok(object_key), Ok(data_source_id), Ok(created_at)) = (
            row.try_get::<String>("", "object_key"),
            row.try_get::<Option<i64>>("", "data_source_id"),
            row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "created_at"),
        ) else {
            tracing::error!(upload_id = %id, "sweeper: malformed orphaned-upload row, skipping");
            continue;
        };
        // Defensive re-check before a destructive delete, in case the SQL filter and this predicate
        // drift apart.
        if !is_orphan_upload(data_source_id, created_at.with_timezone(&Utc), now) {
            tracing::error!(upload_id = %id, "sweeper: orphaned-upload query returned a non-orphan row, skipping");
            continue;
        }
        if let Err(e) = ctx.store.delete(&object_key).await {
            tracing::error!(upload_id = %id, error = %e, "sweeper: failed to delete orphaned upload object, leaving row for retry");
            continue;
        }
        if let Err(e) = data_source_upload::Entity::delete_by_id(id)
            .exec(&ctx.db)
            .await
        {
            tracing::error!(upload_id = %id, error = %e, "sweeper: failed to delete orphaned upload row after its object was already deleted");
        }
    }
}

/// A stored result row selected for expiry.
struct ExpiredCandidate {
    id: i64,
    org_id: i64,
    data_source_id: i64,
    query_hash: String,
}

/// Outcome of expiring one stored result.
#[derive(Debug, PartialEq, Eq)]
enum Expiry {
    Deleted,
    Kept,
    StorageFailed,
}

/// Counts from one retention sweep tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ResultSweepStats {
    pub deleted: usize,
    pub skipped_orgs: usize,
    pub failed_orgs: usize,
}

/// Deletes one stale result's row and object under the same advisory lock `execute_query` holds
/// while writing, re-checking staleness so a concurrent read or refresh wins. On an object-delete
/// failure the row is kept for the next sweep.
async fn expire_result(
    ctx: &ExecuteQueryContext,
    storage: &arcodash_core::org_storage::OrgStorage,
    candidate: &ExpiredCandidate,
    retention_days: i64,
) -> Result<Expiry, sea_orm::DbErr> {
    let txn = ctx.db.begin().await?;
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [format!(
            "{}:{}:{}",
            candidate.org_id, candidate.data_source_id, candidate.query_hash
        )
        .into()],
    ))
    .await?;
    let deleted = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM query_results \
             WHERE id = $1 AND data IS NULL \
               AND last_used_at < now() - make_interval(days => $2::int) \
             RETURNING data_location",
            [candidate.id.into(), retention_days.into()],
        ))
        .await?;
    let Some(row) = deleted else {
        txn.rollback().await?;
        return Ok(Expiry::Kept);
    };
    if let Some(location) = row.try_get::<Option<String>>("", "data_location")? {
        match storage
            .delete(arcodash_core::org_storage::StorageArea::Results, &location)
            .await
        {
            Ok(()) | Err(arcodash_core::org_storage::StorageError::NotFound) => {}
            Err(e) => {
                tracing::warn!(org_id = candidate.org_id, result_id = candidate.id, error = %e, "sweeper: failed to delete expired result object, keeping row for retry");
                txn.rollback().await?;
                return Ok(Expiry::StorageFailed);
            }
        }
    }
    txn.commit().await?;
    Ok(Expiry::Deleted)
}

/// Pages scanned per tick; bounds the work of one sweep.
pub(crate) const RESULT_SWEEP_MAX_PAGES: usize = 20;

/// Consecutive database errors while expiring rows after which a tick gives up.
const RESULT_SWEEP_MAX_DB_ERRORS: usize = 20;

/// Position in the `(last_used_at, id)` scan order where the next sweep tick resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepCursor {
    pub last_used_at: sea_orm::prelude::DateTimeWithTimeZone,
    pub id: i64,
}

/// Deletes stored results (row and object) unused for `result_retention_days`. Skips legacy
/// inline rows and organizations whose bucket the operator does not manage. Returns the number of
/// rows deleted.
#[cfg(test)]
pub async fn sweep_expired_results(ctx: &ExecuteQueryContext) -> usize {
    sweep_expired_results_with_limit(ctx, sweep_batch_limit(), RESULT_SWEEP_MAX_PAGES, None)
        .await
        .0
        .deleted
}

/// Sweeps in keyset-ordered pages of `limit` rows, at most `max_pages`, resuming after `start`.
/// Returns the cursor for the next tick: `None` once the scan reached the end (or a page failed
/// to load), so rows that are always skipped cannot starve those behind them.
pub(crate) async fn sweep_expired_results_with_limit(
    ctx: &ExecuteQueryContext,
    limit: i64,
    max_pages: usize,
    start: Option<SweepCursor>,
) -> (ResultSweepStats, Option<SweepCursor>) {
    let retention_days = ctx.result_retention_days;
    let mut stats = ResultSweepStats::default();
    let mut storages: std::collections::HashMap<
        i64,
        Option<arcodash_core::org_storage::OrgStorage>,
    > = std::collections::HashMap::new();
    let mut failed_orgs: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut cursor = start;
    let mut db_errors = 0usize;

    for _ in 0..max_pages {
        let (after_ts, after_id): (String, i64) = match cursor {
            Some(c) => (
                c.last_used_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                c.id,
            ),
            None => ("-infinity".to_string(), 0),
        };
        let Some(rows) = load_batch(
            &ctx.db,
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id, org_id, data_source_id, query_hash, last_used_at FROM query_results \
                 WHERE data IS NULL AND data_location IS NOT NULL \
                   AND last_used_at < now() - make_interval(days => $1::int) \
                   AND last_used_at >= $3::text::timestamptz \
                   AND (last_used_at, id) > ($3::text::timestamptz, $4::bigint) \
                 ORDER BY last_used_at, id LIMIT $2",
                [
                    retention_days.into(),
                    limit.into(),
                    after_ts.into(),
                    after_id.into(),
                ],
            ),
            "expired results",
        )
        .await
        else {
            return (stats, None);
        };
        let page_len = rows.len();

        for row in rows {
            let (Ok(id), Ok(last_used_at)) = (
                row.try_get::<i64>("", "id"),
                row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "last_used_at"),
            ) else {
                tracing::error!("sweeper: malformed expired-result row, skipping");
                continue;
            };
            cursor = Some(SweepCursor { last_used_at, id });
            let (Ok(org_id), Ok(data_source_id), Ok(query_hash)) = (
                row.try_get::<i64>("", "org_id"),
                row.try_get::<i64>("", "data_source_id"),
                row.try_get::<String>("", "query_hash"),
            ) else {
                tracing::error!("sweeper: malformed expired-result row, skipping");
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = storages.entry(org_id) {
                let usable = match ctx.storage.resolve(&ctx.db, org_id).await {
                    Ok(s) if s.manages_retention() && !s.writes_paused() => Some(s),
                    Ok(_) => {
                        stats.skipped_orgs += 1;
                        None
                    }
                    Err(e) => {
                        tracing::warn!(org_id, error = %e, "sweeper: cannot resolve storage for result retention, skipping org");
                        stats.skipped_orgs += 1;
                        None
                    }
                };
                slot.insert(usable);
            }
            let Some(Some(storage)) = storages.get(&org_id) else {
                continue;
            };
            let candidate = ExpiredCandidate {
                id,
                org_id,
                data_source_id,
                query_hash,
            };
            match expire_result(ctx, storage, &candidate, retention_days).await {
                Ok(outcome) => {
                    db_errors = 0;
                    match outcome {
                        Expiry::Deleted => stats.deleted += 1,
                        Expiry::Kept => {}
                        Expiry::StorageFailed => {
                            if failed_orgs.insert(org_id) {
                                stats.failed_orgs += 1;
                            }
                            storages.insert(org_id, None);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(org_id, result_id = id, error = %e, "sweeper: failed to expire result");
                    db_errors += 1;
                    if db_errors >= RESULT_SWEEP_MAX_DB_ERRORS {
                        tracing::error!(
                            "sweeper: stopping result sweep after repeated database errors"
                        );
                        return (stats, cursor);
                    }
                }
            }
        }

        if usize::try_from(limit).map_or(true, |l| page_len < l) {
            return (stats, None);
        }
    }
    (stats, cursor)
}

/// A conveyor task can go terminal while its `runs` row stays `pending`/`running` (separate tables,
/// no shared transaction); detect and count it so the churn is visible.
pub async fn reconcile_run_task_divergence(ctx: &ExecuteQueryContext) -> usize {
    let rows = match ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT t.id AS task_id, t.status AS task_status, t.task_type, \
                    r.id AS run_id, r.status AS run_status \
             FROM conveyor_tasks t \
             JOIN runs r ON r.id = CASE WHEN jsonb_typeof(t.payload->'run_id') = 'number' \
                                        THEN (t.payload->>'run_id')::bigint END \
             WHERE t.status IN ('failed', 'timed_out', 'cancelled', 'skipped') \
               AND t.finished_at > now() - interval '1 hour' \
               AND r.status IN ('pending', 'running') \
             ORDER BY t.finished_at DESC \
             LIMIT 100",
            [],
        ))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "sweeper: failed to scan for run/conveyor-task divergence");
            return 0;
        }
    };

    let mut found = 0usize;
    for row in rows {
        let Ok(task_id) = row.try_get::<i64>("", "task_id") else {
            tracing::error!(
                "sweeper: malformed run/conveyor-task divergence row, task_id column failed to decode, skipping"
            );
            continue;
        };
        let (Ok(task_status), Ok(task_type), Ok(run_id), Ok(run_status)) = (
            row.try_get::<String>("", "task_status"),
            row.try_get::<String>("", "task_type"),
            row.try_get::<i64>("", "run_id"),
            row.try_get::<String>("", "run_status"),
        ) else {
            tracing::error!(
                task_id,
                "sweeper: malformed run/conveyor-task divergence row, skipping"
            );
            continue;
        };
        tracing::warn!(
            task_id,
            task_status = %task_status,
            task_type = %task_type,
            run_id,
            run_status = %run_status,
            "sweeper: conveyor task went terminal but its run row did not follow"
        );
        crate::health::RUN_TASK_DIVERGENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        found += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    //! DB-free up to `is_orphan_upload_*`; the rest needs Postgres + Redis (skipped without
    //! `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::aux_tick_tests::EnvVarGuard;
    use crate::scheduler::test_support::*;
    use arcodash_core::queue;
    use redis::AsyncCommands;

    #[test]
    fn is_orphan_upload_requires_both_null_data_source_and_age_past_the_cutoff() {
        let now = Utc::now();
        let old_enough = now - chrono::Duration::seconds(ORPHAN_UPLOAD_AGE_SECS + 60);
        let too_recent = now - chrono::Duration::seconds(ORPHAN_UPLOAD_AGE_SECS - 60);

        assert!(is_orphan_upload(None, old_enough, now));
        assert!(
            !is_orphan_upload(None, too_recent, now),
            "an upload younger than the cutoff must not be swept even if unlinked"
        );
        assert!(
            !is_orphan_upload(Some(42), old_enough, now),
            "a linked upload must never be swept regardless of age"
        );
    }

    // Serializes `sweep_waiting_runs` tests: they share the process-global `MONITOR_WAIT_TIMEOUT`
    // and Redis queue key, and the query has no org scope. Async mutex because the guard is held
    // across `.await`.
    static SWEEP_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn sweep_expired_leases_fails_the_run_and_frees_the_inflight_slot() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-lease-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let expired = chrono::Utc::now() - chrono::Duration::seconds(30);
        let run_id = seed_query_run(&db, &q, "running", Some(expired)).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let before = crate::health::SWEEPER_RECLAIMED_EXPIRED_LEASE
            .load(std::sync::atomic::Ordering::Relaxed);
        sweep_expired_leases(&ctx).await;
        let after = crate::health::SWEEPER_RECLAIMED_EXPIRED_LEASE
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after - before >= 1,
            "the reclaim counter is process-global and unscoped, so a backlog of expired leases \
             left by other runs can move it by more than this test's own row — the row-level \
             assertions below confirm this sweep reclaimed the one row this test seeded"
        );

        let (status, error) = run_status_and_error(&db, run_id).await;
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("worker lost"));

        let insert = insert_inflight_run(&db, org.id, "query", q.id).await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the lease-expiry sweep"
        );
    }

    /// A sweep processes at most `SWEEP_BATCH_LIMIT` rows per cycle (assertions stay loose since
    /// the query is globally unscoped).
    #[tokio::test]
    async fn a_sweep_processes_at_most_the_batch_limit_in_one_cycle() {
        let _guard = SWEEP_ENV_LOCK.lock().await;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _limit_guard = EnvVarGuard::set("SWEEP_BATCH_LIMIT", "2");

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-batch-limit-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let expired = chrono::Utc::now() - chrono::Duration::seconds(30);
        let mut run_ids = Vec::new();
        for _ in 0..3 {
            let q = seed_query(&db, org.id, ds.id).await;
            let run_id = seed_query_run(&db, &q, "running", Some(expired)).await;
            run_ids.push(run_id);
        }

        let ctx = test_ctx(db.clone(), redis.clone());

        let count_failed = |db: &sea_orm::DatabaseConnection, ids: &[i64]| {
            let db = db.clone();
            let ids = ids.to_vec();
            async move {
                let mut failed = 0;
                for id in ids {
                    failed += usize::from(run_status(&db, id).await == "failed");
                }
                failed
            }
        };

        sweep_expired_leases(&ctx).await;
        let finalized_after_first = count_failed(&db, &run_ids).await;
        assert!(
            finalized_after_first < 3,
            "a batch limit of 2 must prevent all 3 seeded rows from finalizing in one cycle"
        );

        sweep_expired_leases(&ctx).await;
        let finalized_after_second = count_failed(&db, &run_ids).await;
        assert_eq!(
            finalized_after_second, 3,
            "the row left over from the capped first cycle must be handled on a later one"
        );
    }

    /// Each sweep returns the count of rows it acted on (`>=`: the query is globally unscoped).
    #[tokio::test]
    async fn a_sweep_returns_the_number_of_rows_it_acted_on() {
        let _guard = SWEEP_ENV_LOCK.lock().await;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-return-count-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let expired = chrono::Utc::now() - chrono::Duration::seconds(30);
        let q1 = seed_query(&db, org.id, ds.id).await;
        let q2 = seed_query(&db, org.id, ds.id).await;
        seed_query_run(&db, &q1, "running", Some(expired)).await;
        seed_query_run(&db, &q2, "running", Some(expired)).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let reclaimed = sweep_expired_leases(&ctx).await;
        assert!(
            reclaimed >= 2,
            "must return at least the 2 rows this test seeded"
        );
    }

    /// A leaked `pending` run is terminalized by `fail_abandoned_pending`: slot freed and a real
    /// terminal Redis job status written.
    #[tokio::test]
    async fn fail_abandoned_pending_finalizes_a_leaked_enqueued_run_and_frees_the_inflight_slot() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-abandoned-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, \
                 enqueued_at = now() - ($2 || ' seconds')::interval WHERE id = $3",
            [
                job_id.clone().into(),
                (abandoned_pending_secs() + 30).into(),
                run_id.into(),
            ],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        fail_abandoned_pending(&ctx).await;

        let row = query_row(
            &db,
            "SELECT status, finished_at, error FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await;
        let status: String = row.try_get("", "status").unwrap();
        let finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "finished_at").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(status, "timed_out");
        assert!(finished_at.is_some());
        assert!(error.map(|e| !e.is_empty()).unwrap_or(false));

        let insert = insert_inflight_run(&db, org.id, "query", q.id).await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after fail_abandoned_pending"
        );

        let job_status = queue::get_job_status(&redis, &job_id).await.unwrap();
        assert_eq!(
            job_status.map(|s| s.status),
            Some(queue::JobStatus::Failure),
            "the terminal Redis job status must actually be written"
        );
    }

    /// `fail_abandoned_pending` never touches a parked run (`enqueued_at IS NULL`), however old.
    #[tokio::test]
    async fn fail_abandoned_pending_leaves_a_never_enqueued_parked_run_untouched() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-abandoned-parked-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET created_at = now() - ($1 || ' seconds')::interval WHERE id = $2",
            [(abandoned_pending_secs() * 10).into(), run_id.into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        fail_abandoned_pending(&ctx).await;

        let status = run_status(&db, run_id).await;
        assert_eq!(
            status, "pending",
            "a run parked over its cap (never enqueued) must not be touched by the ceiling sweep"
        );
    }

    /// A run claimed into `running` between the batch SELECT and the per-row finalize is not
    /// terminalized (the CAS only matches `pending`).
    #[tokio::test]
    async fn fail_abandoned_pending_does_not_clobber_a_run_claimed_into_running_after_its_scan() {
        let _guard = SWEEP_ENV_LOCK.lock().await;
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-abandoned-race-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET enqueued_at = now() - ($1 || ' seconds')::interval WHERE id = $2",
            [(abandoned_pending_secs() + 30).into(), run_id.into()],
        )
        .await;

        // Simulates a consumer winning the pending->running CAS after the batch SELECT.
        assert!(run_lifecycle::claim_pending_run(&db, run_id).await.unwrap());

        let ctx = test_ctx(db.clone(), redis.clone());
        fail_abandoned_pending(&ctx).await;

        let status = run_status(&db, run_id).await;
        assert_eq!(
            status, "running",
            "a run claimed into running after the sweep's scan must not be finalized out from under it"
        );

        // uq_runs_inflight must still be held by the running row.
        let insert = insert_inflight_run(&db, org.id, "query", q.id).await;
        assert!(
            insert.is_err(),
            "the claimed run's in-flight slot must not have been released"
        );

        // Clean up: a leaseless `running` row would inflate later reclaim counts.
        exec_sql(
            &db,
            "UPDATE runs SET status = 'failed' WHERE id = $1",
            [run_id.into()],
        )
        .await;
    }

    /// An abandoned run saturating a `max_concurrent_runs = 1` source is terminalized by the sweep
    /// pair, releasing the cap slot so the next run admits.
    #[tokio::test]
    async fn fail_abandoned_pending_then_requeue_frees_the_wedged_cap_slot() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-wedge-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        set_max_concurrent_runs(&db, ds.id, 1).await;
        let q_a = seed_published_query(&db, org.id, ds.id).await;
        let q_b = seed_published_query(&db, org.id, ds.id).await;

        // Run A: the leak — pending, enqueued, but its queue message is long gone.
        let job_a = uuid::Uuid::new_v4().to_string();
        let run_a = seed_query_run(&db, &q_a, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, \
                 enqueued_at = now() - ($2 || ' seconds')::interval WHERE id = $3",
            [
                job_a.into(),
                (abandoned_pending_secs() + 30).into(),
                run_a.into(),
            ],
        )
        .await;

        // Run B: a never-enqueued stale pending run for the same data source.
        let job_b = uuid::Uuid::new_v4().to_string();
        let run_b = seed_query_run(&db, &q_b, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, created_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [
                job_b.clone().into(),
                (STALE_PENDING_SECS + 30).into(),
                run_b.into(),
            ],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let b_model = run::Entity::find_by_id(run_b)
            .one(&ctx.db)
            .await
            .unwrap()
            .unwrap();
        let admitted = deadline::try_enqueue_run(&ctx, &b_model).await.unwrap();
        assert!(
            !admitted,
            "correct admission control: B must be refused while A's leaked (but still pending) \
             run saturates the cap"
        );

        fail_abandoned_pending(&ctx).await;
        requeue_stale_pending(&ctx).await;

        let a_status = run_status(&db, run_a).await;
        assert_eq!(a_status, "timed_out", "the leaked run must be terminalized");

        let b_status = queue::get_job_status(&redis, &job_b).await.unwrap();
        assert_eq!(
            b_status.map(|s| s.status),
            Some(queue::JobStatus::Pending),
            "B must actually be enqueued for real once the leaked slot frees"
        );
    }

    /// A retry via `requeue_stale_pending` never moves `enqueued_at`, so the abandoned-pending
    /// ceiling is a real clock.
    #[tokio::test]
    async fn requeue_stale_pending_does_not_reset_enqueued_at() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-w6-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, \
                 created_at = now() - ($2 || ' seconds')::interval, \
                 enqueued_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [
                job_id.into(),
                (STALE_PENDING_SECS + 100).into(),
                run_id.into(),
            ],
        )
        .await;

        let before: sea_orm::prelude::DateTimeWithTimeZone = query_row(
            &db,
            "SELECT enqueued_at FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await
        .try_get("", "enqueued_at")
        .unwrap();

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending(&ctx).await;

        let after: sea_orm::prelude::DateTimeWithTimeZone = query_row(
            &db,
            "SELECT enqueued_at FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await
        .try_get("", "enqueued_at")
        .unwrap();

        assert_eq!(before, after, "a retry must never reset the ceiling clock");
    }

    #[tokio::test]
    async fn requeue_stale_pending_enqueues_a_run_that_never_made_it_onto_a_queue() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-stale-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        // Published, so `requeue_stale_pending` enqueues it (unpublished drafts are only enqueued
        // on manual trigger).
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, created_at = now() - ($2 || ' seconds')::interval WHERE id = $3",
            [
                job_id.clone().into(),
                (STALE_PENDING_SECS + 30).into(),
                run_id.into(),
            ],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending(&ctx).await;

        let status = queue::get_job_status(&redis, &job_id).await.unwrap();
        assert!(
            status.is_some(),
            "a stale pending run must actually be re-enqueued (real job-status write + LPUSH)"
        );
        assert_eq!(status.unwrap().status, queue::JobStatus::Pending);
    }

    /// Removes and counts queued `ExecuteQuery` envelopes for this test's unique `job_id`.
    async fn drain_execute_query_jobs_for_job(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        job_id: &str,
    ) -> usize {
        let mut count = 0;
        while find_and_remove_job(
            redis,
            key,
            |j| matches!(j, queue::JobMessage::ExecuteQuery { job_id: jid, .. } if jid == job_id),
        )
        .await
        .is_some()
        {
            count += 1;
        }
        count
    }

    /// An already-enqueued stuck row gets exactly one re-push per cooldown window, not one per
    /// sweep tick.
    #[tokio::test]
    async fn requeue_stale_pending_gates_a_second_push_of_an_already_enqueued_run_behind_a_cooldown(
    ) {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-cooldown-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, \
                 created_at = now() - ($2 || ' seconds')::interval, \
                 enqueued_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [
                job_id.clone().into(),
                (STALE_PENDING_SECS + 30).into(),
                run_id.into(),
            ],
        )
        .await;

        let key = queue::QueueName::ScheduledQueries.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());

        requeue_stale_pending(&ctx).await;
        assert_eq!(
            drain_execute_query_jobs_for_job(&redis, &key, &job_id).await,
            1,
            "the first sweep to see this stale, already-enqueued row must re-push it once"
        );

        requeue_stale_pending(&ctx).await;
        assert_eq!(
            drain_execute_query_jobs_for_job(&redis, &key, &job_id).await,
            0,
            "an immediate second sweep must be gated by the cooldown, not push a duplicate"
        );

        let mut conn = redis.clone();
        let _: () = conn
            .del(format!("arcodash:requeue-cooldown:run:{run_id}"))
            .await
            .unwrap();
        requeue_stale_pending(&ctx).await;
        assert_eq!(
            drain_execute_query_jobs_for_job(&redis, &key, &job_id).await,
            1,
            "clearing the cooldown key must restore the push"
        );
    }

    /// A failing cooldown `SET` (Redis error, not just an absent key) falls through to the push
    /// rather than dropping it.
    #[tokio::test]
    async fn requeue_stale_pending_pushes_anyway_when_the_cooldown_check_errors() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-cooldown-erroring-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, \
                 created_at = now() - ($2 || ' seconds')::interval, \
                 enqueued_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [
                job_id.clone().into(),
                (STALE_PENDING_SECS + 30).into(),
                run_id.into(),
            ],
        )
        .await;

        let redis_url = std::env::var("TEST_REDIS_URL").unwrap();
        let host_port = redis_url
            .strip_prefix("redis://")
            .expect("TEST_REDIS_URL must start with redis://");
        let acl_user = format!("cooldown-set-denied-{suffix}");
        let mut admin_conn = redis.clone();
        // `&*` grants every pubsub channel (Valkey 8 defaults new users to `resetchannels`).
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg(&acl_user)
            .arg("on")
            .arg(">testpw")
            .arg("~*")
            .arg("&*")
            .arg("+@all")
            .arg("-set")
            .query_async(&mut admin_conn)
            .await
            .unwrap();

        let restricted_url = format!("redis://{acl_user}:testpw@{host_port}");
        let restricted = arcodash_core::valkey_client::connect(&restricted_url)
            .await
            .unwrap();
        let ctx = ExecuteQueryContext {
            redis: restricted,
            ..test_ctx(db.clone(), redis.clone())
        };

        let key = queue::QueueName::ScheduledQueries.queue_key();
        requeue_stale_pending(&ctx).await;

        let _: redis::RedisResult<()> = redis::cmd("ACL")
            .arg("DELUSER")
            .arg(&acl_user)
            .query_async(&mut admin_conn)
            .await;

        assert_eq!(
            drain_execute_query_jobs_for_job(&redis, &key, &job_id).await,
            1,
            "a cooldown check that errors must still push, not silently drop the retry"
        );
        let cooldown_key_exists: bool = redis
            .clone()
            .exists(format!("arcodash:requeue-cooldown:run:{run_id}"))
            .await
            .unwrap();
        assert!(
            !cooldown_key_exists,
            "the cooldown key must not exist afterward — the SET never landed, it fell open"
        );
    }

    /// A never-enqueued row keeps retrying every sweep with no cooldown gating, including right
    /// after its `enqueued_at` is stamped.
    #[tokio::test]
    async fn requeue_stale_pending_never_enqueued_row_is_pushed_by_consecutive_sweeps() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-cooldown-null-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, created_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [
                job_id.clone().into(),
                (STALE_PENDING_SECS + 30).into(),
                run_id.into(),
            ],
        )
        .await;

        let key = queue::QueueName::ScheduledQueries.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());

        // System-wide batch-capped sweep: retry until this row lands in a cycle.
        for round in 0..2 {
            let mut pushed = 0;
            for _ in 0..30 {
                requeue_stale_pending(&ctx).await;
                pushed = drain_execute_query_jobs_for_job(&redis, &key, &job_id).await;
                if pushed > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert_eq!(
                pushed, 1,
                "round {round}: a row with no prior enqueued_at must not be blocked by a cooldown it never claimed"
            );
        }
    }

    async fn drain(redis: &redis::aio::ConnectionManager, key: &str) {
        let mut conn = redis.clone();
        while redis::AsyncCommands::lpop::<_, Option<String>>(&mut conn, key, None)
            .await
            .unwrap()
            .is_some()
        {}
    }

    async fn find_and_remove_resume_job(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        run_id: i64,
    ) -> Option<queue::JobMessage> {
        find_and_remove_job(redis, key, |j| {
            matches!(j, queue::JobMessage::ResumeMonitorEvaluation { run_id: rid, .. } if *rid == run_id)
        })
        .await
    }

    async fn seed_threshold_monitor(
        db: &sea_orm::DatabaseConnection,
        q: &arcodash_core::entities::query::Model,
    ) -> monitor::Model {
        seed_monitor(
            db,
            q.org_id,
            q.id,
            q.user_id,
            serde_json::json!({"column": "value", "op": ">", "value": 50.0}),
        )
        .await
    }

    /// Parks `waiter_id` on `target_id` as if it started at `started_at`.
    async fn park_waiter(
        db: &sea_orm::DatabaseConnection,
        waiter_id: i64,
        target_id: i64,
        started_at: DateTime<Utc>,
    ) {
        exec_sql(
            db,
            "UPDATE runs SET waiting_on_run_id = $1, started_at = $2 WHERE id = $3",
            [target_id.into(), started_at.into(), waiter_id.into()],
        )
        .await;
    }

    async fn run_status_and_error(
        db: &sea_orm::DatabaseConnection,
        run_id: i64,
    ) -> (String, Option<String>) {
        let row = query_row(
            db,
            "SELECT status, error FROM runs WHERE id = $1",
            [run_id.into()],
        )
        .await;
        (
            row.try_get("", "status").unwrap(),
            row.try_get("", "error").unwrap(),
        )
    }

    /// Inserts a fresh manual `pending` run for the entity; fails while another run holds its
    /// `uq_runs_inflight` slot.
    async fn insert_inflight_run(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        entity_type: &str,
        entity_id: i64,
    ) -> Result<sea_orm::ExecResult, sea_orm::DbErr> {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO runs (org_id, entity_type, entity_id, triggered_by, status, created_at) \
             VALUES ($1, $2, $3, 'manual', 'pending', now())",
            [org_id.into(), entity_type.into(), entity_id.into()],
        ))
        .await
    }

    /// A run legitimately parked on `waiting_on_run_id` is never reclaimed as a dead lease, even
    /// with a stale `lease_expires_at`.
    #[tokio::test]
    async fn sweep_expired_leases_does_not_touch_a_waiting_run() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-lease-waiting-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let expired = Utc::now() - chrono::Duration::seconds(30);
        let waiter_id = seed_run(
            &db,
            org.id,
            "monitor",
            mon.id,
            None,
            None,
            "running",
            Some(expired),
        )
        .await;
        exec_sql(
            &db,
            "UPDATE runs SET waiting_on_run_id = $1 WHERE id = $2",
            [target_id.into(), waiter_id.into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        sweep_expired_leases(&ctx).await;

        let status = run_status(&db, waiter_id).await;
        assert_eq!(
            status, "running",
            "a waiting run's expired lease must not be mistaken for a dead lease"
        );
    }

    #[tokio::test]
    async fn sweep_waiting_runs_liveness_re_enqueues_resume_for_a_missed_fan_out() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-liveness-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let key = queue::QueueName::Default.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());

        // Held for the rest of the test: the sweep is unscoped and shares process-global state with
        // sibling tests.
        let _guard = SWEEP_ENV_LOCK.lock().await;
        drain(&redis, &key).await;
        exec_sql(
            &db,
            "UPDATE runs SET waiting_on_run_id = $1, started_at = now() WHERE id = $2",
            [target_id.into(), waiter_id.into()],
        )
        .await;
        // Terminalize the target directly, simulating a missed fan-out.
        exec_sql(
            &db,
            "UPDATE runs SET status = 'success' WHERE id = $1",
            [target_id.into()],
        )
        .await;

        sweep_waiting_runs(&ctx).await;
        // Production order: the stranded sweep runs right after the liveness pass and must not grab
        // this waiter (it holds a live grace lease).
        sweep_stranded_monitor_runs(&ctx).await;

        let row = query_row(
            &db,
            "SELECT waiting_on_run_id, lease_expires_at FROM runs WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_run_id").unwrap();
        assert_eq!(
            waiting_on, None,
            "the liveness pass must clear the missed waiter"
        );
        let lease_expires_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "lease_expires_at").unwrap();
        assert!(
            lease_expires_at.is_some_and(|t| t.with_timezone(&Utc) > Utc::now()),
            "detach_cas must stamp a future grace lease on the winning detach"
        );

        let job = find_and_remove_resume_job(&redis, &key, waiter_id)
            .await
            .expect("exactly one ResumeMonitorEvaluation must have been enqueued");
        match job {
            queue::JobMessage::ResumeMonitorEvaluation {
                run_id,
                query_run_id,
                query_run_status,
                ..
            } => {
                assert_eq!(run_id, waiter_id);
                assert_eq!(query_run_id, target_id);
                assert_eq!(query_run_status, "success");
            }
            other => panic!("expected ResumeMonitorEvaluation, got {other:?}"),
        }
        assert!(
            find_and_remove_resume_job(&redis, &key, waiter_id)
                .await
                .is_none(),
            "the stranded sweep must not enqueue a second resume for the same waiter"
        );

        exec_sql(
            &db,
            "UPDATE runs SET status = 'success' WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        drop(_guard);
    }

    #[tokio::test]
    async fn sweep_waiting_runs_detaches_at_the_max_fresh_wait_ceiling_and_evaluates_stale() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-ceiling-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let stale_at = Utc::now() - chrono::Duration::seconds(500);
        let qr = seed_query_result(
            &db,
            org.id,
            ds.id,
            stale_at,
            serde_json::json!({"value": 10.0}),
        )
        .await;
        set_published_latest_query_data(&db, q.id, qr.id).await;

        let mon = seed_monitor(
            &db,
            org.id,
            q.id,
            q.user_id,
            serde_json::json!({"column": "value", "op": ">", "value": 50.0, "max_fresh_wait_secs": 1}),
        )
        .await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let started = Utc::now() - chrono::Duration::seconds(5);
        let key = queue::QueueName::Default.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());

        // Held for the rest of the test: the sweep is unscoped and shares process-global state with
        // sibling tests.
        let _guard = SWEEP_ENV_LOCK.lock().await;
        drain(&redis, &key).await;
        park_waiter(&db, waiter_id, target_id, started).await;

        sweep_waiting_runs(&ctx).await;

        let row = query_row(
            &db,
            "SELECT status, waiting_on_run_id, query_result_id, data_age_secs, condition_met \
                 FROM runs WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        let status: String = row.try_get("", "status").unwrap();
        let waiting_on: Option<i64> = row.try_get("", "waiting_on_run_id").unwrap();
        let qr_id: Option<i64> = row.try_get("", "query_result_id").unwrap();
        let age: Option<i64> = row.try_get("", "data_age_secs").unwrap();
        let condition_met: Option<bool> = row.try_get("", "condition_met").unwrap();
        assert_eq!(status, "success");
        assert_eq!(waiting_on, None);
        assert_eq!(qr_id, Some(qr.id));
        assert!(
            age.unwrap_or(0) >= 500,
            "the recorded age must reflect the stale result, got {age:?}"
        );
        assert_eq!(
            condition_met,
            Some(false),
            "the stale-fallback eval (value 10 <= threshold 50) must still record a real outcome"
        );

        let target_status = run_status(&db, target_id).await;
        assert_eq!(
            target_status, "running",
            "the in-flight query run is left to finish on its own"
        );

        // Target finalizes; its fan-out finds no waiter, so nothing is re-enqueued.
        run_lifecycle::finalize_run(
            &ctx,
            target_id,
            RunOutcome {
                status: RunStatus::Success,
                row_count: Some(0),
                error: None,
                query_result_id: None,
                data_age_secs: None,
                condition_met: None,
            },
        )
        .await
        .unwrap();
        assert!(
            find_and_remove_resume_job(&redis, &key, waiter_id)
                .await
                .is_none(),
            "the ceiling-detached waiter must not be re-resumed when its target finally finalizes"
        );
        drop(_guard);
    }

    #[tokio::test]
    async fn sweep_waiting_runs_hard_timeout_fails_an_always_wait_monitor() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-hardtimeout-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        // Always-wait (`max_fresh_wait_secs` unset): only the hard timeout applies.
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let started = Utc::now() - chrono::Duration::seconds(5);
        let ctx = test_ctx(db.clone(), redis.clone());

        // Held across the env set/call/remove: the sweep is unscoped and the timeout override is
        // process-global.
        let _guard = SWEEP_ENV_LOCK.lock().await;
        park_waiter(&db, waiter_id, target_id, started).await;
        std::env::set_var("MONITOR_WAIT_TIMEOUT", "1");
        sweep_waiting_runs(&ctx).await;
        std::env::remove_var("MONITOR_WAIT_TIMEOUT");
        drop(_guard);

        let (status, error) = run_status_and_error(&db, waiter_id).await;
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("timed out waiting for fresh data"));
    }

    /// A swallowed timeout would leave the row ownerless for `sweep_stranded_monitor_runs` to fail;
    /// the outcome must be recorded.
    #[tokio::test]
    async fn a_hard_timeout_finalizes_the_waiter_as_failed_and_leaves_no_stranded_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-hardtimeout-stranded-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        // Old enough for both the hard timeout and the stranded threshold.
        let started = Utc::now() - chrono::Duration::seconds(STALE_PENDING_SECS + 30);
        let ctx = test_ctx(db.clone(), redis.clone());
        let key = queue::QueueName::Default.queue_key();

        let _guard = SWEEP_ENV_LOCK.lock().await;
        drain(&redis, &key).await;
        park_waiter(&db, waiter_id, target_id, started).await;
        std::env::set_var("MONITOR_WAIT_TIMEOUT", "1");
        sweep_waiting_runs(&ctx).await;
        std::env::remove_var("MONITOR_WAIT_TIMEOUT");

        let row = query_row(
            &db,
            "SELECT status, error, lease_expires_at, waiting_on_run_id FROM runs WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(
            status, "failed",
            "the finalize must actually record the timeout"
        );
        assert_eq!(error.as_deref(), Some("timed out waiting for fresh data"));

        // A second waiter, detached but never finalized, carries a future lease and is never the
        // ownerless shape.
        let waiter2_id =
            seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        park_waiter(&db, waiter2_id, target_id, started).await;
        assert!(run_lifecycle::detach_cas(&db, waiter2_id, target_id)
            .await
            .unwrap());
        let detached_row = query_row(
            &db,
            "SELECT waiting_on_run_id, lease_expires_at FROM runs WHERE id = $1",
            [waiter2_id.into()],
        )
        .await;
        let detached_waiting_on: Option<i64> =
            detached_row.try_get("", "waiting_on_run_id").unwrap();
        assert_eq!(detached_waiting_on, None);
        let detached_lease: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            detached_row.try_get("", "lease_expires_at").unwrap();
        assert!(
            detached_lease.is_some_and(|t| t.with_timezone(&Utc) > Utc::now()),
            "a detached-but-not-yet-finalized waiter must carry a future grace lease"
        );

        // With the outcome recorded, the row no longer matches the stranded sweep's predicate, so
        // it can't be finalized twice.
        sweep_stranded_monitor_runs(&ctx).await;
        drop(_guard);

        assert!(
            find_and_remove_resume_job(&redis, &key, waiter_id)
                .await
                .is_none(),
            "a correctly-finalized timeout must never be picked up by the stranded-run recovery sweep"
        );
        let after_status = run_status(&db, waiter_id).await;
        assert_eq!(
            after_status, "failed",
            "the stranded-run sweep must not touch a run that already finalized honestly"
        );

        // waiter2 (detached, future grace lease) must be untouched; `sweep_expired_leases` owns it.
        let waiter2_after = query_row(
            &db,
            "SELECT status, lease_expires_at FROM runs WHERE id = $1",
            [waiter2_id.into()],
        )
        .await;
        let waiter2_status: String = waiter2_after.try_get("", "status").unwrap();
        let waiter2_lease: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            waiter2_after.try_get("", "lease_expires_at").unwrap();
        assert_eq!(
            waiter2_status, "running",
            "a detached-but-not-yet-finalized waiter must be left alone by the stranded sweep"
        );
        assert!(
            waiter2_lease.is_some_and(|t| t.with_timezone(&Utc) > Utc::now()),
            "waiter2's future grace lease must still be intact after the stranded sweep runs"
        );
    }

    /// `detach_cas` winning after another caller already finalized the run yields `Ok(false)` and
    /// never overwrites the recorded status.
    #[tokio::test]
    async fn a_finalize_that_lost_its_cas_is_not_reported_as_an_error() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-hardtimeout-lostcas-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let started = Utc::now() - chrono::Duration::seconds(STALE_PENDING_SECS + 30);
        let ctx = test_ctx(db.clone(), redis.clone());

        let _guard = SWEEP_ENV_LOCK.lock().await;
        park_waiter(&db, waiter_id, target_id, started).await;

        // Another caller finalizes the waiter as Success before the timeout sweep runs.
        run_lifecycle::finalize_run(
            &ctx,
            waiter_id,
            RunOutcome::terminal(RunStatus::Success, None),
        )
        .await
        .unwrap();

        std::env::set_var("MONITOR_WAIT_TIMEOUT", "1");
        sweep_waiting_runs(&ctx).await;
        std::env::remove_var("MONITOR_WAIT_TIMEOUT");
        drop(_guard);

        let (status, error) = run_status_and_error(&db, waiter_id).await;
        assert_eq!(
            status, "success",
            "the earlier, real finalize must survive — the timeout sweep must not clobber it"
        );
        assert_eq!(error, None);
    }

    /// A waiter with a live grace lease is left alone by the stranded sweep and recovered as
    /// `failed` by `sweep_expired_leases` once it expires.
    #[tokio::test]
    async fn sweep_stranded_monitor_runs_leaves_a_grace_leased_waiter_to_sweep_expired_leases() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _guard = SWEEP_ENV_LOCK.lock().await;
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-stranded-leased-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let target_id = seed_query_run(&db, &q, "running", None).await;
        // A NULL `waiting_on_run_id` with a future lease is the shape `detach_cas` leaves, not the
        // ownerless one.
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let old_started = Utc::now() - chrono::Duration::seconds(STALE_PENDING_SECS + 30);
        park_waiter(&db, waiter_id, target_id, old_started).await;
        assert!(run_lifecycle::detach_cas(&db, waiter_id, target_id)
            .await
            .unwrap());

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        sweep_stranded_monitor_runs(&ctx).await;

        assert!(
            find_and_remove_resume_job(&redis, &key, waiter_id)
                .await
                .is_none(),
            "a grace-leased waiter must not be re-resumed by the stranded sweep"
        );
        let row = query_row(
            &db,
            "SELECT status, lease_expires_at FROM runs WHERE id = $1",
            [waiter_id.into()],
        )
        .await;
        let status: String = row.try_get("", "status").unwrap();
        assert_eq!(status, "running", "the stranded sweep must not touch it");
        let lease_expires_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            row.try_get("", "lease_expires_at").unwrap();
        assert!(
            lease_expires_at.is_some(),
            "the grace lease must survive untouched"
        );

        backdate_lease(&db, "runs", waiter_id).await;
        sweep_expired_leases(&ctx).await;

        let (status, error) = run_status_and_error(&db, waiter_id).await;
        assert_eq!(
            status, "failed",
            "an expired grace lease must recover as failed"
        );
        assert_eq!(error.as_deref(), Some("worker lost"));

        let insert = insert_inflight_run(&db, org.id, "monitor", mon.id).await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the lease-expiry sweep"
        );
        drop(_guard);
    }

    /// A `running` monitor run with no wait link and no lease is finalized `failed` directly.
    #[tokio::test]
    async fn sweep_stranded_monitor_runs_finalizes_an_ownerless_row_as_failed() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _guard = SWEEP_ENV_LOCK.lock().await;
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-stranded-ownerless-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        // `waiting_on_run_id` and `lease_expires_at` both NULL — the ownerless shape.
        let waiter_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "running", None).await;
        let old_started = Utc::now() - chrono::Duration::seconds(STALE_PENDING_SECS + 30);
        exec_sql(
            &db,
            "UPDATE runs SET started_at = $1 WHERE id = $2",
            [old_started.into(), waiter_id.into()],
        )
        .await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        sweep_stranded_monitor_runs(&ctx).await;

        assert!(
            find_and_remove_resume_job(&redis, &key, waiter_id)
                .await
                .is_none(),
            "the stranded sweep must not enqueue a fabricated resume"
        );
        let (status, error) = run_status_and_error(&db, waiter_id).await;
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("monitor run left with no owner"));

        let insert = insert_inflight_run(&db, org.id, "monitor", mon.id).await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the stranded sweep"
        );
        drop(_guard);
    }

    // Delivery durability: stale-pending recovery, expired-lease reclaim, end-to-end proofs.

    use arcodash_core::entities::{mail_delivery, monitor_subscription, notification_delivery};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    async fn backdate_created_at(
        db: &sea_orm::DatabaseConnection,
        table: &str,
        id: i64,
        secs_ago: i64,
    ) {
        let sql = format!(
            "UPDATE {table} SET created_at = now() - ($1 || ' seconds')::interval WHERE id = $2"
        );
        exec_sql(db, &sql, [secs_ago.into(), id.into()]).await;
    }

    /// Moves a `notification_deliveries` row to `retrying` with a backdated `started_at` and no
    /// live lease.
    async fn set_retrying(
        db: &sea_orm::DatabaseConnection,
        id: i64,
        attempt_number: i32,
        started_secs_ago: i64,
    ) {
        exec_sql(
            db,
            "UPDATE notification_deliveries \
             SET status = 'retrying', attempt_number = $1, lease_expires_at = NULL, \
                 started_at = now() - ($2 || ' seconds')::interval \
             WHERE id = $3",
            [attempt_number.into(), started_secs_ago.into(), id.into()],
        )
        .await;
    }

    async fn backdate_lease(db: &sea_orm::DatabaseConnection, table: &str, id: i64) {
        let sql = format!(
            "UPDATE {table} SET lease_expires_at = now() - interval '1 second' WHERE id = $1"
        );
        exec_sql(db, &sql, [id.into()]).await;
    }

    #[tokio::test]
    async fn requeue_stale_pending_notifications_rebuilds_a_lost_message() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-notif-stale-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let sub = monitor_subscription::ActiveModel {
            monitor_id: Set(mon.id),
            user_id: Set(q.user_id),
            destination_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let delivery_id = notification_delivery::insert_pending(
            &db,
            org.id,
            mon.id,
            Some(sub.id),
            -1,
            "triggered",
            None,
        )
        .await
        .unwrap();
        backdate_created_at(
            &db,
            "notification_deliveries",
            delivery_id,
            STALE_PENDING_SECS + 30,
        )
        .await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_notifications(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a SendNotification must have been re-enqueued for the stale pending delivery");
        match job {
            queue::JobMessage::SendNotification {
                subscriber_id,
                subscription_id,
                attempt,
                new_state,
                monitor_id,
                destination_id,
                ..
            } => {
                assert_eq!(subscriber_id, q.user_id);
                assert_eq!(subscription_id, Some(sub.id));
                assert_eq!(attempt, 1);
                assert_eq!(new_state, "triggered");
                assert_eq!(monitor_id, mon.id);
                assert_eq!(destination_id, -1);
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn requeue_stale_pending_notifications_recovers_null_subscription_via_monitor_owner() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-notif-stale-esc-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;

        // subscription_id = None — the escalation shape.
        let delivery_id =
            notification_delivery::insert_pending(&db, org.id, mon.id, None, -1, "triggered", None)
                .await
                .unwrap();
        backdate_created_at(
            &db,
            "notification_deliveries",
            delivery_id,
            STALE_PENDING_SECS + 30,
        )
        .await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_notifications(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a SendNotification must have been re-enqueued");
        match job {
            queue::JobMessage::SendNotification {
                subscriber_id,
                subscription_id,
                ..
            } => {
                assert_eq!(
                    subscriber_id, mon.user_id,
                    "subscriber_id must resolve to the monitor's own owner via COALESCE"
                );
                assert_eq!(subscription_id, None);
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    /// A `retrying` delivery whose backoff timer never fired is recovered too, re-enqueued at
    /// `attempt_number + 1`.
    #[tokio::test]
    async fn requeue_stale_pending_notifications_recovers_a_stuck_retrying_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-notif-stuck-retrying-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;

        let delivery_id =
            notification_delivery::insert_pending(&db, org.id, mon.id, None, -1, "triggered", None)
                .await
                .unwrap();
        set_retrying(&db, delivery_id, 2, STALE_RETRYING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_notifications(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a stuck 'retrying' delivery past the staleness threshold must be re-enqueued");
        match job {
            queue::JobMessage::SendNotification { attempt, .. } => {
                assert_eq!(
                    attempt, 3,
                    "attempt must be the stored attempt_number (2, the one that already failed) + 1"
                );
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    /// A `retrying` row still mid-backoff is left alone; backdated past `STALE_PENDING_SECS` but
    /// short of `STALE_RETRYING_SECS`.
    #[tokio::test]
    async fn requeue_stale_pending_notifications_leaves_a_fresh_retrying_row_alone() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-notif-fresh-retrying-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;

        let delivery_id =
            notification_delivery::insert_pending(&db, org.id, mon.id, None, -1, "triggered", None)
                .await
                .unwrap();
        set_retrying(&db, delivery_id, 1, STALE_PENDING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_notifications(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await;
        assert!(
            job.is_none(),
            "a retrying row still within its legitimate backoff window must not be re-enqueued"
        );
    }

    #[tokio::test]
    async fn requeue_stale_pending_mail_rebuilds_send_digest() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-digest-stale-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;

        let delivery_id = mail_delivery::insert_pending_digest(&db, org.id, q.user_id)
            .await
            .unwrap();
        backdate_created_at(&db, "mail_deliveries", delivery_id, STALE_PENDING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_mail(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendDigest { delivery_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a SendDigest must have been re-enqueued for the stale pending mail delivery");
        match job {
            queue::JobMessage::SendDigest {
                org_id, user_id, ..
            } => {
                assert_eq!(org_id, org.id);
                assert_eq!(user_id, q.user_id);
            }
            other => panic!("expected SendDigest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn requeue_stale_pending_mail_rebuilds_send_mail_with_full_payload() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-sendmail-stale-{suffix}")).await;

        let to = format!("stale-mail-{suffix}@test.local");
        let subject = "stale subject".to_string();
        let body_html = "<p>stale body</p>".to_string();
        let delivery_id =
            mail_delivery::insert_pending_mail(&db, org.id, &to, &subject, &body_html)
                .await
                .unwrap();
        backdate_created_at(&db, "mail_deliveries", delivery_id, STALE_PENDING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_mail(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendMail { delivery_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a SendMail must have been re-enqueued with its full payload");
        match job {
            queue::JobMessage::SendMail {
                org_id,
                to: got_to,
                subject: got_subject,
                body_html: got_body,
                ..
            } => {
                assert_eq!(org_id, org.id);
                assert_eq!(got_to, to);
                assert_eq!(got_subject, subject);
                assert_eq!(got_body, body_html);
            }
            other => panic!("expected SendMail, got {other:?}"),
        }
    }

    /// Moves a `mail_deliveries` row to `retrying` with a backdated `started_at` and no live lease.
    async fn set_mail_retrying(db: &sea_orm::DatabaseConnection, id: i64, started_secs_ago: i64) {
        exec_sql(
            db,
            "UPDATE mail_deliveries \
             SET status = 'retrying', lease_expires_at = NULL, \
                 started_at = now() - ($1 || ' seconds')::interval \
             WHERE id = $2",
            [started_secs_ago.into(), id.into()],
        )
        .await;
    }

    #[tokio::test]
    async fn requeue_stale_pending_mail_recovers_a_stuck_retrying_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-stuck-retrying-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;

        let delivery_id = mail_delivery::insert_pending_digest(&db, org.id, q.user_id)
            .await
            .unwrap();
        set_mail_retrying(&db, delivery_id, STALE_RETRYING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_mail(&ctx).await;

        find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendDigest { delivery_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a stuck 'retrying' mail delivery past the staleness threshold must be re-enqueued");
    }

    /// A `retrying` mail row within its window is left alone.
    #[tokio::test]
    async fn requeue_stale_pending_mail_leaves_a_fresh_retrying_row_alone() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-fresh-retrying-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;

        let delivery_id = mail_delivery::insert_pending_digest(&db, org.id, q.user_id)
            .await
            .unwrap();
        set_mail_retrying(&db, delivery_id, STALE_PENDING_SECS + 30).await;

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_mail(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendDigest { delivery_id: Some(id), .. } if *id == delivery_id)
        })
        .await;
        assert!(
            job.is_none(),
            "a retrying mail row still within its legitimate backoff window must not be re-enqueued"
        );
    }

    /// An enqueue failure after `enqueue_send_digest` inserts its row must not leave it `pending`
    /// forever, or a later fan-out would double-send.
    #[tokio::test]
    async fn a_failed_enqueue_marks_the_mail_deliveries_row_failed_not_left_pending() {
        use sea_orm::{ColumnTrait, QueryFilter};

        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-enqueue-fail-{suffix}")).await;

        // Redis client pointed at a dead port, so the enqueue fails.
        let dead_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let dead_valkey = dead_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");
        let dead_queue = queue::ValkeyQueue::new(dead_valkey);

        let result =
            arcodash_core::mail_notify::enqueue_send_digest(&db, &dead_queue, None, org.id, 42)
                .await;
        let delivery_id = match result {
            Err(_) => {
                // The id isn't returned on failure; recover it by shape.
                let row = mail_delivery::Entity::find()
                    .filter(mail_delivery::Column::OrgId.eq(org.id))
                    .one(&db)
                    .await
                    .unwrap()
                    .expect(
                        "enqueue_send_digest must still insert its row before the enqueue attempt",
                    );
                row.id
            }
            Ok(id) => panic!("enqueue against a dead Redis port must fail, got Ok({id})"),
        };

        let row = mail_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status, "failed",
            "a row whose enqueue failed must be marked failed, not left pending"
        );

        backdate_created_at(&db, "mail_deliveries", delivery_id, STALE_PENDING_SECS + 30).await;
        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_mail(&ctx).await;
        assert!(
            find_and_remove_job(&redis, &key, |j| matches!(
                j,
                queue::JobMessage::SendDigest { delivery_id: Some(id), .. } if *id == delivery_id
            ))
            .await
            .is_none(),
            "a failed (not pending) row must never be picked up by the stale-pending sweep"
        );
    }

    #[tokio::test]
    async fn stale_pending_triggered_redelivery_carries_persisted_explanation() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-redeliver-expl-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;
        exec_sql(
            &db,
            "INSERT INTO monitor_observations (org_id, monitor_id, observed_at, value, state, explanation) \
             VALUES ($1, $2, now() - interval '5 minutes', 1, 'triggered', 'Up 40% vs median')",
            [org.id.into(), mon.id.into()],
        )
        .await;

        let delivery_id =
            notification_delivery::insert_pending(&db, org.id, mon.id, None, -1, "triggered", None)
                .await
                .unwrap();
        backdate_created_at(
            &db,
            "notification_deliveries",
            delivery_id,
            STALE_PENDING_SECS + 30,
        )
        .await;

        let key = queue::QueueName::Default.queue_key();
        let ctx = test_ctx(db.clone(), redis.clone());
        requeue_stale_pending_notifications(&ctx).await;

        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("a SendNotification must have been re-enqueued");
        match job {
            queue::JobMessage::SendNotification { explanation, .. } => {
                assert_eq!(explanation.as_deref(), Some("Up 40% vs median"));
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    /// Kill-worker-mid-send recovery: claim, never finalize, backdate the lease, run both sweeps,
    /// then confirm the real consumer dispatch delivers.
    #[tokio::test]
    async fn kill_worker_mid_send_is_recovered_end_to_end_through_real_dispatch() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-kill-mid-send-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;

        let delivery_id =
            notification_delivery::insert_pending(&db, org.id, mon.id, None, -1, "triggered", None)
                .await
                .unwrap();
        backdate_created_at(
            &db,
            "notification_deliveries",
            delivery_id,
            STALE_PENDING_SECS + 30,
        )
        .await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        let smtp_port: u16 = std::env::var("MAILPIT_SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1026);
        ctx.mailer = Some(std::sync::Arc::new(
            lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::builder_dangerous("localhost")
                .port(smtp_port)
                .build(),
        ));

        // Worker "pops" the message and claims the row...
        assert!(delivery_lifecycle::claim(
            &db,
            delivery_lifecycle::NOTIFICATION_DELIVERIES,
            delivery_id
        )
        .await
        .unwrap());
        // ...then crashes before ever finalizing it. Backdate the lease so it looks abandoned.
        backdate_lease(&db, "notification_deliveries", delivery_id).await;

        reclaim_expired_delivery_leases(&ctx).await;
        let row = notification_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status, "pending",
            "the expired lease must be reclaimed back to pending"
        );
        assert!(row.lease_expires_at.is_none());

        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;

        requeue_stale_pending_notifications(&ctx).await;
        let job = find_and_remove_job(&redis, &key, |j| {
            matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
        })
        .await
        .expect("the reclaimed row must be re-enqueued by the stale-pending sweep");

        // Hand it to the real consumer dispatch.
        crate::consumer::dispatch(&ctx, job).await;

        let final_row = notification_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            final_row.status, "sent",
            "the recovered delivery must actually complete through real dispatch"
        );
    }

    /// Redis-flush recovery: drain the queue directly and confirm every send is recovered from its
    /// Postgres row.
    #[tokio::test]
    async fn redis_flush_of_several_notifications_is_fully_recovered_from_postgres_alone() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-redis-flush-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_threshold_monitor(&db, &q).await;

        const N: usize = 3;
        let mut delivery_ids = Vec::with_capacity(N);
        let ctx = test_ctx(db.clone(), redis.clone());
        let key = queue::QueueName::Default.queue_key();
        drain(&redis, &key).await;
        for _ in 0..N {
            let delivery_id = notification_delivery::insert_pending(
                &db,
                org.id,
                mon.id,
                None,
                -1,
                "triggered",
                None,
            )
            .await
            .unwrap();
            ctx.queue
                .enqueue(
                    queue::QueueName::Default,
                    queue::JobMessage::SendNotification {
                        org_id: org.id,
                        monitor_id: mon.id,
                        destination_id: -1,
                        new_state: "triggered".to_string(),
                        subscriber_id: mon.user_id,
                        subscription_id: None,
                        attempt: 1,
                        run_id: Some(delivery_id),
                        observed: None,
                        explanation: None,
                    },
                )
                .await
                .unwrap();
            backdate_created_at(
                &db,
                "notification_deliveries",
                delivery_id,
                STALE_PENDING_SECS + 30,
            )
            .await;
            delivery_ids.push(delivery_id);
        }

        // Simulate a flush: drain the queue directly; the Postgres rows survive.
        drain(&redis, &key).await;

        requeue_stale_pending_notifications(&ctx).await;

        for delivery_id in delivery_ids {
            find_and_remove_job(&redis, &key, |j| {
                matches!(j, queue::JobMessage::SendNotification { run_id: Some(id), .. } if *id == delivery_id)
            })
            .await
            .unwrap_or_else(|| {
                panic!(
                    "delivery {delivery_id} must have been recovered from its Postgres row \
                     after the simulated Redis flush"
                )
            });
        }
    }

    // Orphan upload sweep: object deleted before the row; recent and linked uploads untouched.

    async fn seed_upload(
        db: &sea_orm::DatabaseConnection,
        store: &arcodash_core::storage::ObjectStore,
        org_id: i64,
        data_source_id: Option<i64>,
        age_hours: i64,
    ) -> (uuid::Uuid, String) {
        let id = uuid::Uuid::new_v4();
        let object_key = format!("orgs/{org_id}/uploads/csv/{id}/data.csv");
        // A fresh `-backend memory` emulator (CI) has no buckets.
        store.ensure_bucket().await.expect("ensure test bucket");
        store
            .put(
                &object_key,
                bytes::Bytes::from_static(b"a,b\n1,2\n"),
                "text/csv",
            )
            .await
            .expect("seed object put failed");
        let created_at = chrono::Utc::now() - chrono::Duration::hours(age_hours);
        data_source_upload::ActiveModel {
            id: Set(id),
            org_id: Set(org_id),
            object_key: Set(object_key.clone()),
            filename: Set("data.csv".to_string()),
            byte_size: Set(8),
            content_type: Set("text/csv".to_string()),
            created_by: Set(None),
            data_source_id: Set(data_source_id),
            created_at: Set(created_at.into()),
        }
        .insert(db)
        .await
        .expect("seed upload row failed");
        (id, object_key)
    }

    #[tokio::test]
    async fn sweep_orphan_uploads_deletes_only_the_unlinked_aged_upload_and_its_object() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-orphan-sweep-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis.clone());

        // (a) unlinked, older than the 24h cutoff — must be swept.
        let (orphan_id, orphan_key) = seed_upload(&db, &ctx.store, org.id, None, 25).await;
        // (b) unlinked, but younger than the cutoff — must survive.
        let (recent_id, recent_key) = seed_upload(&db, &ctx.store, org.id, None, 1).await;
        // (c) linked to a data source, as old as (a) — must survive.
        let (linked_id, linked_key) = seed_upload(&db, &ctx.store, org.id, Some(ds.id), 25).await;

        sweep_orphan_uploads(&ctx).await;

        assert!(
            data_source_upload::Entity::find_by_id(orphan_id)
                .one(&db)
                .await
                .unwrap()
                .is_none(),
            "the aged unlinked upload row must be swept"
        );
        assert!(
            ctx.store.get(&orphan_key).await.is_err(),
            "the aged unlinked upload's object must be deleted from the store"
        );

        assert!(
            data_source_upload::Entity::find_by_id(recent_id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "an unlinked upload younger than the cutoff must survive"
        );
        assert!(
            ctx.store.get(&recent_key).await.is_ok(),
            "a surviving upload's object must remain in the store"
        );

        assert!(
            data_source_upload::Entity::find_by_id(linked_id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "a linked upload must never be swept regardless of age"
        );
        assert!(
            ctx.store.get(&linked_key).await.is_ok(),
            "a linked upload's object must never be deleted"
        );
    }

    #[tokio::test]
    async fn reconcile_run_task_divergence_counts_a_terminal_task_whose_run_never_followed() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-divergence-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "INSERT INTO conveyor_tasks (task_type, payload, status, triggered_by, finished_at, created_at) \
             VALUES ('execute_query', $1, 'failed', 'enqueue', now(), now())",
            [serde_json::json!({"run_id": run_id}).into()],
        )
        .await;
        // A terminal task with no `run_id` in its payload at all must not error the whole scan.
        exec_sql(
            &db,
            "INSERT INTO conveyor_tasks (task_type, payload, status, triggered_by, finished_at, created_at) \
             VALUES ('send_mail', $1, 'failed', 'enqueue', now(), now())",
            [serde_json::json!({"org_id": org.id}).into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let before = crate::health::RUN_TASK_DIVERGENCE.load(std::sync::atomic::Ordering::Relaxed);
        reconcile_run_task_divergence(&ctx).await;
        let after = crate::health::RUN_TASK_DIVERGENCE.load(std::sync::atomic::Ordering::Relaxed);

        // `>= 1`, not `== 1`: the scan is system-wide and a shared test DB may hold other divergent
        // pairs; the run_id-less row must not error the scan.
        assert!(
            after - before >= 1,
            "seeding a task whose run is still pending must move the counter by at least 1, got {}",
            after - before
        );
    }

    async fn seed_stored_result(
        db: &sea_orm::DatabaseConnection,
        store: &arcodash_core::test_support::blob::MemoryBlobStore,
        org_id: i64,
        ds_id: i64,
        age_days: i64,
    ) -> arcodash_core::entities::query_result::Model {
        use arcodash_core::entities::query_result;
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let when = Utc::now() - chrono::Duration::days(age_days);
        let location = format!("{org_id}/{}.json.gz", uuid::Uuid::new_v4());
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let row = query_result::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds_id),
            query_hash: Set(format!("hash-{}", uuid::Uuid::new_v4())),
            query: Set("SELECT 1".to_string()),
            data: Set(None),
            data_location: Set(Some(location.clone())),
            data_size_bytes: Set(Some(3)),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(when.into()),
            last_used_at: Set(when.into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        arcodash_core::org_storage::BlobStore::put(
            store,
            &format!("orgs/test/results/{location}"),
            bytes::Bytes::from_static(b"abc"),
            "application/gzip",
        )
        .await
        .unwrap();
        row
    }

    fn ctx_with_mem_store(
        ctx: ExecuteQueryContext,
        mode: arcodash_core::org_storage::StorageMode,
    ) -> (
        ExecuteQueryContext,
        std::sync::Arc<arcodash_core::test_support::blob::MemoryBlobStore>,
    ) {
        let store =
            std::sync::Arc::new(arcodash_core::test_support::blob::MemoryBlobStore::default());
        let ctx = ExecuteQueryContext {
            storage: std::sync::Arc::new(
                arcodash_core::test_support::blob::StaticResolver::with_store(
                    mode,
                    store.clone(),
                    "orgs/test/",
                ),
            ),
            ..ctx
        };
        (ctx, store)
    }

    async fn result_exists(db: &sea_orm::DatabaseConnection, id: i64) -> bool {
        arcodash_core::entities::query_result::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .is_some()
    }

    #[tokio::test]
    async fn sweep_deletes_object_and_row_after_retention() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let (ctx, store) = ctx_only_org_managed(test_ctx(db.clone(), redis), org.id);
        let row = seed_stored_result(&db, &store, org.id, ds.id, 8).await;

        let deleted = sweep_expired_results(&ctx).await;

        assert_eq!(deleted, 1);
        assert!(!result_exists(&db, row.id).await, "row must be deleted");
        assert!(store.keys().is_empty(), "object must be deleted");
    }

    #[tokio::test]
    async fn sweep_keeps_recently_used_results() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let (ctx, store) = ctx_with_mem_store(
            test_ctx(db.clone(), redis),
            arcodash_core::org_storage::StorageMode::ArcoDash,
        );
        let row = seed_stored_result(&db, &store, org.id, ds.id, 6).await;

        sweep_expired_results(&ctx).await;

        assert!(result_exists(&db, row.id).await);
        assert_eq!(store.keys().len(), 1);
    }

    #[tokio::test]
    async fn sweep_never_touches_inline_legacy_rows() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let (ctx, store) = ctx_only_org_managed(test_ctx(db.clone(), redis), org.id);
        let row = seed_query_result(
            &db,
            org.id,
            ds.id,
            Utc::now() - chrono::Duration::days(30),
            serde_json::json!({"a": 1}),
        )
        .await;

        sweep_expired_results(&ctx).await;

        assert!(
            result_exists(&db, row.id).await,
            "inline rows are never swept"
        );
        assert_eq!(store.deletes(), 0);
    }

    #[tokio::test]
    async fn sweep_skips_customer_storage_orgs() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let (ctx, store) = ctx_with_mem_store(
            test_ctx(db.clone(), redis),
            arcodash_core::org_storage::StorageMode::Customer,
        );
        let row = seed_stored_result(&db, &store, org.id, ds.id, 30).await;

        sweep_expired_results(&ctx).await;

        assert!(result_exists(&db, row.id).await);
        assert_eq!(store.keys().len(), 1);
        assert_eq!(store.deletes(), 0);
    }

    #[tokio::test]
    async fn query_pointer_is_nulled_after_sweep() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let (ctx, store) = ctx_only_org_managed(test_ctx(db.clone(), redis), org.id);
        let row = seed_stored_result(&db, &store, org.id, ds.id, 8).await;
        set_latest_query_data(&db, q.id, row.id).await;

        sweep_expired_results(&ctx).await;

        let q = arcodash_core::entities::query::Entity::find_by_id(q.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(q.latest_query_data_id, None);
    }

    #[tokio::test]
    async fn sweep_skips_row_refreshed_concurrently() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let (ctx, store) = ctx_with_mem_store(
            test_ctx(db.clone(), redis),
            arcodash_core::org_storage::StorageMode::ArcoDash,
        );
        let row = seed_stored_result(&db, &store, org.id, ds.id, 8).await;
        let candidate = ExpiredCandidate {
            id: row.id,
            org_id: org.id,
            data_source_id: ds.id,
            query_hash: row.query_hash.clone(),
        };
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE query_results SET last_used_at = now() WHERE id = $1",
            [row.id.into()],
        ))
        .await
        .unwrap();
        let storage = ctx.storage.resolve(&ctx.db, org.id).await.unwrap();

        let outcome = expire_result(&ctx, &storage, &candidate, 7).await.unwrap();

        assert_eq!(outcome, Expiry::Kept);
        assert!(result_exists(&db, row.id).await);
        assert_eq!(store.keys().len(), 1);
        assert_eq!(store.deletes(), 0);
    }

    /// ArcoDash-managed for `org_id` only; every other org is customer-managed and never swept,
    /// so leftovers from other tests cannot touch the store.
    struct OnlyOrgManaged {
        org_id: i64,
        store: std::sync::Arc<dyn arcodash_core::org_storage::BlobStore>,
    }

    #[async_trait::async_trait]
    impl arcodash_core::org_storage::OrgStorageResolver for OnlyOrgManaged {
        async fn resolve(
            &self,
            _db: &sea_orm::DatabaseConnection,
            org_id: i64,
        ) -> Result<arcodash_core::org_storage::OrgStorage, arcodash_core::org_storage::StorageError>
        {
            let mode = if org_id == self.org_id {
                arcodash_core::org_storage::StorageMode::ArcoDash
            } else {
                arcodash_core::org_storage::StorageMode::Customer
            };
            Ok(arcodash_core::org_storage::OrgStorage::new(
                org_id,
                mode,
                self.store.clone(),
                "orgs/test/",
            ))
        }
    }

    fn ctx_only_org_managed(
        ctx: ExecuteQueryContext,
        org_id: i64,
    ) -> (
        ExecuteQueryContext,
        std::sync::Arc<arcodash_core::test_support::blob::MemoryBlobStore>,
    ) {
        let store =
            std::sync::Arc::new(arcodash_core::test_support::blob::MemoryBlobStore::default());
        let ctx = ExecuteQueryContext {
            storage: std::sync::Arc::new(OnlyOrgManaged {
                org_id,
                store: store.clone(),
            }),
            ..ctx
        };
        (ctx, store)
    }

    /// ArcoDash-managed for every org except `customer_org`.
    struct SplitResolver {
        customer_org: i64,
        store: std::sync::Arc<dyn arcodash_core::org_storage::BlobStore>,
    }

    #[async_trait::async_trait]
    impl arcodash_core::org_storage::OrgStorageResolver for SplitResolver {
        async fn resolve(
            &self,
            _db: &sea_orm::DatabaseConnection,
            org_id: i64,
        ) -> Result<arcodash_core::org_storage::OrgStorage, arcodash_core::org_storage::StorageError>
        {
            let mode = if org_id == self.customer_org {
                arcodash_core::org_storage::StorageMode::Customer
            } else {
                arcodash_core::org_storage::StorageMode::ArcoDash
            };
            Ok(arcodash_core::org_storage::OrgStorage::new(
                org_id,
                mode,
                self.store.clone(),
                "orgs/test/",
            ))
        }
    }

    #[tokio::test]
    async fn sweep_is_not_starved_by_skipped_orgs() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org_a = seed_org(&db, &format!("org-result-sweep-a-{suffix}")).await;
        let ds_a = seed_data_source(&db, org_a.id).await;
        let org_b = seed_org(&db, &format!("org-result-sweep-b-{suffix}")).await;
        let ds_b = seed_data_source(&db, org_b.id).await;
        let store =
            std::sync::Arc::new(arcodash_core::test_support::blob::MemoryBlobStore::default());
        let ctx = ExecuteQueryContext {
            storage: std::sync::Arc::new(SplitResolver {
                customer_org: org_a.id,
                store: store.clone(),
            }),
            ..test_ctx(db.clone(), redis)
        };
        let mut a_rows = Vec::new();
        for _ in 0..3 {
            a_rows.push(seed_stored_result(&db, &store, org_a.id, ds_a.id, 30).await);
        }
        let b_row = seed_stored_result(&db, &store, org_b.id, ds_b.id, 8).await;
        let b_key = format!("orgs/test/results/{}", b_row.data_location.clone().unwrap());

        let start = Some(SweepCursor {
            last_used_at: a_rows[0].last_used_at - chrono::Duration::milliseconds(1),
            id: 0,
        });

        sweep_expired_results_with_limit(&ctx, 2, RESULT_SWEEP_MAX_PAGES, start).await;

        assert!(
            !result_exists(&db, b_row.id).await,
            "org B row must be swept"
        );
        assert!(
            !store.keys().contains(&b_key),
            "org B object must be deleted"
        );
        for row in &a_rows {
            assert!(
                result_exists(&db, row.id).await,
                "customer-managed row stays"
            );
        }
        assert_eq!(store.keys().len(), 3);
    }

    #[tokio::test]
    async fn sweep_cursor_survives_ticks() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org_a = seed_org(&db, &format!("org-result-sweep-a-{suffix}")).await;
        let ds_a = seed_data_source(&db, org_a.id).await;
        let org_b = seed_org(&db, &format!("org-result-sweep-b-{suffix}")).await;
        let ds_b = seed_data_source(&db, org_b.id).await;
        let store =
            std::sync::Arc::new(arcodash_core::test_support::blob::MemoryBlobStore::default());
        let ctx = ExecuteQueryContext {
            storage: std::sync::Arc::new(SplitResolver {
                customer_org: org_a.id,
                store: store.clone(),
            }),
            ..test_ctx(db.clone(), redis)
        };
        let mut a_rows = Vec::new();
        for _ in 0..3 {
            a_rows.push(seed_stored_result(&db, &store, org_a.id, ds_a.id, 100).await);
        }
        let b_row = seed_stored_result(&db, &store, org_b.id, ds_b.id, 99).await;
        let b_key = format!("orgs/test/results/{}", b_row.data_location.clone().unwrap());
        let start = Some(SweepCursor {
            last_used_at: a_rows[0].last_used_at - chrono::Duration::milliseconds(1),
            id: 0,
        });

        let (_, cursor) = sweep_expired_results_with_limit(&ctx, 1, 2, start).await;

        assert!(
            result_exists(&db, b_row.id).await,
            "page cap not yet reached org B"
        );
        assert!(cursor.is_some(), "a capped tick returns its position");

        sweep_expired_results_with_limit(&ctx, 1, 2, cursor).await;

        assert!(
            !result_exists(&db, b_row.id).await,
            "org B row must be swept"
        );
        assert!(
            !store.keys().contains(&b_key),
            "org B object must be deleted"
        );
    }

    /// Failing ArcoDash storage for one org; every other org is customer-managed and skipped.
    struct FailingForOrg {
        org_id: i64,
        failing: std::sync::Arc<arcodash_core::test_support::blob::FailingBlobStore>,
    }

    #[async_trait::async_trait]
    impl arcodash_core::org_storage::OrgStorageResolver for FailingForOrg {
        async fn resolve(
            &self,
            _db: &sea_orm::DatabaseConnection,
            org_id: i64,
        ) -> Result<arcodash_core::org_storage::OrgStorage, arcodash_core::org_storage::StorageError>
        {
            let mode = if org_id == self.org_id {
                arcodash_core::org_storage::StorageMode::ArcoDash
            } else {
                arcodash_core::org_storage::StorageMode::Customer
            };
            Ok(arcodash_core::org_storage::OrgStorage::new(
                org_id,
                mode,
                self.failing.clone(),
                "orgs/test/",
            ))
        }
    }

    #[tokio::test]
    async fn sweep_stops_org_after_storage_failure() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-result-sweep-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let failing =
            std::sync::Arc::new(arcodash_core::test_support::blob::FailingBlobStore::default());
        let ctx = ExecuteQueryContext {
            storage: std::sync::Arc::new(FailingForOrg {
                org_id: org.id,
                failing: failing.clone(),
            }),
            ..test_ctx(db.clone(), redis)
        };
        let seed_store = arcodash_core::test_support::blob::MemoryBlobStore::default();
        let r1 = seed_stored_result(&db, &seed_store, org.id, ds.id, 30).await;
        let r2 = seed_stored_result(&db, &seed_store, org.id, ds.id, 29).await;

        let (stats, _) =
            sweep_expired_results_with_limit(&ctx, 10, RESULT_SWEEP_MAX_PAGES, None).await;

        assert!(result_exists(&db, r1.id).await);
        assert!(result_exists(&db, r2.id).await);
        assert_eq!(failing.deletes(), 1);
        assert_eq!(stats.deleted, 0);
        assert_eq!(stats.failed_orgs, 1);
    }
}
