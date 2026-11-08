use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement};
use uuid::Uuid;

use arcodash_conveyor::{TaskContext, TaskOutcome, TaskResult, TypedTaskHandler};
use arcodash_core::baseline::{baseline_verdict, BaselineVerdict, BASELINE_WINDOW};
use arcodash_core::entities::{
    monitor, monitor_subscription, notification_delivery, query, query_result, run, schedule,
};
use arcodash_core::entitlements::WorkGate;
use arcodash_core::events::record_event;
use arcodash_core::models::{parse_monitor_options, MonitorOptions};
use arcodash_core::monitor_eval::EvaluateMonitorPayload;
use arcodash_core::notify::{enqueue_send_notification, SendNotificationParams};
use arcodash_core::schedule_spec::ScheduleSpec;

use crate::handlers::execute_query::ExecuteQueryContext;
use crate::handlers::explain_alert;
use crate::handlers::resume_monitor;
use crate::handlers::send_notification::{format_number, format_observed_cell};
use crate::run_lifecycle::{self, RunOutcome, RunStatus};

/// `run_id` is `Some` for a `runs`-row-driven evaluation (full freshness protocol); `None`
/// evaluates once against the cached result with no run bookkeeping.
pub async fn handle(ctx: &ExecuteQueryContext, org_id: i64, monitor_id: i64, run_id: Option<i64>) {
    if let WorkGate::Blocked { reason } = ctx.entitlements.scheduled_work(&ctx.db, org_id).await {
        tracing::info!(org_id, reason, "skipping scheduled work");
        // A run-backed evaluation must not stay `pending` forever, wedging `uq_runs_inflight`.
        if let Some(rid) = run_id {
            if let Err(e) =
                finalize_terminal(ctx, rid, RunStatus::Skipped, Some(reason.to_string())).await
            {
                tracing::error!(monitor_id, run_id = rid, error = %e, "evaluate_monitor: failed to finalize skipped run");
            }
        }
        return;
    }

    let Some(rid) = run_id else {
        if let Err(e) = handle_inner(ctx, org_id, monitor_id).await {
            tracing::error!(monitor_id, error = %e, "evaluate_monitor failed");
        }
        return;
    };

    match run_lifecycle::claim_pending_run(&ctx.db, rid).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::error!(monitor_id, run_id = rid, error = %e, "evaluate_monitor: start_run failed");
            return;
        }
    }

    // Lease guard: held across the freshness protocol, dropped on any exit.
    let _heartbeat = run_lifecycle::spawn_heartbeat(ctx.db.clone(), rid);

    if let Err(e) = drive_fresh_evaluation(ctx, org_id, monitor_id, rid).await {
        tracing::error!(monitor_id, run_id = rid, error = %e, "evaluate_monitor: freshness protocol failed");
        // Any error escaping the protocol finalizes the run failed so it never stays 'running'.
        if let Err(e) = finalize_terminal(ctx, rid, RunStatus::Failed, Some(format!("{e:#}"))).await
        {
            tracing::error!(monitor_id, run_id = rid, error = %e, "evaluate_monitor: failed to finalize a failed run");
        }
    }
}

/// [`run_lifecycle::finalize_run`] with a status-and-error-only outcome.
pub async fn finalize_terminal(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    status: RunStatus,
    error: Option<String>,
) -> anyhow::Result<()> {
    run_lifecycle::finalize_run(ctx, run_id, RunOutcome::terminal(status, error)).await?;
    Ok(())
}

/// The org-scoped monitor and its options, or `None` when it is missing or snoozed, or its query
/// went back to draft: all cases where nothing is evaluated.
async fn load_checkable_monitor(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
) -> anyhow::Result<Option<(monitor::Model, MonitorOptions)>> {
    let Some(monitor) = monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await?
        .filter(|m| m.org_id == org_id)
    else {
        tracing::warn!(monitor_id, "evaluate_monitor: monitor not found");
        return Ok(None);
    };
    let options = parse_monitor_options(monitor_id, &monitor.options);
    if options.is_silenced(Utc::now())
        || query_recently_unpublished(db, monitor.query_id, org_id).await?
    {
        return Ok(None);
    }
    Ok(Some((monitor, options)))
}

async fn handle_inner(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor_id: i64,
) -> anyhow::Result<()> {
    let Some((monitor, _)) = load_checkable_monitor(&ctx.db, org_id, monitor_id).await? else {
        return Ok(());
    };
    let eval = evaluate_state(&ctx.db, &monitor, org_id).await?;
    let observation_id = record_observation(&ctx.db, org_id, monitor.id, None, &eval).await;
    apply_transition_and_notify(ctx, org_id, &monitor, None, &eval, observation_id).await
}

/// True iff the monitor's query was published when watched but has since been taken back to draft.
/// Fail-open (`false`) on a missing/foreign query.
pub async fn query_recently_unpublished(
    db: &sea_orm::DatabaseConnection,
    query_id: i64,
    org_id: i64,
) -> anyhow::Result<bool> {
    let q = query::Entity::find_by_id(query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(db)
        .await?;
    Ok(q.is_some_and(|q| q.published_change_id.is_some() && q.unpublished_at.is_some()))
}

/// Resolve the `query_result` a monitor evaluates: the published result once the query is
/// published, else the draft's latest. Returns `(query_result_id, retrieved_at)` or `None`.
/// Re-checks the org explicitly since the worker is RLS-exempt.
pub async fn resolve_monitor_result(
    db: &sea_orm::DatabaseConnection,
    monitor: &monitor::Model,
    org_id: i64,
) -> anyhow::Result<Option<(i64, chrono::DateTime<Utc>)>> {
    Ok(resolve_result_row(db, monitor, org_id)
        .await?
        .map(|qr| (qr.id, qr.retrieved_at.with_timezone(&Utc))))
}

async fn resolve_result_row(
    db: &sea_orm::DatabaseConnection,
    monitor: &monitor::Model,
    org_id: i64,
) -> anyhow::Result<Option<query_result::Model>> {
    let Some(q) = query::Entity::find_by_id(monitor.query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    // A query taken back to draft stops checking rather than reading draft experiments.
    let qr_id = if q.published_change_id.is_none() {
        q.latest_query_data_id
    } else if q.unpublished_at.is_none() {
        q.published_latest_query_data_id
    } else {
        None
    };
    let Some(qr_id) = qr_id else {
        return Ok(None);
    };
    Ok(query_result::Entity::find_by_id(qr_id).one(db).await?)
}

/// Freshness fast path, else attach-or-trigger a query run and park this monitor run behind it.
async fn drive_fresh_evaluation(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor_id: i64,
    run_id: i64,
) -> anyhow::Result<()> {
    let Some((monitor, options)) = load_checkable_monitor(&ctx.db, org_id, monitor_id).await?
    else {
        return finalize_terminal(ctx, run_id, RunStatus::Success, None).await;
    };

    let Some((qr_id, retrieved_at)) = resolve_monitor_result(&ctx.db, &monitor, org_id).await?
    else {
        // No cached result yet: trigger a first refresh if the query is runnable, else evaluate as
        // unknown.
        let query_exists = query::Entity::find_by_id(monitor.query_id)
            .filter(query::Column::OrgId.eq(org_id))
            .one(&ctx.db)
            .await?
            .is_some();
        return if query_exists {
            attach_or_trigger_and_park(ctx, &monitor, run_id).await
        } else {
            evaluate_and_finalize(ctx, org_id, &monitor, run_id, None).await
        };
    };

    let staleness = resolve_staleness_secs(ctx, run_id, &options).await?;
    let age = (Utc::now() - retrieved_at).num_seconds().max(0);
    if age <= staleness {
        return evaluate_and_finalize(ctx, org_id, &monitor, run_id, Some((qr_id, age))).await;
    }
    attach_or_trigger_and_park(ctx, &monitor, run_id).await
}

/// Staleness threshold: monitor override, else schedule interval, else global default.
async fn resolve_staleness_secs(
    ctx: &ExecuteQueryContext,
    run_id: i64,
    options: &MonitorOptions,
) -> anyhow::Result<i64> {
    if let Some(secs) = options.max_data_staleness_secs {
        return Ok(secs);
    }

    let schedule_id: Option<i64> = ctx
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT schedule_id FROM runs WHERE id = $1",
            [run_id.into()],
        ))
        .await?
        .map(|row| row.try_get::<Option<i64>>("", "schedule_id"))
        .transpose()?
        .flatten();
    let spec = match schedule_id {
        Some(id) => schedule::Entity::find_by_id(id).one(&ctx.db).await?,
        None => None,
    }
    .and_then(|s| serde_json::from_value::<ScheduleSpec>(s.spec).ok());
    Ok(match spec {
        Some(ScheduleSpec::Interval { every_secs }) => every_secs,
        // Fallback freshness window when there is no staleness override or interval schedule.
        _ => arcodash_core::config::env_parsed("MONITOR_DEFAULT_STALENESS_SECS", 900),
    })
}

