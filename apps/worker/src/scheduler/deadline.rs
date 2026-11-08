//! The deadline loop: sleep until `MIN(next_run_at)` (or a pub/sub nudge), claim due schedules
//! under `FOR UPDATE SKIP LOCKED`, plan one `runs` row per tick (applying `overlap_policy`), then
//! enqueue subject to each data source's concurrency cap. Correctness never depends on the leader
//! lock; it is an efficiency-only optimization.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use redis::aio::PubSubStream;
use redis::AsyncCommands;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait, Statement,
    TransactionTrait,
};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use uuid::Uuid;

use arcodash_core::entities::{run, schedule};
use arcodash_core::schedule_spec::{compute_next, ScheduleSpec};

use crate::handlers::execute_query::ExecuteQueryContext;
use crate::run_lifecycle;

/// Floor sleep after a `claim_and_plan` failure; without it a persistent failure leaves schedules
/// overdue and the loop retries with no rate limit.
const CLAIM_ERROR_FLOOR: Duration = Duration::from_secs(5);
/// How often the loop retries `subscribe_wake` after losing the pub/sub connection.
const WAKE_RESUBSCRIBE_INTERVAL: Duration = Duration::from_secs(30);
const LEADER_LOCK_KEY: &str = "arcodash:scheduler:leader";
const MAX_SLEEP_SECS: u64 = 120;
pub const SWEEP_INTERVAL_SECS: u64 = 60;
pub const STALE_PENDING_SECS: i64 = 60;
const DEFAULT_DS_CONCURRENCY: i64 = 4;
const CLAIM_LIMIT: i64 = 100;

/// Schedules eligible to fire as of `$1`, ignoring `next_run_at`: enabled, not auto-paused, not
/// paused, and for a `query` schedule, its query currently published. Shared by the claim and the
/// sleep computation so a paused row with a past `next_run_at` can't pin the sleep to 0.
const ELIGIBLE_SCHEDULE: &str = "s.enabled AND s.auto_paused_at IS NULL \
    AND (s.paused_until IS NULL OR s.paused_until <= $1) \
    AND (s.entity_type <> 'query' OR EXISTS ( \
      SELECT 1 FROM queries q \
      WHERE q.id = s.entity_id \
        AND q.published_change_id IS NOT NULL AND q.unpublished_at IS NULL \
    ))";

/// `ABANDONED_PENDING_SECS`: how old an enqueued-but-never-claimed `pending` run must be before the
/// sweeper finalizes it as `timed_out`. Must exceed the longest legitimate wait for a worker claim
/// (bounded by tier concurrency); raise alongside `RUN_TIMEOUT_SECS` or a backlogged long tier.
pub fn abandoned_pending_secs() -> i64 {
    arcodash_core::config::env_parsed("ABANDONED_PENDING_SECS", 3600)
}

/// `MONITOR_WAIT_TIMEOUT`: hard ceiling (seconds) a monitor run may stay parked on an in-flight
/// query run before the sweeper fails it, even when `max_fresh_wait_secs` is unset (always wait).
pub fn monitor_wait_timeout_secs() -> i64 {
    arcodash_core::config::env_parsed("MONITOR_WAIT_TIMEOUT", 7200)
}