/// Evaluate the condition and finalize the run. `evaluated` is `Some((query_result_id,
/// data_age_secs))` for a concrete result, `None` for unknown; the evaluation's reason lands on
/// the run's `error`. Finalizes BEFORE the transition/notify: a lease the sweeper already
/// reclaimed loses the CAS, so its evaluation must neither move state nor notify.
pub async fn evaluate_and_finalize(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor: &monitor::Model,
    run_id: i64,
    evaluated: Option<(i64, i64)>,
) -> anyhow::Result<()> {
    let eval = evaluate_state(&ctx.db, monitor, org_id).await?;
    let won = run_lifecycle::finalize_run(
        ctx,
        run_id,
        RunOutcome {
            status: RunStatus::Success,
            row_count: None,
            error: eval.reason.clone(),
            query_result_id: evaluated.map(|(id, _)| id),
            data_age_secs: evaluated.map(|(_, age)| age),
            condition_met: match eval.state.as_str() {
                "triggered" => Some(true),
                "ok" => Some(false),
                _ => None,
            },
        },
    )
    .await?;
    if !won {
        tracing::warn!(
            monitor_id = monitor.id,
            run_id,
            "evaluate_monitor: lost the finalize race, skipping the state transition and notify"
        );
        return Ok(());
    }
    let observation_id = record_observation(&ctx.db, org_id, monitor.id, Some(run_id), &eval).await;
    apply_transition_and_notify(ctx, org_id, monitor, Some(run_id), &eval, observation_id).await
}

// Simplification: 500 points, no downsampling; add when sparkline users want >500.
const MAX_OBSERVATIONS: i64 = 500;

/// Appends one history point for a real check and trims to the newest `MAX_OBSERVATIONS`.
/// Best-effort.
async fn record_observation(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
    run_id: Option<i64>,
    eval: &Evaluation,
) -> Option<i64> {
    if eval.state == "unknown" {
        return None;
    }
    let inserted = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO monitor_observations \
             (org_id, monitor_id, observed_at, value, state, run_id, query_result_id) \
             VALUES ($1, $2, now(), $3, $4, $5, $6) RETURNING id",
            [
                org_id.into(),
                monitor_id.into(),
                eval.value.into(),
                eval.state.as_str().into(),
                run_id.into(),
                eval.query_result_id.into(),
            ],
        ))
        .await
        .and_then(|r| r.ok_or_else(|| sea_orm::DbErr::RecordNotFound("observation id".into())))
        .and_then(|r| r.try_get::<i64>("", "id"));
    let id = match inserted {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(monitor_id, error = %e, "record_observation: insert failed");
            return None;
        }
    };
    if let Err(e) = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM monitor_observations WHERE monitor_id = $1 \
             AND id NOT IN ( \
                 SELECT id FROM monitor_observations WHERE monitor_id = $1 \
                 ORDER BY observed_at DESC, id DESC LIMIT $2 \
             )",
            [monitor_id.into(), MAX_OBSERVATIONS.into()],
        ))
        .await
    {
        tracing::warn!(monitor_id, error = %e, "record_observation: prune failed");
    }
    Some(id)
}

/// Best-effort write of an alert explanation onto its observation row.
async fn persist_explanation(
    ctx: &ExecuteQueryContext,
    monitor_id: i64,
    observation_id: i64,
    text: String,
) {
    if let Err(e) = ctx
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE monitor_observations SET explanation = $1 WHERE id = $2",
            [text.into(), observation_id.into()],
        ))
        .await
    {
        tracing::warn!(monitor_id, error = %e, "evaluate_monitor: failed to persist explanation");
    }
}

/// Attaches an explanation to a still-triggered incident that has none. Never notifies.
async fn backfill_incident_explanation(
    ctx: &ExecuteQueryContext,
    monitor: &monitor::Model,
    eval: &Evaluation,
    observation_id: Option<i64>,
) {
    let Some(id) = observation_id else { return };
    if explain_alert::latest_explanation(&ctx.db, monitor.id)
        .await
        .is_some()
    {
        return;
    }
    if let Some(text) = explain_alert::explanation_for(ctx, monitor, eval, observation_id).await {
        persist_explanation(ctx, monitor.id, id, text).await;
    }
}

/// Apply an evaluated state via a guarded `UPDATE ... WHERE state = $old` and notify on a real
/// transition. Only the caller that flips the row notifies, so racing evaluations and
/// double-delivered resumes can't double-notify.
async fn apply_transition_and_notify(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor: &monitor::Model,
    run_id: Option<i64>,
    eval: &Evaluation,
    observation_id: Option<i64>,
) -> anyhow::Result<()> {
    let new_state = eval.state.as_str();
    let old_state = monitor.state.as_str();
    if new_state == old_state {
        if new_state == "triggered" {
            backfill_incident_explanation(ctx, monitor, eval, observation_id).await;
        }
        return Ok(());
    }

    // Rearm-suppressed re-triggers continue the incident (no escalation/ack reset), as does a new
    // monitor's first healthy check. Computed before the UPDATE overwrites `last_triggered_at`.
    let now = Utc::now();
    let skip_notify = (new_state == "triggered"
        && monitor
            .rearm
            .filter(|&r| r > 0)
            .zip(monitor.last_triggered_at)
            .is_some_and(|(rearm, lta)| {
                (now - lta.with_timezone(&Utc)).num_seconds() < i64::from(rearm)
            }))
        || (old_state == "unknown" && new_state == "ok");

    // Guarded transition: a new incident clears ack/escalation state unless rearm-suppressed; `->
    // ok` clears ack. 0 rows affected means a racing caller applied it, so skip notify + event.
    let rows = ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE monitors \
             SET state = $1, updated_at = now(), \
                 unevaluable_reason = CASE WHEN $1 = 'unevaluable' THEN $4 ELSE NULL END, \
                 last_triggered_at = CASE WHEN $1 = 'triggered' AND $5 THEN now() ELSE last_triggered_at END, \
                 acked_at        = CASE WHEN ($1 = 'triggered' AND $5) OR $1 = 'ok' THEN NULL ELSE acked_at END, \
                 acked_by        = CASE WHEN ($1 = 'triggered' AND $5) OR $1 = 'ok' THEN NULL ELSE acked_by END, \
                 escalation_step = CASE WHEN $1 = 'triggered' AND $5 THEN 0 ELSE escalation_step END, \
                 last_escalated_at = CASE WHEN $1 = 'triggered' AND $5 THEN NULL ELSE last_escalated_at END \
             WHERE id = $2 AND state = $3 \
             RETURNING id",
            [
                new_state.into(),
                monitor.id.into(),
                old_state.into(),
                eval.reason.clone().into(),
                (!skip_notify).into(),
            ],
        ))
        .await?;

    if rows.is_empty() {
        return Ok(());
    }
    crate::health::record_monitor_transition(new_state);

    tracing::info!(
        monitor_id = monitor.id,
        org_id,
        run_id = ?run_id,
        old_state,
        new_state,
        suppressed = skip_notify,
        "evaluate_monitor: monitor state changed"
    );

    // Recorded before any notification is enqueued: digests and the audit log read this event, and
    // a notify job on another worker reads it for previous-state context.
    if let Err(e) = record_event(
        &ctx.db,
        org_id,
        None,
        arcodash_core::events::MONITOR_STATE_CHANGED,
        Some("monitor"),
        Some(monitor.id),
        serde_json::json!({ "old_state": old_state, "new_state": new_state }),
    )
    .await
    {
        tracing::warn!(monitor_id = monitor.id, error = %e, "evaluate_monitor: failed to record monitor_state_changed audit event");
    }

    if skip_notify {
        return Ok(());
    }

    let subs = monitor_subscription::Entity::find()
        .filter(monitor_subscription::Column::MonitorId.eq(monitor.id))
        .all(&ctx.db)
        .await?;

    let explanation = if new_state == "triggered" {
        explain_alert::explanation_for(ctx, monitor, eval, observation_id).await
    } else {
        None
    };
    if let (Some(text), Some(id)) = (&explanation, observation_id) {
        persist_explanation(ctx, monitor.id, id, text.clone()).await;
    }

    for sub in &subs {
        // Insert the durable delivery row BEFORE the LPUSH so a crash between them leaves a
        // recoverable `pending` row.
        let destination_id = sub.destination_id.unwrap_or(-1);
        let delivery_id = match notification_delivery::insert_pending(
            &ctx.db,
            org_id,
            monitor.id,
            Some(sub.id),
            destination_id,
            new_state,
            run_id,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    monitor_id = monitor.id,
                    subscription_id = sub.id,
                    error = %e,
                    "evaluate_monitor: failed to pre-create delivery row, skipping notify"
                );
                continue;
            }
        };
        if let Err(e) = enqueue_send_notification(
            ctx.queue.as_ref(),
            ctx.conveyor.get().map(std::convert::AsRef::as_ref),
            SendNotificationParams {
                org_id,
                monitor_id: monitor.id,
                destination_id,
                new_state: new_state.to_string(),
                subscriber_id: sub.user_id,
                subscription_id: Some(sub.id),
                attempt: 1,
                delivery_id: Some(delivery_id),
                observed: eval.observed.clone(),
                explanation: explanation.clone(),
            },
        )
        .await
        {
            tracing::warn!(monitor_id = monitor.id, subscription_id = sub.id, error = %e, "evaluate_monitor: failed to enqueue SendNotification");
        }
    }

    Ok(())
}