/// Run the deadline loop until `shutdown` fires.
pub async fn run(
    ctx: Arc<ExecuteQueryContext>,
    redis_url: String,
    mut shutdown: watch::Receiver<bool>,
) {
    let lock_id = Uuid::new_v4().to_string();
    let mut wake = subscribe_wake(&redis_url).await;
    if wake.is_none() {
        tracing::warn!(
            "scheduler: pub/sub wake subscription unavailable, falling back to the 120s sleep cap"
        );
    }
    let mut last_wake_attempt = Instant::now();
    let mut last_sweep = Instant::now()
        .checked_sub(Duration::from_secs(SWEEP_INTERVAL_SECS))
        .unwrap();

    tracing::info!("scheduler: deadline engine started");

    while !*shutdown.borrow() {
        if wake.is_none() && last_wake_attempt.elapsed() >= WAKE_RESUBSCRIBE_INTERVAL {
            last_wake_attempt = Instant::now();
            wake = subscribe_wake(&redis_url).await;
        }

        if !super::acquire_leader_lock(&ctx.redis, LEADER_LOCK_KEY, &lock_id).await {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = shutdown.changed() => {}
            }
            continue;
        }

        let now = Utc::now();

        let planned: Vec<run::Model> = match claim_and_plan(&ctx, now).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "scheduler: claim_and_plan failed this tick");
                tokio::select! {
                    () = tokio::time::sleep(CLAIM_ERROR_FLOOR) => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            }
        };

        for r in &planned {
            if let Err(e) = try_enqueue_run(&ctx, r).await {
                tracing::warn!(run_id = r.id, error = %e, "scheduler: enqueue failed (sweeper will retry)");
            }
        }

        if last_sweep.elapsed() >= Duration::from_secs(SWEEP_INTERVAL_SECS) {
            let sweep_started = Instant::now();
            let expired_leases = super::sweeper::sweep_expired_leases(&ctx).await;
            let abandoned_pending = super::sweeper::fail_abandoned_pending(&ctx).await;
            let stale_pending = super::sweeper::requeue_stale_pending(&ctx).await;
            let waiting_runs = super::sweeper::sweep_waiting_runs(&ctx).await;
            let stranded_monitor_runs = super::sweeper::sweep_stranded_monitor_runs(&ctx).await;
            let expired_delivery_leases =
                super::sweeper::reclaim_expired_delivery_leases(&ctx).await;
            let stale_pending_notifications =
                super::sweeper::requeue_stale_pending_notifications(&ctx).await;
            let stale_pending_mail = super::sweeper::requeue_stale_pending_mail(&ctx).await;
            let run_task_divergence = super::sweeper::reconcile_run_task_divergence(&ctx).await;
            let total = expired_leases
                + abandoned_pending
                + stale_pending
                + waiting_runs
                + stranded_monitor_runs
                + expired_delivery_leases
                + stale_pending_notifications
                + stale_pending_mail
                + run_task_divergence;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a sweep cycle's duration in milliseconds fits in u64"
            )]
            let elapsed_ms = sweep_started.elapsed().as_millis() as u64;
            crate::health::SWEEP_CYCLE_DURATION_MS
                .store(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
            if total > 0 || elapsed_ms > 1_000 {
                tracing::info!(
                    expired_leases,
                    abandoned_pending,
                    stale_pending,
                    waiting_runs,
                    stranded_monitor_runs,
                    expired_delivery_leases,
                    stale_pending_notifications,
                    stale_pending_mail,
                    run_task_divergence,
                    total,
                    elapsed_ms,
                    "scheduler: sweep cycle complete"
                );
            }
            last_sweep = Instant::now();
        }

        // Re-sample the clock: the work above takes variable time and sleeping against the loop-top
        // `now` would wake late.
        let now = Utc::now();
        let next = min_next_run_at(&ctx.db, now).await;
        let dur = sleep_until(next, now);

        tokio::select! {
            () = tokio::time::sleep(dur) => {}
            () = recv_wake(&mut wake) => {}
            _ = shutdown.changed() => {}
        }
    }

    super::release_leader_lock(&ctx.redis, LEADER_LOCK_KEY, &lock_id).await;
    tracing::info!("scheduler: stopped");
}

/// `ConnectionManager` can't hold a blocking `SUBSCRIBE`, so this opens one dedicated pub/sub
/// connection from `redis_url`. `None` means the subscription failed or died; the loop relies on
/// the `MAX_SLEEP_SECS` cap until `run` retries (every `WAKE_RESUBSCRIBE_INTERVAL`).
async fn subscribe_wake(redis_url: &str) -> Option<PubSubStream> {
    let client = redis::Client::open(redis_url).ok()?;
    let mut pubsub = client.get_async_pubsub().await.ok()?;
    pubsub
        .subscribe(arcodash_core::redis_util::SCHED_WAKE_CHANNEL)
        .await
        .ok()?;
    Some(pubsub.into_on_message())
}