/// Attach-or-trigger, race-free via `uq_runs_inflight`; NULL `schedule_id` exempts the run from
/// `uq_runs_entity_tick`.
async fn attach_or_trigger_and_park(
    ctx: &ExecuteQueryContext,
    monitor: &monitor::Model,
    run_id: i64,
) -> anyhow::Result<()> {
    let q = query::Entity::find_by_id(monitor.query_id)
        .filter(query::Column::OrgId.eq(monitor.org_id))
        .one(&ctx.db)
        .await?
        .context("monitor.query_id vanished between resolve and attach")?;
    let data_source_id = q.published_data_source_id.unwrap_or(q.data_source_id);

    let inserted = ctx
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, data_source_id, \
                               scheduled_for, triggered_by, status, job_id, created_at) \
             VALUES ($1, 'query', $2, NULL, $3, NULL, 'monitor', 'pending', $4, now()) \
             ON CONFLICT DO NOTHING RETURNING id",
            [
                monitor.org_id.into(),
                q.id.into(),
                data_source_id.into(),
                Uuid::new_v4().to_string().into(),
            ],
        ))
        .await?;

    let qrun_id: i64 = if let Some(row) = inserted {
        // We triggered the fresh run: enqueue it (the sweeper requeues if parked over the per-DS
        // cap).
        let id: i64 = row.try_get("", "id")?;
        if let Some(run) = run::Entity::find_by_id(id).one(&ctx.db).await? {
            crate::scheduler::deadline::try_enqueue_run(ctx, &run).await?;
        }
        id
    } else {
        // ATTACH to the existing pending|running query run.
        let row = ctx
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM runs WHERE entity_type = 'query' AND entity_id = $1 \
                   AND status IN ('pending','running') LIMIT 1",
                [q.id.into()],
            ))
            .await?;
        match row {
            Some(r) => r.try_get("", "id")?,
            // The in-flight run finalized meanwhile: re-drive the evaluation once. Boxed because of
            // the async recursion cycle.
            None => {
                return Box::pin(drive_fresh_evaluation(
                    ctx,
                    monitor.org_id,
                    monitor.id,
                    run_id,
                ))
                .await
            }
        }
    };

    // Park: point this run at the query run and drop its worker lease.
    ctx.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE runs SET waiting_on_run_id = $1, lease_expires_at = NULL, worker_id = NULL \
             WHERE id = $2 AND status = 'running'",
            [qrun_id.into(), run_id.into()],
        ))
        .await?;

    // Terminal race: if the query run finished before the park UPDATE, whoever clears
    // `waiting_on_run_id` via the detach-CAS owns the resume.
    let status: Option<String> = ctx
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT status FROM runs WHERE id = $1",
            [qrun_id.into()],
        ))
        .await?
        .map(|r| r.try_get::<String>("", "status"))
        .transpose()?;

    let terminal = matches!(
        status.as_deref(),
        Some("success" | "failed" | "timed_out" | "cancelled" | "skipped") | None
    );
    if terminal && run_lifecycle::detach_cas(&ctx.db, run_id, qrun_id).await? {
        // Won the detach: resume inline ("failed" if the run vanished).
        let s = status.as_deref().unwrap_or("failed");
        return resume_monitor::resume(ctx, monitor.org_id, run_id, qrun_id, s).await;
    }
    // Not terminal, or lost the detach: release the worker slot; the finalizer/sweeper resumes it.
    Ok(())
}

/// Extract a numeric cell whether JSON-encoded as a number or numeric string (Postgres NUMERIC and
/// `ClickHouse` 64-bit ints arrive as strings).
fn cell_as_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

/// One check's verdict plus the context captured while deciding it.
pub struct Evaluation {
    pub state: String,
    pub reason: Option<String>,
    pub observed: Option<String>,
    pub value: Option<f64>,
    pub query_result_id: Option<i64>,
}

impl Evaluation {
    fn new(
        state: &str,
        reason: Option<String>,
        observed: Option<String>,
        value: Option<f64>,
        query_result_id: Option<i64>,
    ) -> Self {
        Self {
            state: state.to_string(),
            reason,
            observed,
            value,
            query_result_id,
        }
    }
}

/// `"unknown"` = no result resolved; `"unevaluable"` = a result exists but the condition couldn't
/// be computed (`reason` says why). `observed` is the compared cell's display string, set only for
/// `"triggered"`/`"ok"`; `value` and `query_result_id` identify the reading.
async fn evaluate_state(
    db: &sea_orm::DatabaseConnection,
    monitor: &monitor::Model,
    org_id: i64,
) -> anyhow::Result<Evaluation> {
    crate::health::MONITOR_EVALS_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Some((qr_id, data)) = resolve_result_row(db, monitor, org_id)
        .await?
        .and_then(|qr| Some((qr.id, qr.data?)))
    else {
        return Ok(Evaluation::new("unknown", None, None, None, None));
    };
    let unevaluable = |reason: String| -> anyhow::Result<Evaluation> {
        Ok(Evaluation::new(
            "unevaluable",
            Some(reason),
            None,
            None,
            Some(qr_id),
        ))
    };

    let Some(rows) = data.get("rows").and_then(|r| r.as_array()) else {
        return unevaluable("The query result has no row data.".into());
    };
    let Some(row) = rows.first() else {
        return unevaluable("The query returned no rows to check.".into());
    };
    let option = |key: &str| monitor.options.get(key);
    let Some(col) = option("column").and_then(|v| v.as_str()) else {
        return unevaluable("No column is configured for this monitor.".into());
    };
    let Some(cell) = row.get(col) else {
        return unevaluable(format!(
            "Column \"{col}\" is not present in the query result."
        ));
    };
    let Some(cell_val) = cell_as_f64(cell) else {
        return unevaluable(format!("Column \"{col}\"'s value is not numeric."));
    };
    let observed = format_observed_cell(cell);

    let (state, observed) = if option("kind").and_then(|v| v.as_str()) == Some("baseline") {
        let sensitivity: arcodash_core::models::Sensitivity = option("sensitivity")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        #[allow(
            clippy::cast_possible_wrap,
            reason = "BASELINE_WINDOW is a small constant"
        )]
        let mut history: Vec<f64> = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT value FROM monitor_observations \
                 WHERE monitor_id = $1 AND value IS NOT NULL \
                 ORDER BY observed_at DESC, id DESC LIMIT $2",
                [monitor.id.into(), (BASELINE_WINDOW as i64).into()],
            ))
            .await?
            .iter()
            .map(|r| r.try_get::<f64>("", "value"))
            .collect::<Result<_, _>>()?;
        history.reverse();
        match baseline_verdict(&history, cell_val, sensitivity.z_threshold()) {
            BaselineVerdict::Outlier { z, median, .. } => (
                "triggered",
                format!("{observed} (median {}, z {z:.1})", format_number(median)),
            ),
            _ => ("ok", observed),
        }
    } else {
        let Some(threshold) = option("value").and_then(sea_orm::JsonValue::as_f64) else {
            return unevaluable("No comparison value is configured for this monitor.".into());
        };
        let Some(op) = option("op").and_then(|v| v.as_str()) else {
            return unevaluable("No comparison operator is configured for this monitor.".into());
        };
        let triggered = match op {
            ">" => cell_val > threshold,
            "<" => cell_val < threshold,
            ">=" => cell_val >= threshold,
            "<=" => cell_val <= threshold,
            "==" => (cell_val - threshold).abs() < f64::EPSILON,
            "!=" => (cell_val - threshold).abs() >= f64::EPSILON,
            _ => return unevaluable(format!("Unknown comparison operator \"{op}\".")),
        };
        (if triggered { "triggered" } else { "ok" }, observed)
    };
    Ok(Evaluation::new(
        state,
        None,
        Some(observed),
        Some(cell_val),
        Some(qr_id),
    ))
}

/// Adapts the never-parks direct-evaluation path (`run_id = None`) onto the conveyor SDK.
pub struct EvaluateMonitorHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TypedTaskHandler for EvaluateMonitorHandler {
    type Payload = EvaluateMonitorPayload;

    async fn run(&self, _task: TaskContext, p: EvaluateMonitorPayload) -> TaskResult {
        if let WorkGate::Blocked { reason } = self
            .ctx
            .entitlements
            .scheduled_work(&self.ctx.db, p.org_id)
            .await
        {
            tracing::info!(org_id = p.org_id, reason, "skipping scheduled work");
            return Ok(TaskOutcome::Success { result_ref: None });
        }
        Ok(
            match handle_inner(&self.ctx, p.org_id, p.monitor_id).await {
                Ok(()) => TaskOutcome::Success { result_ref: None },
                Err(e) => TaskOutcome::Failed {
                    error: format!("{e:#}"),
                },
            },
        )
    }
}