/// Awaits the next wake message, or pends forever with no subscription. A closed stream (`next()`
/// returns `Ready(None)` forever) is cleared to `None` rather than polled again, which would spin
/// `run`'s `select!` with zero delay.
async fn recv_wake(wake: &mut Option<PubSubStream>) {
    let dead = match wake {
        Some(stream) => stream.next().await.is_none(),
        None => std::future::pending::<bool>().await,
    };
    if dead {
        tracing::warn!(
            "scheduler: pub/sub wake connection closed, falling back to the sleep cap until it reconnects"
        );
        *wake = None;
    }
}

/// Claim every due, enabled, unpaused schedule (`FOR UPDATE SKIP LOCKED`), advance each
/// `next_run_at` in the same transaction so it can't re-fire while in flight, and plan a `runs` row
/// for each. Returns only runs that need enqueuing (skip/coalesce/no-op cases are filtered out). A
/// `query` schedule also requires its query to be currently published; filtering here freezes
/// `next_run_at` while unpublished instead of writing a `skipped` run every tick, so it resumes
/// where it left off.
async fn claim_and_plan(
    ctx: &ExecuteQueryContext,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<run::Model>> {
    let txn = ctx.db.begin().await?;

    let due: Vec<schedule::Model> = schedule::Entity::find()
        .from_raw_sql(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT s.* FROM schedules s \
                 WHERE s.next_run_at <= $1 AND {ELIGIBLE_SCHEDULE} \
                 ORDER BY s.next_run_at LIMIT $2 \
                 FOR UPDATE SKIP LOCKED"
            ),
            [now.into(), CLAIM_LIMIT.into()],
        ))
        .all(&txn)
        .await?;

    let mut planned = Vec::new();
    let mut coalesced = Vec::new();
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
            "UPDATE schedules SET next_run_at = $1, updated_at = now() WHERE id = $2",
            [next.into(), s.id.into()],
        ))
        .await?;

        match plan_run(&txn, &s, tick).await? {
            Planned::Run(run) => planned.push(*run),
            Planned::Coalesced {
                entity_type,
                entity_id,
                tick,
            } => coalesced.push((entity_type, entity_id, tick)),
            Planned::None => {}
        }
    }

    txn.commit().await?;

    // The coalesce record is advisory with no ordering requirement against the commit, so it is
    // written after commit to keep a slow Redis off the `FOR UPDATE` lock window.
    for (entity_type, entity_id, tick) in coalesced {
        let key = coalesce_key(&entity_type, entity_id);
        let mut conn = ctx.redis.clone();
        let result: Result<bool, _> = conn
            .set_options(
                key,
                tick.to_rfc3339(),
                redis::SetOptions::default()
                    .conditional_set(redis::ExistenceCheck::NX)
                    .with_expiration(redis::SetExpiry::EX(86_400)),
            )
            .await;
        // A failed record must not fail the plan pass; only this collapsed tick is lost.
        if let Err(e) = result {
            tracing::warn!(
                entity_type = %entity_type,
                entity_id,
                error = %e,
                "scheduler: failed to record a coalesced tick"
            );
        }
    }

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
        "scheduler: auto-pausing a schedule with an invalid spec/timezone"
    );
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE schedules SET auto_paused_at = now(), updated_at = now() WHERE id = $1",
        [schedule_id.into()],
    ))
    .await?;
    Ok(())
}

/// Outcome of planning one schedule's due tick. `Coalesced` carries the coalesce-key write that
/// `claim_and_plan` performs after commit.
enum Planned {
    Run(Box<run::Model>),
    Coalesced {
        entity_type: String,
        entity_id: i64,
        tick: DateTime<Utc>,
    },
    None,
}

/// Plan one `runs` row for `s`'s due `tick`. `ON CONFLICT DO NOTHING` can trip
/// `uq_runs_entity_tick` or `uq_runs_inflight`; on no row returned, a follow-up `SELECT` tells them
/// apart (idempotent no-op vs. `overlap_policy`).
async fn plan_run(
    txn: &DatabaseTransaction,
    s: &schedule::Model,
    tick: DateTime<Utc>,
) -> anyhow::Result<Planned> {
    let data_source_id: Option<i64> = if s.entity_type == "query" {
        txn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT data_source_id FROM queries WHERE id = $1",
            [s.entity_id.into()],
        ))
        .await?
        .map(|r| r.try_get("", "data_source_id"))
        .transpose()?
    } else {
        None
    };

    let job_id = Uuid::new_v4().to_string();
    let inserted = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, data_source_id, \
                               scheduled_for, triggered_by, status, job_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'schedule', 'pending', $7, now()) \
             ON CONFLICT DO NOTHING \
             RETURNING id",
            [
                s.org_id.into(),
                s.entity_type.clone().into(),
                s.entity_id.into(),
                s.id.into(),
                data_source_id.into(),
                tick.into(),
                job_id.into(),
            ],
        ))
        .await?;

    if let Some(row) = inserted.into_iter().next() {
        let id: i64 = row.try_get("", "id")?;
        let run = run::Entity::find_by_id(id).one(txn).await?.ok_or_else(|| {
            anyhow::anyhow!("just-inserted run {id} vanished within the same transaction")
        })?;
        return Ok(Planned::Run(Box::new(run)));
    }

    // Conflict: tell uq_runs_entity_tick (idempotent no-op) from uq_runs_inflight (apply
    // overlap_policy) via an existence check on the tick-scoped index.
    let tick_already_planned = !txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS one FROM runs \
             WHERE entity_type = $1 AND entity_id = $2 AND scheduled_for = $3 \
               AND schedule_id IS NOT NULL LIMIT 1",
            [
                s.entity_type.clone().into(),
                s.entity_id.into(),
                tick.into(),
            ],
        ))
        .await?
        .is_empty();

    if tick_already_planned {
        return Ok(Planned::None);
    }

    match s.overlap_policy.as_str() {
        "coalesce" => Ok(Planned::Coalesced {
            entity_type: s.entity_type.clone(),
            entity_id: s.entity_id,
            tick,
        }),
        other => {
            if other != "skip" {
                // parallel/replace are rejected at write time; fall back to skip so a stray row
                // can't wedge the loop.
                tracing::warn!(
                    schedule_id = s.id,
                    overlap_policy = other,
                    "scheduler: unsupported overlap_policy, treating as skip"
                );
            }
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, scheduled_for, \
                                   triggered_by, status, created_at) \
                 VALUES ($1, $2, $3, $4, $5, 'schedule', 'skipped', now()) \
                 ON CONFLICT DO NOTHING",
                [
                    s.org_id.into(),
                    s.entity_type.clone().into(),
                    s.entity_id.into(),
                    s.id.into(),
                    tick.into(),
                ],
            ))
            .await?;
            Ok(Planned::None)
        }
    }
}

fn coalesce_key(entity_type: &str, entity_id: i64) -> String {
    format!("coalesce:{entity_type}:{entity_id}")
}

/// Enqueue a planned run unless its data source is at its concurrency cap, in which case it stays
/// `pending` for the sweeper/a later tick. The cap is soft: two leaders can transiently overshoot
/// by one.
pub async fn try_enqueue_run(ctx: &ExecuteQueryContext, run: &run::Model) -> anyhow::Result<bool> {
    if let Some(ds) = run.data_source_id {
        let cap = ds_cap(&ctx.db, ds).await?;
        let running = count_running_on_ds(&ctx.db, ds, run.id).await?;
        if running >= cap {
            return Ok(false);
        }
    }
    run_lifecycle::enqueue_run(ctx, run).await
}

async fn ds_cap(db: &DatabaseConnection, ds: i64) -> anyhow::Result<i64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT COALESCE(max_concurrent_runs, $2) AS cap FROM data_sources WHERE id = $1",
            [ds.into(), DEFAULT_DS_CONCURRENCY.into()],
        ))
        .await?;
    Ok(row
        .map(|r| r.try_get("", "cap"))
        .transpose()?
        .unwrap_or(DEFAULT_DS_CONCURRENCY))
}