#[cfg(test)]
mod fixtures {
    //! Seeding and lookup helpers shared by this file's test modules.
    use super::*;
    use crate::scheduler::test_support::*;
    use arcodash_core::entities::event;
    use sea_orm::prelude::DateTimeWithTimeZone;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, QueryOrder};

    /// Postgres only; `None` (the test skips) without `TEST_DATABASE_URL`.
    pub(super) async fn test_db() -> Option<sea_orm::DatabaseConnection> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        };
        Some(arcodash_core::db::connect(&url).await.unwrap())
    }

    /// `value > 50` on column `value`.
    pub(super) fn over_50() -> serde_json::Value {
        serde_json::json!({ "column": "value", "op": ">", "value": 50.0 })
    }

    /// Seed an org's data source, draft query and result holding `value: row_value` (raw JSON, so
    /// number and string cells can be tested).
    pub(super) async fn seed_query_with_result(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        row_value: serde_json::Value,
    ) -> query::Model {
        let ds = seed_data_source(db, org_id).await;
        let q = seed_query(db, org_id, ds.id).await;
        let row = serde_json::json!({ "value": row_value });
        let qr = seed_query_result(db, org_id, ds.id, Utc::now(), row).await;
        set_latest_query_data(db, q.id, qr.id).await;
        query::Entity::find_by_id(q.id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// A fresh org with a published query whose published result (retrieved at `retrieved_at`)
    /// holds `value`, watched by a monitor with `options`. Returns `(org_id, monitor, qr_id)`.
    pub(super) async fn seed_published_monitor(
        db: &sea_orm::DatabaseConnection,
        retrieved_at: chrono::DateTime<Utc>,
        value: f64,
        options: serde_json::Value,
    ) -> (i64, monitor::Model, i64) {
        let org = seed_org(db, &format!("org-evalmonitor-{}", Uuid::new_v4())).await;
        let ds = seed_data_source(db, org.id).await;
        let q = seed_published_query(db, org.id, ds.id).await;
        let row = serde_json::json!({ "value": value });
        let qr = seed_query_result(db, org.id, ds.id, retrieved_at, row).await;
        set_published_latest_query_data(db, q.id, qr.id).await;
        let mon = seed_monitor(db, org.id, q.id, q.user_id, options).await;
        (org.id, mon, qr.id)
    }

    /// An in-memory (never inserted) monitor in state `unknown`.
    pub(super) fn mem_monitor(
        org_id: i64,
        query_id: i64,
        options: serde_json::Value,
    ) -> monitor::Model {
        let now = DateTimeWithTimeZone::from(Utc::now());
        monitor::Model {
            id: 0,
            org_id,
            name: "evaluate-monitor-test".to_string(),
            query_id,
            user_id: 0,
            options,
            state: "unknown".to_string(),
            last_triggered_at: None,
            rearm: None,
            created_at: now,
            updated_at: now,
            acked_at: None,
            acked_by: None,
            escalation_step: 0,
            last_escalated_at: None,
            visibility: "workspace".to_string(),
            unevaluable_reason: None,
        }
    }

    pub(super) async fn reload(db: &sea_orm::DatabaseConnection, id: i64) -> monitor::Model {
        monitor::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Sets the stored state and returns the reloaded row: the transition CAS matches on the
    /// in-memory `monitor.state`.
    pub(super) async fn set_state(
        db: &sea_orm::DatabaseConnection,
        id: i64,
        state: &str,
    ) -> monitor::Model {
        exec_sql(
            db,
            "UPDATE monitors SET state = $1 WHERE id = $2",
            [state.into(), id.into()],
        )
        .await;
        reload(db, id).await
    }

    /// Subscribes a throwaway user to `monitor_id` so a transition has a recipient.
    pub(super) async fn subscribe(db: &sea_orm::DatabaseConnection, org_id: i64, monitor_id: i64) {
        let subscriber = seed_user(db, org_id, vec![]).await;
        let now = DateTimeWithTimeZone::from(Utc::now());
        monitor_subscription::ActiveModel {
            monitor_id: Set(monitor_id),
            user_id: Set(subscriber.id),
            destination_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    pub(super) async fn delivery_count(db: &sea_orm::DatabaseConnection, monitor_id: i64) -> usize {
        notification_delivery::Entity::find()
            .filter(notification_delivery::Column::MonitorId.eq(monitor_id))
            .all(db)
            .await
            .unwrap()
            .len()
    }

    /// `monitor_state_changed` events recorded for `monitor_id`, oldest first.
    pub(super) async fn state_events(
        db: &sea_orm::DatabaseConnection,
        monitor_id: i64,
    ) -> Vec<event::Model> {
        event::Entity::find()
            .filter(event::Column::ObjectId.eq(monitor_id))
            .filter(event::Column::Action.eq("monitor_state_changed"))
            .order_by_asc(event::Column::Id)
            .all(db)
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod org_scoping_tests {
    //! `evaluate_state` must filter the query lookup by org: a monitor pointing at another org's
    //! query evaluates to "unknown". Needs `TEST_DATABASE_URL`.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::{exec_sql, seed_org};

    #[tokio::test]
    async fn cross_org_query_id_evaluates_as_unknown_not_the_foreign_result() {
        let Some(db) = test_db().await else { return };
        let suffix = Uuid::new_v4();
        let org_a = seed_org(&db, &format!("orga-evalmonitor-{suffix}")).await;
        let org_b = seed_org(&db, &format!("orgb-evalmonitor-{suffix}")).await;
        let query_b = seed_query_with_result(&db, org_b.id, serde_json::json!(100.0)).await;

        // Attack: an org-A monitor pointing at org B's query.
        let base_monitor = mem_monitor(org_a.id, query_b.id, over_50());
        let Evaluation { state, .. } = evaluate_state(&db, &base_monitor, org_a.id).await.unwrap();
        assert_eq!(
            state, "unknown",
            "a monitor's query_id pointing at another org's query must evaluate as unknown, \
             never read that foreign org's real query_result"
        );

        // Control: the same query evaluated as org B's own monitor reads real data.
        let same_org_monitor = monitor::Model {
            org_id: org_b.id,
            ..base_monitor
        };
        let Evaluation { state, .. } = evaluate_state(&db, &same_org_monitor, org_b.id)
            .await
            .unwrap();
        assert_eq!(
            state, "triggered",
            "same-org read must see the real query_result and evaluate the threshold"
        );
    }

    #[test]
    fn cell_as_f64_parses_numeric_strings_and_native_numbers() {
        assert_eq!(
            cell_as_f64(&serde_json::json!("10813421.70")),
            Some(10813421.70)
        );
        assert_eq!(cell_as_f64(&serde_json::json!("15")), Some(15.0));
        assert_eq!(cell_as_f64(&serde_json::json!(" 42 ")), Some(42.0));
        assert_eq!(cell_as_f64(&serde_json::json!(42.5)), Some(42.5));
        assert_eq!(cell_as_f64(&serde_json::json!(7)), Some(7.0));
        assert_eq!(cell_as_f64(&serde_json::json!("not-a-number")), None);
        assert_eq!(cell_as_f64(&serde_json::json!("")), None);
    }

    /// Numeric string cells (Postgres NUMERIC, ClickHouse 64-bit) must still evaluate the
    /// threshold, not fall back to "unknown".
    #[tokio::test]
    async fn string_encoded_numeric_cell_still_evaluates_the_condition() {
        let Some(db) = test_db().await else { return };
        let org = seed_org(&db, &format!("org-strnum-evalmonitor-{}", Uuid::new_v4())).await;
        let query = seed_query_with_result(&db, org.id, serde_json::json!("100.5")).await;

        let monitor = mem_monitor(org.id, query.id, over_50());
        let Evaluation { state, .. } = evaluate_state(&db, &monitor, org.id).await.unwrap();
        assert_eq!(
            state, "triggered",
            "a JSON-string-encoded numeric cell must still be compared against the threshold"
        );
    }

    /// A missing column surfaces as "unevaluable" with a reason naming it, not a bare "unknown".
    #[tokio::test]
    async fn column_missing_from_result_is_unevaluable_not_unknown_or_ok() {
        let Some(db) = test_db().await else { return };
        let org = seed_org(
            &db,
            &format!("org-missingcol-evalmonitor-{}", Uuid::new_v4()),
        )
        .await;
        let query = seed_query_with_result(&db, org.id, serde_json::json!(100.0)).await;

        let options = serde_json::json!({ "column": "revenue", "value": 50.0, "op": ">" });
        let monitor = mem_monitor(org.id, query.id, options);
        let Evaluation { state, reason, .. } = evaluate_state(&db, &monitor, org.id).await.unwrap();
        assert_eq!(state, "unevaluable");
        let reason = reason.expect("unevaluable state must carry a reason");
        assert!(
            reason.contains("revenue"),
            "reason should name the missing column, got: {reason}"
        );
    }

    /// A zero-row result is "unevaluable", distinct from "unknown".
    #[tokio::test]
    async fn empty_result_rows_is_unevaluable() {
        let Some(db) = test_db().await else { return };
        let org = seed_org(
            &db,
            &format!("org-emptyrows-evalmonitor-{}", Uuid::new_v4()),
        )
        .await;
        let query = seed_query_with_result(&db, org.id, serde_json::json!(0)).await;
        exec_sql(
            &db,
            "UPDATE query_results SET data = '{\"rows\": []}'::jsonb WHERE id = $1",
            [query.latest_query_data_id.into()],
        )
        .await;

        let monitor = mem_monitor(org.id, query.id, over_50());
        let Evaluation { state, reason, .. } = evaluate_state(&db, &monitor, org.id).await.unwrap();
        assert_eq!(state, "unevaluable");
        assert!(reason.is_some());
    }

    #[tokio::test]
    async fn resolve_monitor_result_returns_none_once_the_query_is_unpublished() {
        let Some(db) = test_db().await else { return };
        let (org_id, monitor, _) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;

        let resolved = resolve_monitor_result(&db, &monitor, org_id).await.unwrap();
        assert!(resolved.is_some());
        assert!(!query_recently_unpublished(&db, monitor.query_id, org_id)
            .await
            .unwrap());

        exec_sql(
            &db,
            "UPDATE queries SET unpublished_at = now() WHERE id = $1",
            [monitor.query_id.into()],
        )
        .await;
        let resolved = resolve_monitor_result(&db, &monitor, org_id).await.unwrap();
        assert_eq!(
            resolved, None,
            "an unpublished (but previously live) query must resolve to nothing, never the \
             draft's own result pointer"
        );
        assert!(query_recently_unpublished(&db, monitor.query_id, org_id)
            .await
            .unwrap());
    }
}

#[cfg(test)]
mod incident_reset_tests {
    //! Every `ok|unknown -> triggered` transition clears ack/escalation state; `-> ok` clears ack
    //! state only. Needs Postgres + Redis.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::{seed_org, seed_user, setup, test_ctx};

    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;

    /// Seed an org's query, result (`value: row_value`) and a `value > 50` monitor with optional
    /// leftover ack/escalation state.
    #[allow(clippy::too_many_arguments)]
    async fn seed_monitor_with_state(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        row_value: f64,
        initial_state: &str,
        acked_at: Option<chrono::DateTime<Utc>>,
        acked_by: Option<i64>,
        escalation_step: i32,
        last_escalated_at: Option<chrono::DateTime<Utc>>,
    ) -> monitor::Model {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let q = seed_query_with_result(db, org_id, serde_json::json!(row_value)).await;
        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("incident-reset-test-monitor-{}", Uuid::new_v4())),
            query_id: Set(q.id),
            user_id: Set(q.user_id),
            options: Set(over_50()),
            state: Set(initial_state.to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            acked_at: Set(acked_at.map(Into::into)),
            acked_by: Set(acked_by),
            escalation_step: Set(escalation_step),
            last_escalated_at: Set(last_escalated_at.map(Into::into)),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// A fresh org, and a `value > 50` monitor over `row_value` in `state` that a user has acked
    /// at escalation step `escalation_step`.
    async fn seed_acked(
        db: &sea_orm::DatabaseConnection,
        row_value: f64,
        state: &str,
        escalation_step: i32,
    ) -> (i64, monitor::Model) {
        let org = seed_org(db, &format!("org-incident-reset-{}", Uuid::new_v4())).await;
        let acker = seed_user(db, org.id, vec![]).await;
        let mon = seed_monitor_with_state(
            db,
            org.id,
            row_value,
            state,
            Some(Utc::now()),
            Some(acker.id),
            escalation_step,
            Some(Utc::now()),
        )
        .await;
        (org.id, mon)
    }

    /// A fresh org and an un-acked `value > 50` monitor over `row_value` in `state`.
    async fn seed_plain(
        db: &sea_orm::DatabaseConnection,
        row_value: f64,
        state: &str,
    ) -> (i64, monitor::Model) {
        let org = seed_org(db, &format!("org-incident-reset-{}", Uuid::new_v4())).await;
        let mon = seed_monitor_with_state(db, org.id, row_value, state, None, None, 0, None).await;
        (org.id, mon)
    }

    #[tokio::test]
    async fn ok_to_triggered_transition_resets_ack_and_escalation_state() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_acked(&db, 100.0, "ok", 2).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(reloaded.state, "triggered");
        assert!(
            reloaded.acked_at.is_none(),
            "a new incident must clear acked_at"
        );
        assert!(
            reloaded.acked_by.is_none(),
            "a new incident must clear acked_by"
        );
        assert_eq!(
            reloaded.escalation_step, 0,
            "a new incident must reset escalation_step to 0"
        );
        assert!(
            reloaded.last_escalated_at.is_none(),
            "a new incident must clear last_escalated_at"
        );
        assert!(
            reloaded.last_triggered_at.is_some(),
            "a fresh (non-suppressed) incident must set last_triggered_at"
        );
    }

    /// A rearm-suppressed re-trigger continues the incident: only `state` flips, with no
    /// ack/escalation reset and no notification.
    #[tokio::test]
    async fn a_rearm_suppressed_retrigger_does_not_reset_escalation_state() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_acked(&db, 100.0, "ok", 2).await;

        let mut active: monitor::ActiveModel = mon.into();
        active.rearm = Set(Some(3600));
        active.last_triggered_at = Set(Some(Utc::now().into()));
        let mon = active.update(&db).await.unwrap();

        handle(&test_ctx(db.clone(), redis.clone()), org_id, mon.id, None).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.state, "triggered",
            "the state flip itself still applies"
        );
        assert_eq!(
            reloaded.escalation_step, 2,
            "a rearm-suppressed retrigger must not reset escalation_step"
        );
        assert_eq!(
            reloaded.last_escalated_at, mon.last_escalated_at,
            "a rearm-suppressed retrigger must not clear last_escalated_at"
        );
        assert!(
            reloaded.acked_at.is_some(),
            "a rearm-suppressed retrigger must not clear an existing ack"
        );
        assert!(
            reloaded.acked_by.is_some(),
            "a rearm-suppressed retrigger must not clear an existing ack"
        );
        assert_eq!(
            reloaded.last_triggered_at, mon.last_triggered_at,
            "a rearm-suppressed retrigger must not restart last_triggered_at"
        );

        // Scoped to this monitor: the shared queue key can carry other tests' jobs.
        use redis::AsyncCommands;
        let mut conn = redis.clone();
        let items: Vec<String> = conn
            .lrange(arcodash_core::queue::QueueName::Default.queue_key(), 0, -1)
            .await
            .unwrap();
        let notified = items.iter().any(|s| {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .is_some_and(|j| j["type"] == "SendNotification" && j["monitor_id"] == mon.id)
        });
        assert!(
            !notified,
            "a rearm-suppressed retrigger must not enqueue a notification"
        );
    }

    #[tokio::test]
    async fn triggered_to_ok_transition_clears_ack_state() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_acked(&db, 10.0, "triggered", 1).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(reloaded.state, "ok");
        assert!(
            reloaded.acked_at.is_none(),
            "a -> ok transition must clear acked_at (incident over)"
        );
        assert!(
            reloaded.acked_by.is_none(),
            "a -> ok transition must clear acked_by"
        );
        // Not reset by a `-> ok` transition: the escalation tick only considers `triggered`
        // monitors.
        assert_eq!(reloaded.escalation_step, 1);
        assert!(reloaded.last_escalated_at.is_some());
    }

    #[tokio::test]
    async fn snoozed_monitor_is_skipped_entirely() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 100.0, "ok").await;

        let future = (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
        let mut active: monitor::ActiveModel = mon.into();
        active.options = Set(serde_json::json!({
            "column": "value", "value": 50.0, "op": ">", "muted_until": future,
        }));
        let mon = active.update(&db).await.unwrap();

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.state, "ok",
            "a snoozed monitor must not transition state"
        );
        assert_eq!(
            reloaded.updated_at, mon.updated_at,
            "a snoozed monitor's row must be untouched by evaluate_monitor"
        );
        assert!(
            state_events(&db, mon.id).await.is_empty(),
            "a snoozed monitor must not record a monitor_state_changed event"
        );
    }

    #[tokio::test]
    async fn evaluate_monitor_skips_expired_org() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 100.0, "ok").await;

        let mut ctx = test_ctx(db.clone(), redis);
        ctx.entitlements = Arc::new(crate::scheduler::test_support::BlockedByTest);
        handle(&ctx, org_id, mon.id, None).await;

        assert_eq!(
            reload(&db, mon.id).await.state,
            "ok",
            "a blocked org's monitor must not be evaluated at all"
        );
    }

    /// A check that can't evaluate persists `state = "unevaluable"` and the reason on the monitor
    /// row.
    #[tokio::test]
    async fn unevaluable_check_persists_reason_and_state_on_the_monitor_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 100.0, "unknown").await;
        let mut active: monitor::ActiveModel = mon.into();
        active.options = Set(serde_json::json!({ "column": "revenue", "value": 50.0, "op": ">" }));
        let mon = active.update(&db).await.unwrap();

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(reloaded.state, "unevaluable");
        let reason = reloaded.unevaluable_reason.expect(
            "the reason must persist onto the monitor row, not just the transient return value",
        );
        assert!(reason.contains("revenue"), "got: {reason}");
    }

    /// A brand-new monitor's first healthy check sends no notification, but the transition is still
    /// audited.
    #[tokio::test]
    async fn unknown_to_ok_first_check_suppresses_notification_but_still_records_event() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 10.0, "unknown").await;
        subscribe(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        assert_eq!(reload(&db, mon.id).await.state, "ok");
        assert_eq!(
            delivery_count(&db, mon.id).await,
            0,
            "unknown -> ok must not create a notification delivery"
        );
        let events = state_events(&db, mon.id).await;
        assert_eq!(
            events.len(),
            1,
            "unknown -> ok is a real transition and must still be recorded in the audit trail, \
             even though it sends no notification"
        );
        let prop = |key: &str| {
            events[0]
                .additional_properties
                .get(key)
                .and_then(|v| v.as_str())
        };
        assert_eq!(prop("old_state"), Some("unknown"));
        assert_eq!(prop("new_state"), Some("ok"));
    }

    /// A real alert must still fire on a brand-new monitor's first check.
    #[tokio::test]
    async fn unknown_to_triggered_sends_notification() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 100.0, "unknown").await;
        subscribe(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        assert_eq!(reload(&db, mon.id).await.state, "triggered");
        assert_eq!(
            delivery_count(&db, mon.id).await,
            1,
            "unknown -> triggered is a real alert and must still notify"
        );
        assert_eq!(state_events(&db, mon.id).await.len(), 1);
    }

    /// A recovery (already-known -> ok) is never suppressed — only the very first check is.
    #[tokio::test]
    async fn ok_to_triggered_sends_notification() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon) = seed_plain(&db, 100.0, "ok").await;
        subscribe(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, None).await;

        assert_eq!(reload(&db, mon.id).await.state, "triggered");
        assert_eq!(delivery_count(&db, mon.id).await, 1);
        assert_eq!(state_events(&db, mon.id).await.len(), 1);
    }
}