/// Counts toward a data source's cap every other `running` run plus every other enqueued `pending`
/// run (`enqueued_at IS NOT NULL`). A `pending` row parked over the cap must not count, or the cap
/// would self-deadlock. `exclude_run_id` is the row being admitted, so a re-checking row never
/// refuses itself. Counting enqueued-but-unclaimed rows closes a burst overshoot at the cost of
/// slightly lower burst throughput, by design.
async fn count_running_on_ds(
    db: &DatabaseConnection,
    ds: i64,
    exclude_run_id: i64,
) -> anyhow::Result<i64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT count(*) AS c FROM runs WHERE data_source_id = $1 AND id <> $2 \
             AND (status = 'running' OR (status = 'pending' AND enqueued_at IS NOT NULL))",
            [ds.into(), exclude_run_id.into()],
        ))
        .await?;
    Ok(row.map(|r| r.try_get("", "c")).transpose()?.unwrap_or(0))
}

/// Earliest `next_run_at` among eligible schedules ([`ELIGIBLE_SCHEDULE`]).
async fn min_next_run_at(db: &DatabaseConnection, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let row = match db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT MIN(s.next_run_at) AS min_next FROM schedules s WHERE {ELIGIBLE_SCHEDULE}"
            ),
            [now.into()],
        ))
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            // Log a transient DB blip: it becomes the loop's full MAX_SLEEP_SECS backoff, otherwise
            // indistinguishable from "nothing scheduled".
            tracing::warn!(error = %e, "scheduler: failed to compute the next due time, backing off");
            return None;
        }
    };
    // A shape error here is a schema bug, not an availability blip, so no separate log.
    row.try_get::<Option<DateTime<Utc>>>("", "min_next")
        .ok()
        .flatten()
}