#[cfg(test)]
mod published_result_tests {
    //! A monitor on a published query evaluates the published result even after the draft is
    //! edited. Needs `TEST_DATABASE_URL`.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::*;

    #[tokio::test]
    async fn monitor_evaluates_published_result_after_unpublished_draft_edit() {
        let Some(db) = test_db().await else { return };
        let org = seed_org(&db, &format!("org-monitor-published-{}", Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;

        // Published result (value=100, triggers) vs a newer draft result (value=10): the published
        // pointer must win.
        let row = |value: f64| serde_json::json!({ "value": value });
        let published = seed_query_result(&db, org.id, ds.id, Utc::now(), row(100.0)).await;
        let draft = seed_query_result(&db, org.id, ds.id, Utc::now(), row(10.0)).await;
        set_published_latest_query_data(&db, q.id, published.id).await;
        set_latest_query_data(&db, q.id, draft.id).await;

        let monitor = mem_monitor(org.id, q.id, over_50());
        let Evaluation { state, .. } = evaluate_state(&db, &monitor, org.id).await.unwrap();
        assert_eq!(
            state, "triggered",
            "must evaluate the published result (value=100 > 50), not the draft's own newer \
             re-execution (value=10, which would not trigger)"
        );
    }
}

#[cfg(test)]
mod freshness_protocol_tests {
    //! The `run_id = Some` freshness protocol: fast path, attach-or-trigger, herd collapse, fan-out
    //! collision. Needs Postgres + Redis.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::*;
    use arcodash_core::queue;
    use redis::AsyncCommands;

    /// `value > 50`, fresh for 60s.
    fn over_50_fresh_60s() -> serde_json::Value {
        serde_json::json!({"column": "value", "op": ">", "value": 50.0, "max_data_staleness_secs": 60})
    }

    async fn inflight_query_runs(
        db: &sea_orm::DatabaseConnection,
        query_id: i64,
    ) -> Vec<run::Model> {
        run::Entity::find()
            .filter(run::Column::EntityType.eq("query"))
            .filter(run::Column::EntityId.eq(query_id))
            .filter(run::Column::Status.is_in(["pending", "running"]))
            .all(db)
            .await
            .unwrap()
    }

    async fn pending_monitor_run(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        mon_id: i64,
    ) -> i64 {
        seed_run(db, org_id, "monitor", mon_id, None, None, "pending", None).await
    }

    #[tokio::test]
    async fn fast_path_evaluates_fresh_result_without_triggering_a_query_run() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, qr_id) =
            seed_published_monitor(&db, Utc::now(), 100.0, over_50_fresh_60s()).await;
        let run_id = pending_monitor_run(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, Some(run_id)).await;

        let run = load_run(&db, run_id).await;
        assert_eq!(run.status, "success");
        assert_eq!(run.query_result_id, Some(qr_id));
        let age = run.data_age_secs;
        assert!(age.unwrap_or(999) < 10, "age should be small, got {age:?}");
        assert_eq!(
            run.condition_met,
            Some(true),
            "value 100 > threshold 50 must record condition_met = true"
        );
        assert_eq!(
            inflight_query_runs(&db, mon.query_id).await.len(),
            0,
            "the fast path must not trigger a query run"
        );
    }

    /// The below-threshold fast path records `condition_met = false`.
    #[tokio::test]
    async fn fast_path_ok_records_condition_met_false() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, _) =
            seed_published_monitor(&db, Utc::now(), 10.0, over_50_fresh_60s()).await;
        let run_id = pending_monitor_run(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, Some(run_id)).await;

        let run = load_run(&db, run_id).await;
        assert_eq!(run.status, "success");
        assert_eq!(
            run.condition_met,
            Some(false),
            "value 10 <= threshold 50 must record condition_met = false"
        );
    }

    #[tokio::test]
    async fn fast_path_reads_the_draft_pointer_for_an_unpublished_query() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-freshness-draft-{}", Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let row = serde_json::json!({"value": 100.0});
        let qr = seed_query_result(&db, org.id, ds.id, Utc::now(), row).await;
        set_latest_query_data(&db, q.id, qr.id).await;
        let mon = seed_monitor(&db, org.id, q.id, q.user_id, over_50_fresh_60s()).await;
        let run_id = pending_monitor_run(&db, org.id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org.id, mon.id, Some(run_id)).await;

        let run = load_run(&db, run_id).await;
        assert_eq!(run.status, "success");
        assert_eq!(
            run.query_result_id,
            Some(qr.id),
            "an unpublished query's draft pointer must be read"
        );
    }

    #[tokio::test]
    async fn stale_result_triggers_exactly_one_query_run_and_parks() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let stale_at = Utc::now() - chrono::Duration::seconds(1000);
        let (org_id, mon, _) =
            seed_published_monitor(&db, stale_at, 100.0, over_50_fresh_60s()).await;
        let run_id = pending_monitor_run(&db, org_id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_id, mon.id, Some(run_id)).await;

        let qruns = inflight_query_runs(&db, mon.query_id).await;
        assert_eq!(
            qruns.len(),
            1,
            "stale data must trigger exactly one query run"
        );
        assert_eq!(qruns[0].triggered_by, "monitor");

        let run = load_run(&db, run_id).await;
        assert_eq!(run.status, "running");
        assert_eq!(run.waiting_on_run_id, Some(qruns[0].id));
        assert!(
            run.lease_expires_at.is_none(),
            "a parked run must hold no lease"
        );
    }

    /// A monitor with no cached result triggers a first refresh instead of finalizing "unknown".
    #[tokio::test]
    async fn never_evaluated_monitor_triggers_a_first_query_run() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(&db, &format!("org-freshness-never-eval-{}", Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        let mon = seed_monitor(&db, org.id, q.id, q.user_id, over_50()).await;
        let run_id = pending_monitor_run(&db, org.id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org.id, mon.id, Some(run_id)).await;

        let qruns = inflight_query_runs(&db, q.id).await;
        assert_eq!(
            qruns.len(),
            1,
            "a never-evaluated monitor must trigger a first query run"
        );
        let run = load_run(&db, run_id).await;
        assert_eq!(
            run.status, "running",
            "the monitor run must park behind the triggered query run, not finalize immediately"
        );
        assert_eq!(run.waiting_on_run_id, Some(qruns[0].id));
    }

    /// A monitor whose query has nothing to evaluate for this org finalizes "unknown" with no query
    /// run.
    #[tokio::test]
    async fn never_evaluated_monitor_with_missing_query_finalizes_unknown() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = Uuid::new_v4();
        let org_a = seed_org(&db, &format!("orga-freshness-missing-{suffix}")).await;
        let org_b = seed_org(&db, &format!("orgb-freshness-missing-{suffix}")).await;
        let ds_b = seed_data_source(&db, org_b.id).await;
        let foreign_q = seed_query(&db, org_b.id, ds_b.id).await;
        let mon = seed_monitor(&db, org_a.id, foreign_q.id, foreign_q.user_id, over_50()).await;
        let run_id = pending_monitor_run(&db, org_a.id, mon.id).await;

        handle(&test_ctx(db.clone(), redis), org_a.id, mon.id, Some(run_id)).await;

        assert_eq!(
            inflight_query_runs(&db, foreign_q.id).await.len(),
            0,
            "no query run should be triggered when the query isn't visible to this org"
        );
        let run = load_run(&db, run_id).await;
        assert_eq!(
            run.status, "success",
            "the run must still finalize, not wedge pending"
        );
        assert_eq!(
            run.condition_met, None,
            "an unresolvable check records condition_met = NULL (unknown)"
        );
    }

    #[tokio::test]
    async fn herd_of_monitors_on_one_stale_query_collapses_to_a_single_query_run() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let stale_at = Utc::now() - chrono::Duration::seconds(1000);
        let (org_id, mon_a, _) =
            seed_published_monitor(&db, stale_at, 100.0, over_50_fresh_60s()).await;
        let mon_b = seed_monitor(
            &db,
            org_id,
            mon_a.query_id,
            mon_a.user_id,
            over_50_fresh_60s(),
        )
        .await;
        let run_a = pending_monitor_run(&db, org_id, mon_a.id).await;
        let run_b = pending_monitor_run(&db, org_id, mon_b.id).await;

        let ctx = test_ctx(db.clone(), redis);
        handle(&ctx, org_id, mon_a.id, Some(run_a)).await;
        handle(&ctx, org_id, mon_b.id, Some(run_b)).await;

        let qruns = inflight_query_runs(&db, mon_a.query_id).await;
        assert_eq!(
            qruns.len(),
            1,
            "a herd of monitors on one stale query must collapse to exactly one query run"
        );
        for run_id in [run_a, run_b] {
            assert_eq!(
                load_run(&db, run_id).await.waiting_on_run_id,
                Some(qruns[0].id)
            );
        }
    }

    /// The fan-out in `finalize_run` and an inline detach contend on `waiting_on_run_id` via
    /// `detach_cas`; the loser must see the link cleared, so a waiter resumes exactly once.
    #[tokio::test]
    async fn fan_out_wins_the_race_and_a_losing_inline_detach_is_a_no_op() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, _) = seed_published_monitor(&db, Utc::now(), 10.0, over_50()).await;
        let q = query::Entity::find_by_id(mon.query_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        let qrun_id = seed_run(
            &db,
            org_id,
            "query",
            q.id,
            None,
            Some(q.data_source_id),
            "running",
            None,
        )
        .await;
        let waiter_id = seed_run(&db, org_id, "monitor", mon.id, None, None, "running", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET waiting_on_run_id = $1, lease_expires_at = NULL WHERE id = $2",
            [qrun_id.into(), waiter_id.into()],
        )
        .await;

        let key = queue::QueueName::Default.queue_key();
        let mut conn = redis.clone();
        while conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .is_some()
        {}

        let ctx = test_ctx(db.clone(), redis.clone());
        run_lifecycle::finalize_run(
            &ctx,
            qrun_id,
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

        let inline_won = run_lifecycle::detach_cas(&db, waiter_id, qrun_id)
            .await
            .unwrap();
        assert!(
            !inline_won,
            "the fan-out must already have cleared the link; a losing inline detach must no-op"
        );

        let payload: String = conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .expect("exactly one ResumeMonitorEvaluation must have been enqueued");
        let job: queue::JobMessage = serde_json::from_str(&payload).unwrap();
        match job {
            queue::JobMessage::ResumeMonitorEvaluation {
                run_id,
                query_run_id,
                ..
            } => {
                assert_eq!(run_id, waiter_id);
                assert_eq!(query_run_id, qrun_id);
            }
            other => panic!("expected ResumeMonitorEvaluation, got {other:?}"),
        }
        assert!(
            conn.lpop::<_, Option<String>>(&key, None)
                .await
                .unwrap()
                .is_none(),
            "exactly one resume job — no duplicate"
        );
    }

    /// An `ok` monitor over a fresh triggering result (100 > 50), with a subscriber, and a
    /// `running` evaluation run whose lease expires at `lease_expires_at`.
    async fn seed_leased_evaluation(
        db: &sea_orm::DatabaseConnection,
        lease_expires_at: chrono::DateTime<Utc>,
    ) -> (i64, monitor::Model, i64, i64) {
        let (org_id, mon, qr_id) = seed_published_monitor(db, Utc::now(), 100.0, over_50()).await;
        let mon = set_state(db, mon.id, "ok").await;
        subscribe(db, org_id, mon.id).await;
        let run_id = seed_run(
            db,
            org_id,
            "monitor",
            mon.id,
            None,
            None,
            "running",
            Some(lease_expires_at),
        )
        .await;
        (org_id, mon, qr_id, run_id)
    }

    /// A sweeper-reclaimed run ("worker lost") makes the still-live handler lose the finalize CAS,
    /// so it must skip the state transition and notification.
    #[tokio::test]
    async fn lost_finalize_race_after_sweeper_reclaim_never_transitions_or_notifies() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let past = Utc::now() - chrono::Duration::seconds(120);
        let (org_id, mon, qr_id, run_id) = seed_leased_evaluation(&db, past).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        crate::scheduler::sweeper::sweep_expired_leases(&ctx).await;

        let run = load_run(&db, run_id).await;
        assert_eq!(
            run.status, "failed",
            "the sweeper must have already reclaimed the expired lease"
        );
        assert_eq!(run.error.as_deref(), Some("worker lost"));

        evaluate_and_finalize(&ctx, org_id, &mon, run_id, Some((qr_id, 0)))
            .await
            .unwrap();

        let run = load_run(&db, run_id).await;
        assert_eq!(
            run.status, "failed",
            "a lost finalize race must never overwrite the sweeper's terminal status"
        );
        assert_eq!(
            run.error.as_deref(),
            Some("worker lost"),
            "the sweeper's error must survive untouched"
        );
        assert_eq!(
            reload(&db, mon.id).await.state,
            "ok",
            "a lost finalize race must never move the monitor's state"
        );
        assert_eq!(
            delivery_count(&db, mon.id).await,
            0,
            "a lost finalize race must never create a notification_deliveries row"
        );
        assert!(
            find_and_remove_notification_for_monitor(
                &redis,
                &queue::QueueName::Default.queue_key(),
                mon.id,
            )
            .await
            .is_none(),
            "a lost finalize race must never enqueue a SendNotification"
        );
    }

    /// Positive control: with a live lease the handler does notify.
    #[tokio::test]
    async fn live_lease_evaluation_transitions_and_notifies_normally() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let future = Utc::now() + chrono::Duration::seconds(90);
        let (org_id, mon, qr_id, run_id) = seed_leased_evaluation(&db, future).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        evaluate_and_finalize(&ctx, org_id, &mon, run_id, Some((qr_id, 0)))
            .await
            .unwrap();

        assert_eq!(
            reload(&db, mon.id).await.state,
            "triggered",
            "a live-lease evaluation must apply the real state transition"
        );
        assert_eq!(delivery_count(&db, mon.id).await, 1);

        let job = find_and_remove_notification_for_monitor(
            &redis,
            &queue::QueueName::Default.queue_key(),
            mon.id,
        )
        .await
        .expect("a SendNotification must have been enqueued");
        match job {
            queue::JobMessage::SendNotification { new_state, .. } => {
                assert_eq!(new_state, "triggered");
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }

        let run = load_run(&db, run_id).await;
        assert_eq!(run.status, "success");
        assert_eq!(run.condition_met, Some(true));
    }

    async fn stored_explanation(db: &sea_orm::DatabaseConnection, run_id: i64) -> Option<String> {
        query_row(
            db,
            "SELECT explanation FROM monitor_observations WHERE run_id = $1",
            [run_id.into()],
        )
        .await
        .try_get("", "explanation")
        .unwrap()
    }

    /// Records three prior `ok` observations (10, 20, 30) so explanations have history.
    async fn seed_history(db: &sea_orm::DatabaseConnection, org_id: i64, mon_id: i64) {
        for v in [10.0, 20.0, 30.0] {
            let eval = Evaluation::new("ok", None, None, Some(v), None);
            record_observation(db, org_id, mon_id, None, &eval).await;
        }
    }

    /// Seeds a subscribed monitor (previous state opposite to the verdict, optionally rearm-armed),
    /// runs one live evaluation, and returns the observation explanation and the queued job's
    /// explanation (None if none).
    async fn explain_run(
        value: f64,
        threshold: f64,
        rearm_suppressed: bool,
    ) -> Option<(Option<String>, Option<Option<String>>)> {
        let (db, redis) = setup().await?;
        let options = serde_json::json!({"column": "value", "op": ">", "value": threshold});
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), value, options).await;
        exec_sql(
            &db,
            "UPDATE monitors SET state = $4, rearm = $2, \
             last_triggered_at = CASE WHEN $3 THEN now() ELSE NULL END WHERE id = $1",
            [
                mon.id.into(),
                (if rearm_suppressed { 3600 } else { 0 }).into(),
                rearm_suppressed.into(),
                (if value > threshold { "ok" } else { "triggered" }).into(),
            ],
        )
        .await;
        let mon = reload(&db, mon.id).await;
        subscribe(&db, org_id, mon.id).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        seed_history(&db, org_id, mon.id).await;
        let run_id = seed_live_run(&db, org_id, mon.id).await;
        evaluate_and_finalize(&ctx, org_id, &mon, run_id, Some((qr_id, 0)))
            .await
            .unwrap();
        let stored = stored_explanation(&db, run_id).await;
        let queued = find_and_remove_notification_for_monitor(
            &redis,
            &queue::QueueName::Default.queue_key(),
            mon.id,
        )
        .await
        .map(|job| match job {
            queue::JobMessage::SendNotification { explanation, .. } => explanation,
            other => panic!("expected SendNotification, got {other:?}"),
        });
        Some((stored, queued))
    }

    #[tokio::test]
    async fn triggered_transition_persists_explanation_and_enqueues_it() {
        let Some((stored, queued)) = explain_run(100.0, 50.0, false).await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let expected = "Crossed > 50: 100, previous 30, 3-check median 20.";
        assert_eq!(stored.as_deref(), Some(expected));
        assert_eq!(queued, Some(Some(expected.to_string())));
    }

    #[tokio::test]
    async fn ok_transition_has_no_explanation() {
        let Some((stored, queued)) = explain_run(100.0, 500.0, false).await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(stored, None);
        assert_eq!(queued, Some(None));
    }

    #[tokio::test]
    async fn rearm_suppressed_trigger_computes_nothing() {
        let Some((stored, queued)) = explain_run(100.0, 50.0, true).await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(stored, None);
        assert_eq!(queued, None);
    }

    #[tokio::test]
    async fn already_triggered_monitor_backfills_explanation_without_notifying() {
        let Some((first, _, queued)) = backfill_run(100.0, "triggered").await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(
            first.as_deref(),
            Some("Crossed > 50: 100, previous 30, 3-check median 20.")
        );
        assert!(!queued);
    }

    #[tokio::test]
    async fn backfill_runs_once_per_incident() {
        let Some((first, second, _)) = backfill_run(100.0, "triggered").await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert!(first.is_some());
        assert_eq!(second, None);
    }

    #[tokio::test]
    async fn ok_monitor_gets_no_backfill() {
        let Some((first, second, queued)) = backfill_run(10.0, "ok").await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!((first, second, queued), (None, None, false));
    }

    /// Evaluates a monitor in `state` twice over `value > 50`; returns each run's explanation and
    /// whether a job was queued.
    async fn backfill_run(
        value: f64,
        state: &str,
    ) -> Option<(Option<String>, Option<String>, bool)> {
        let (db, redis) = setup().await?;
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), value, over_50()).await;
        exec_sql(
            &db,
            "UPDATE monitors SET state = $2, rearm = 0 WHERE id = $1",
            [mon.id.into(), state.into()],
        )
        .await;
        let mon = reload(&db, mon.id).await;
        subscribe(&db, org_id, mon.id).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        seed_history(&db, org_id, mon.id).await;
        let mut stored = Vec::new();
        for _ in 0..2 {
            let run_id = seed_live_run(&db, org_id, mon.id).await;
            evaluate_and_finalize(&ctx, org_id, &mon, run_id, Some((qr_id, 0)))
                .await
                .unwrap();
            stored.push(stored_explanation(&db, run_id).await);
        }
        let queued = find_and_remove_notification_for_monitor(
            &redis,
            &queue::QueueName::Default.queue_key(),
            mon.id,
        )
        .await
        .is_some();
        let second = stored.pop().unwrap();
        Some((stored.pop().unwrap(), second, queued))
    }

    async fn observation_rows(
        db: &sea_orm::DatabaseConnection,
        monitor_id: i64,
    ) -> Vec<(Option<f64>, String, Option<i64>)> {
        db.query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT value, state, run_id FROM monitor_observations \
             WHERE monitor_id = $1 ORDER BY id",
            [monitor_id.into()],
        ))
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.try_get("", "value").unwrap(),
                r.try_get("", "state").unwrap(),
                r.try_get("", "run_id").unwrap(),
            )
        })
        .collect()
    }

    async fn seed_live_run(db: &sea_orm::DatabaseConnection, org_id: i64, mon_id: i64) -> i64 {
        let future = Utc::now() + chrono::Duration::seconds(90);
        seed_run(
            db,
            org_id,
            "monitor",
            mon_id,
            None,
            None,
            "running",
            Some(future),
        )
        .await
    }

    #[tokio::test]
    async fn observation_written_for_ok_and_triggered() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        let run1 = seed_live_run(&db, org_id, mon.id).await;
        evaluate_and_finalize(&ctx, org_id, &mon, run1, Some((qr_id, 0)))
            .await
            .unwrap();
        let rows = observation_rows(&db, mon.id).await;
        assert_eq!(
            rows,
            vec![(Some(100.0), "triggered".to_string(), Some(run1))]
        );

        let mon = monitor::Model {
            options: serde_json::json!({"column": "value", "op": ">", "value": 500.0}),
            ..reload(&db, mon.id).await
        };
        let run2 = seed_live_run(&db, org_id, mon.id).await;
        evaluate_and_finalize(&ctx, org_id, &mon, run2, Some((qr_id, 0)))
            .await
            .unwrap();
        let rows = observation_rows(&db, mon.id).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], (Some(100.0), "ok".to_string(), Some(run2)));
    }

    async fn set_result_row(db: &sea_orm::DatabaseConnection, qr_id: i64, value: f64) {
        exec_sql(
            db,
            "UPDATE query_results SET data = jsonb_build_object('rows', jsonb_build_array($1::jsonb)) \
             WHERE id = $2",
            [serde_json::json!({ "value": value }).into(), qr_id.into()],
        )
        .await;
    }

    async fn eval_once(
        db: &sea_orm::DatabaseConnection,
        ctx: &ExecuteQueryContext,
        org_id: i64,
        mon_id: i64,
        qr_id: i64,
    ) -> String {
        let mon = reload(db, mon_id).await;
        let run = seed_live_run(db, org_id, mon_id).await;
        evaluate_and_finalize(ctx, org_id, &mon, run, Some((qr_id, 0)))
            .await
            .unwrap();
        observation_rows(db, mon_id).await.pop().unwrap().1
    }

    fn baseline() -> serde_json::Value {
        serde_json::json!({"column": "value", "kind": "baseline"})
    }

    #[tokio::test]
    async fn baseline_learning_stays_ok_for_first_seven() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, baseline()).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        for i in 0..7 {
            let v = if i == 6 { 100_000.0 } else { 100.0 };
            set_result_row(&db, qr_id, v).await;
            assert_eq!(eval_once(&db, &ctx, org_id, mon.id, qr_id).await, "ok");
        }
    }

    #[tokio::test]
    async fn baseline_spike_after_seven_steady_triggers() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, baseline()).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        for i in 0..7 {
            set_result_row(&db, qr_id, 100.0 + (i % 2) as f64).await;
            assert_eq!(eval_once(&db, &ctx, org_id, mon.id, qr_id).await, "ok");
        }
        set_result_row(&db, qr_id, 400.0).await;
        assert_eq!(
            eval_once(&db, &ctx, org_id, mon.id, qr_id).await,
            "triggered"
        );
    }

    async fn baseline_first_check(opts: serde_json::Value) -> Option<String> {
        let (db, redis) = setup().await?;
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, opts).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        Some(eval_once(&db, &ctx, org_id, mon.id, qr_id).await)
    }

    #[tokio::test]
    async fn baseline_ignores_op_and_value() {
        let opts =
            serde_json::json!({"column": "value", "kind": "baseline", "op": ">", "value": 0.0});
        let Some(state) = baseline_first_check(opts).await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(state, "ok");
    }

    #[tokio::test]
    async fn baseline_without_op_or_value_is_not_unevaluable() {
        let Some(state) = baseline_first_check(baseline()).await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(state, "ok");
    }

    #[tokio::test]
    async fn unevaluable_observation_has_null_value() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let opts = serde_json::json!({"column": "missing", "op": ">", "value": 50.0});
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, opts).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        let run = seed_live_run(&db, org_id, mon.id).await;
        evaluate_and_finalize(&ctx, org_id, &mon, run, Some((qr_id, 0)))
            .await
            .unwrap();
        let rows = observation_rows(&db, mon.id).await;
        assert_eq!(rows, vec![(None, "unevaluable".to_string(), Some(run))]);
    }

    #[tokio::test]
    async fn unknown_writes_no_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, mon, _) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;
        let eval = Evaluation::new("unknown", None, None, None, None);
        assert_eq!(
            record_observation(&db, org_id, mon.id, None, &eval).await,
            None
        );
        assert!(observation_rows(&db, mon.id).await.is_empty());
    }

    #[tokio::test]
    async fn lost_finalize_race_writes_no_observation() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon, qr_id) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;
        let past = Utc::now() - chrono::Duration::seconds(120);
        let run = seed_run(
            &db,
            org_id,
            "monitor",
            mon.id,
            None,
            None,
            "running",
            Some(past),
        )
        .await;
        let ctx = test_ctx(db.clone(), redis.clone());
        crate::scheduler::sweeper::sweep_expired_leases(&ctx).await;
        evaluate_and_finalize(&ctx, org_id, &mon, run, Some((qr_id, 0)))
            .await
            .unwrap();
        assert!(observation_rows(&db, mon.id).await.is_empty());
    }

    #[tokio::test]
    async fn observations_pruned_to_500_newest_kept() {
        let Some(db) = test_db().await else { return };
        let (org_id, mon, _) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;
        exec_sql(
            &db,
            "INSERT INTO monitor_observations (org_id, monitor_id, observed_at, value, state) \
             SELECT $1, $2, now() - (g || ' seconds')::interval, g, 'ok' \
             FROM generate_series(1, 505) g",
            [org_id.into(), mon.id.into()],
        )
        .await;
        let eval = Evaluation::new("triggered", None, None, Some(0.5), None);
        record_observation(&db, org_id, mon.id, None, &eval)
            .await
            .unwrap();
        let rows = observation_rows(&db, mon.id).await;
        assert_eq!(rows.len(), 500);
        assert!(rows.iter().any(|r| r.0 == Some(0.5)));
        assert!(!rows.iter().any(|r| r.0 == Some(505.0)));
        assert!(rows.iter().any(|r| r.0 == Some(1.0)));
    }
}