/// Duration to sleep given the next due time and the current clock, capped at `MAX_SLEEP_SECS`.
fn sleep_until(next: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
    match next {
        Some(t) if t > now => (t - now)
            .to_std()
            .unwrap_or_default()
            .min(Duration::from_secs(MAX_SLEEP_SECS)),
        Some(_) => Duration::from_millis(0),
        None => Duration::from_secs(MAX_SLEEP_SECS),
    }
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::test_support::*;

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
        assert_eq!(sleep_until(None, now), Duration::from_secs(MAX_SLEEP_SECS));
    }

    #[tokio::test]
    async fn claim_and_plan_is_idempotent_under_two_concurrent_leaders() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-claim-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        // A `query` schedule can only be created against a published query.
        let q = seed_published_query(&db, org.id, ds.id).await;
        let due_at = Utc::now() - chrono::Duration::seconds(5);
        seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let ctx1 = Arc::new(test_ctx(db.clone(), redis.clone()));
        let ctx2 = Arc::new(test_ctx(db.clone(), redis.clone()));
        let now = Utc::now();

        // Two "leaders" racing one due schedule. Assert on the scoped query below, not the raw
        // `planned` count: `claim_and_plan` is system-wide, so a shared DB may plan unrelated
        // schedules.
        let (a, b) = tokio::join!(claim_and_plan(&ctx1, now), claim_and_plan(&ctx2, now));
        a.unwrap();
        b.unwrap();

        let rows = poll_rows(&db, SCHEDULED_RUNS, q.id).await;
        assert_eq!(rows.len(), 1, "exactly one run row for this tick, not two");
    }

    /// A `query` schedule left running past unpublish must not write a `skipped` run every tick
    /// (that would flush the real run history); it sits frozen until republished.
    #[tokio::test]
    async fn claim_and_plan_freezes_a_query_schedule_once_unpublished_and_resumes_on_republish() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-unpub-sched-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        exec_sql(
            &db,
            "UPDATE queries SET unpublished_at = now() WHERE id = $1",
            [q.id.into()],
        )
        .await;

        let due_at = Utc::now() - chrono::Duration::seconds(5);
        let sched = seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        claim_and_plan(&ctx, Utc::now()).await.unwrap();

        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM runs WHERE entity_type='query' AND entity_id=$1",
                [q.id.into()],
            ))
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "an unpublished query's due schedule must not write any run row, skipped or otherwise"
        );

        let reloaded = schedule::Entity::find_by_id(sched.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let unchanged_next = reloaded
            .next_run_at
            .map(|t| t.with_timezone(&Utc))
            .expect("schedule must keep a next_run_at");
        assert!(
            (unchanged_next - due_at).num_milliseconds().abs() < 1000,
            "next_run_at must stay frozen at the missed tick while unpublished, not advance to \
             the following one: got {unchanged_next}, expected ~{due_at}"
        );

        exec_sql(
            &db,
            "UPDATE queries SET unpublished_at = NULL WHERE id = $1",
            [q.id.into()],
        )
        .await;
        claim_and_plan(&ctx, Utc::now()).await.unwrap();

        let rows = poll_rows(&db, SCHEDULED_RUNS, q.id).await;
        assert_eq!(
            rows.len(),
            1,
            "republishing must let the frozen tick fire exactly once, not zero and not a burst"
        );
    }

    #[tokio::test]
    async fn overlap_skip_records_a_visible_skipped_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-skip-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        // A `query` schedule can only be created against a published query.
        let q = seed_published_query(&db, org.id, ds.id).await;
        // A prior in-flight run already occupies uq_runs_inflight for this entity.
        seed_query_run(&db, &q, "running", None).await;

        let due_at = Utc::now() - chrono::Duration::seconds(5);
        seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            due_at,
            0,
        )
        .await;

        let ctx = Arc::new(test_ctx(db.clone(), redis.clone()));
        let planned = claim_and_plan(&ctx, Utc::now()).await.unwrap();
        // `planned` may carry a concurrent sibling test's schedule; scope to this entity.
        assert!(
            planned.iter().all(|r| r.entity_id != q.id),
            "an in-flight run must not be double-planned"
        );

        let rows = poll_rows(
            &db,
            "SELECT status FROM runs WHERE entity_type='query' AND entity_id=$1 AND status='skipped'",
            q.id,
        )
        .await;
        assert_eq!(rows.len(), 1, "a visible skipped row must be recorded");
    }

    const SCHEDULED_RUNS: &str =
        "SELECT id FROM runs WHERE entity_type='query' AND entity_id=$1 AND schedule_id IS NOT NULL";

    /// `claim_and_plan` is system-wide, so a concurrent sibling test's call may process this test's
    /// row instead; poll `sql` (bound to `query_id`) until it returns rows rather than assert once.
    async fn poll_rows(
        db: &sea_orm::DatabaseConnection,
        sql: &str,
        query_id: i64,
    ) -> Vec<sea_orm::QueryResult> {
        for _ in 0..50 {
            let rows = db
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    sql,
                    [query_id.into()],
                ))
                .await
                .unwrap();
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Vec::new()
    }

    #[tokio::test]
    async fn overlap_coalesce_sets_redis_key_then_finalize_produces_one_followup() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-coalesce-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        // Published: the coalesce follow-up goes through `enqueue_run`, which only enqueues
        // published queries.
        let q = seed_published_query(&db, org.id, ds.id).await;
        let running_id = seed_query_run(&db, &q, "running", None).await;

        let due_at = Utc::now() - chrono::Duration::seconds(5);
        seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "coalesce",
            due_at,
            0,
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let planned = claim_and_plan(&ctx, Utc::now()).await.unwrap();
        // `planned` may carry a concurrent sibling test's schedule; scope to this entity.
        assert!(
            planned.iter().all(|r| r.entity_id != q.id),
            "coalesce must not plan an immediate run"
        );

        let key = coalesce_key("query", q.id);
        let mut conn = redis.clone();
        // A sibling test's `claim_and_plan` may process this row instead, so poll rather than
        // assert once.
        let mut stored: Option<String> = None;
        for _ in 0..50 {
            stored = conn.get(&key).await.unwrap();
            if stored.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            stored.is_some(),
            "coalesce key must be set while a run is in flight"
        );

        run_lifecycle::finalize_run(
            &ctx,
            running_id,
            run_lifecycle::RunOutcome {
                status: run_lifecycle::RunStatus::Success,
                row_count: Some(1),
                error: None,
                query_result_id: None,
                data_age_secs: None,
                condition_met: None,
            },
        )
        .await
        .unwrap();

        let stored_after: Option<String> = conn.get(&key).await.unwrap();
        assert!(
            stored_after.is_none(),
            "GETDEL must clear the coalesce key on finalize"
        );

        let followups = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM runs WHERE entity_type='query' AND entity_id=$1 AND status='pending'",
                [q.id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(followups.len(), 1, "exactly one collapsed follow-up run");
    }

    /// The coalesce-key write happens after commit: an unreachable Redis must not roll back or
    /// block the claim; `next_run_at` still advances and locks are released.
    #[tokio::test]
    async fn claim_and_plan_commits_before_writing_the_coalesce_key() {
        let Some((db, _redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-coalesce-redisdown-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_published_query(&db, org.id, ds.id).await;
        // A prior in-flight run occupies uq_runs_inflight so the coalesce branch is taken.
        seed_query_run(&db, &q, "running", None).await;

        let due_at = Utc::now() - chrono::Duration::seconds(5);
        let sched = seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "coalesce",
            due_at,
            0,
        )
        .await;

        // Built lazily against a port nothing listens on so the post-commit coalesce write fails
        // fast.
        let bad_client = redis::Client::open("redis://127.0.0.1:1/").unwrap();
        let bad_redis_config = redis::aio::ConnectionManagerConfig::new()
            .set_number_of_retries(0)
            .set_connection_timeout(Some(Duration::from_millis(200)))
            .set_response_timeout(Some(Duration::from_millis(200)));
        let bad_redis =
            redis::aio::ConnectionManager::new_lazy_with_config(bad_client, bad_redis_config)
                .expect("lazy construction never touches the network");

        let ctx = test_ctx(db.clone(), bad_redis);
        let result = claim_and_plan(&ctx, Utc::now()).await;
        assert!(
            result.is_ok(),
            "an unreachable Redis coalesce write must not fail claim_and_plan: {:?}",
            result.err()
        );

        let reloaded = schedule::Entity::find_by_id(sched.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let advanced = reloaded
            .next_run_at
            .map(|t| t.with_timezone(&Utc))
            .expect("schedule must keep a next_run_at");
        assert!(
            advanced > due_at,
            "the transaction must commit (next_run_at advances past {due_at}) even though the \
             post-commit coalesce write to Redis failed; got {advanced}"
        );
    }

    #[tokio::test]
    async fn per_ds_cap_parks_then_releases_once_a_slot_frees() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-dscap-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        set_max_concurrent_runs(&db, ds.id, 1).await;
        let q_running = seed_query(&db, org.id, ds.id).await;
        let q_parked = seed_query(&db, org.id, ds.id).await;

        let running_id = seed_query_run(&db, &q_running, "running", None).await;
        let parked_id = seed_query_run(&db, &q_parked, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1 WHERE id = $2",
            [uuid::Uuid::new_v4().to_string().into(), parked_id.into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let parked_run = run::Entity::find_by_id(parked_id)
            .one(&ctx.db)
            .await
            .unwrap()
            .unwrap();

        let enqueued = try_enqueue_run(&ctx, &parked_run).await.unwrap();
        assert!(!enqueued, "at cap: must stay parked");

        run_lifecycle::finalize_run(
            &ctx,
            running_id,
            run_lifecycle::RunOutcome {
                status: run_lifecycle::RunStatus::Success,
                row_count: Some(1),
                error: None,
                query_result_id: None,
                data_age_secs: None,
                condition_met: None,
            },
        )
        .await
        .unwrap();

        let enqueued_after = try_enqueue_run(&ctx, &parked_run).await.unwrap();
        assert!(enqueued_after, "slot freed: must enqueue now");
    }

    /// The cap-count query must exclude the row being considered for enqueue, or a sweeper retry on
    /// an already-enqueued `pending` row would refuse to re-admit itself and wedge the data source.
    #[tokio::test]
    async fn a_pending_enqueued_run_does_not_count_against_its_own_admission() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-selfexclude-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        set_max_concurrent_runs(&db, ds.id, 1).await;
        let q = seed_published_query(&db, org.id, ds.id).await;

        let run_id = seed_query_run(&db, &q, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1, enqueued_at = now() WHERE id = $2",
            [uuid::Uuid::new_v4().to_string().into(), run_id.into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let run = run::Entity::find_by_id(run_id)
            .one(&ctx.db)
            .await
            .unwrap()
            .unwrap();

        let enqueued = try_enqueue_run(&ctx, &run).await.unwrap();
        assert!(
            enqueued,
            "a run must never count against the cap it is itself being admitted through"
        );
    }

    /// An enqueued `pending` run (`enqueued_at` set) counts toward the per-data-source cap on its
    /// own, not just `running` ones, so a burst can't overshoot.
    #[tokio::test]
    async fn an_enqueued_but_unclaimed_pending_run_counts_toward_the_cap_on_its_own() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-dscap-enqueued-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        set_max_concurrent_runs(&db, ds.id, 1).await;
        let q_enqueued = seed_query(&db, org.id, ds.id).await;
        let q_second = seed_query(&db, org.id, ds.id).await;

        let enqueued_id = seed_query_run(&db, &q_enqueued, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET enqueued_at = now() WHERE id = $1",
            [enqueued_id.into()],
        )
        .await;

        let second_id = seed_query_run(&db, &q_second, "pending", None).await;
        exec_sql(
            &db,
            "UPDATE runs SET job_id = $1 WHERE id = $2",
            [uuid::Uuid::new_v4().to_string().into(), second_id.into()],
        )
        .await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let second_run = run::Entity::find_by_id(second_id)
            .one(&ctx.db)
            .await
            .unwrap()
            .unwrap();

        let enqueued = try_enqueue_run(&ctx, &second_run).await.unwrap();
        assert!(
            !enqueued,
            "an already-enqueued pending run alone must saturate the cap — no running row needed"
        );
    }

    #[tokio::test]
    async fn auto_pause_triggers_at_threshold_and_drops_from_the_deadline_predicate() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        std::env::set_var("AUTO_PAUSE_THRESHOLD", "10");
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-autopause-{suffix}")).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let sched = seed_schedule(
            &db,
            org.id,
            "query",
            q.id,
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now() + chrono::Duration::seconds(300),
            9, // one failure away from the threshold
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
        run_lifecycle::finalize_run(
            &ctx,
            run_id,
            run_lifecycle::RunOutcome::terminal(
                run_lifecycle::RunStatus::Failed,
                Some("boom".to_string()),
            ),
        )
        .await
        .unwrap();

        let reloaded = schedule::Entity::find_by_id(sched.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.consecutive_failures, 10);
        assert!(reloaded.auto_paused_at.is_some());

        let still_in_predicate = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT 1 AS one FROM schedules WHERE id=$1 AND enabled AND auto_paused_at IS NULL",
                [sched.id.into()],
            ))
            .await
            .unwrap();
        assert!(
            still_in_predicate.is_empty(),
            "an auto-paused schedule must drop out of the deadline scan"
        );
    }
}