#[cfg(test)]
mod conveyor_dispatch_tests {
    //! The conveyor path for the `run_id = None` seam end to end: real enqueue, dispatch and state
    //! transition. Needs Postgres + Redis.
    use super::fixtures::*;
    use super::*;
    use arcodash_core::queue::ValkeyQueue;

    #[test]
    fn flag_off_by_default() {
        std::env::remove_var("CONVEYOR_EVALUATE_MONITOR_ENABLED");
        assert!(!arcodash_core::monitor_eval::conveyor_evaluate_monitor_enabled());
    }

    #[tokio::test]
    async fn conveyor_dispatch_evaluates_and_transitions_the_real_monitor_row() {
        let Some(db) = test_db().await else { return };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let (org_id, mon, _) = seed_published_monitor(&db, Utc::now(), 100.0, over_50()).await;
        assert_eq!(mon.state, "unknown");

        let ctx = Arc::new(crate::scheduler::test_support::test_ctx(
            db.clone(),
            redis.clone(),
        ));

        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis.clone(), redis_url)
                .register(
                    "evaluate_monitor",
                    EvaluateMonitorHandler { ctx: ctx.clone() },
                )
                .tier("email", &["evaluate_monitor"], 4)
                .build();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        // Process-global env var; restored right after the enqueue that needs it.
        std::env::set_var("CONVEYOR_EVALUATE_MONITOR_ENABLED", "true");
        let queue_impl = ValkeyQueue::new(redis.clone());
        arcodash_core::monitor_eval::enqueue_evaluate_monitor(
            &queue_impl,
            Some(&conveyor),
            org_id,
            mon.id,
            None,
        )
        .await
        .unwrap();
        std::env::remove_var("CONVEYOR_EVALUATE_MONITOR_ENABLED");