#[cfg(test)]
mod wake_stream_tests {
    //! `PubSubStream::next()` returns `Ready(None)` forever once its connection dies; `recv_wake`
    //! must treat that as dead or `run`'s `select!` would spin. Real Redis only (skips without
    //! `TEST_REDIS_URL`).
    use super::*;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    /// Both tests open their own `subscribe_wake` connection and diff `CLIENT LIST` snapshots, so
    /// they must not run concurrently.
    static SUBSCRIBE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Every connected client id whose last command was `SUBSCRIBE`; diffing two snapshots isolates
    /// one fresh pub/sub connection.
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
    async fn recv_wake_clears_a_closed_subscription() {
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

        // Kills only this connection by id, not `TYPE pubsub`, which would kill other tests'
        // subscriptions.
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

        // A cleared (None) wake must pend forever; an instantly-Ready future would spin run()'s
        // select! with zero delay.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), recv_wake(&mut wake))
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
            tokio::time::timeout(Duration::from_millis(200), recv_wake(&mut wake))
                .await
                .is_err(),
            "recv_wake with no subscription must stay pending, not resolve instantly"
        );
    }

    #[tokio::test]
    async fn subscribe_wake_opens_a_real_pubsub_connection() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let _guard = SUBSCRIBE_LOCK.lock().await;
        let client = redis::Client::open(redis_url.clone()).unwrap();
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();

        let before = subscribe_client_ids(&mut admin).await;
        let wake = subscribe_wake(&redis_url).await;
        assert!(
            wake.is_some(),
            "subscribe_wake must succeed against a live Redis"
        );

        let after = subscribe_client_ids(&mut admin).await;
        assert!(
            after.iter().any(|id| !before.contains(id)),
            "a new cmd=subscribe connection must appear in CLIENT LIST"
        );
    }
}