        // Poll on `finished_at IS NOT NULL`: a task row is visible with a non-terminal status
        // before dispatch completes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT status, finished_at FROM conveyor_tasks \
                     WHERE task_type = 'evaluate_monitor' AND entity_id = $1",
                    [mon.id.into()],
                ))
                .await
                .unwrap();
            if let Some(row) = &row {
                let finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone> =
                    row.try_get("", "finished_at").unwrap();
                if finished_at.is_some() {
                    break row.try_get::<String>("", "status").unwrap();
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "conveyor never finished the evaluate_monitor task within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        let _ = shutdown_tx.send(true);
        run_handle.await.unwrap();

        assert_eq!(status, "success");
        assert_eq!(
            reload(&db, mon.id).await.state,
            "triggered",
            "the real conveyor dispatch must have driven the actual state transition, not just \
             closed out the task row"
        );
    }
}

#[cfg(test)]
mod entitlement_gate_tests {
    //! A run-backed evaluation for a blocked org finalizes the run `skipped` instead of leaving it
    //! `pending`.
    use super::*;
    use crate::scheduler::test_support::*;

    #[tokio::test]
    async fn blocked_org_finalizes_a_run_backed_evaluation_as_skipped() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org = seed_org(
            &db,
            &format!("org-monitor-entitlement-gate-{}", Uuid::new_v4()),
        )
        .await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let mon = seed_monitor(&db, org.id, q.id, q.user_id, serde_json::json!({})).await;
        let run_id = seed_run(&db, org.id, "monitor", mon.id, None, None, "pending", None).await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.entitlements = Arc::new(BlockedByTest);
        handle(&ctx, org.id, mon.id, Some(run_id)).await;

        let reloaded = load_run(&db, run_id).await;
        assert_eq!(
            reloaded.status, "skipped",
            "a blocked run-backed evaluation must finalize skipped, not stay pending"
        );
        assert_eq!(reloaded.error.as_deref(), Some("blocked by test"));
    }
}
