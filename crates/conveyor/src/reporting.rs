//! Scheduler-internal maintenance over conveyor's own state: age off old terminal tasks and roll
//! up hourly stats. Not task types — just plain functions the leader-held loop in `scheduler.rs`
//! calls on its own timer, the same way it already calls `sweeper`.

use redis::aio::ConnectionManager;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

use crate::config::{prune_batch_limit, prune_max_per_cycle};

/// Delete terminal tasks whose age exceeds the retention window, keeping the audit table usable for
/// the hourly rollup without letting it grow unbounded. Ages off `COALESCE(finished_at,
/// created_at)` because a scheduler-skipped overlap row (`scheduler::plan_tick`) is terminal
/// (`status='skipped'`) yet never sets `finished_at` — keying on `finished_at` alone would leak
/// those forever.
///
/// Deletes in [`prune_batch_limit`]-sized chunks up to [`prune_max_per_cycle`] total, so one call
/// (run inline in the leader loop) can't hold the table's lock for as long as an unbounded backlog
/// would take — the residual is picked up on the next hourly cycle. `ORDER BY id` in the subselect
/// is required so successive chunks make forward progress instead of re-scanning the same head of
/// the table.
pub async fn prune_old_tasks(db: &DatabaseConnection, retention_days: i64) -> anyhow::Result<u64> {
    let batch_limit = prune_batch_limit();
    let max_per_cycle = prune_max_per_cycle();
    let mut total = 0u64;
    loop {
        let res = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "DELETE FROM conveyor_tasks WHERE id IN ( \
                   SELECT id FROM conveyor_tasks \
                    WHERE status IN ('success','failed','timed_out','cancelled','skipped') \
                      AND COALESCE(finished_at, created_at) < now() - make_interval(days => $1::int) \
                    ORDER BY id LIMIT $2)",
                [retention_days.into(), batch_limit.into()],
            ))
            .await?;
        let n = res.rows_affected();
        total += n;
        if n < batch_limit.cast_unsigned() || total >= max_per_cycle.cast_unsigned() {
            break;
        }
    }
    Ok(total)
}

/// Upsert per-(hour, `task_type`) aggregates for the last three fully-completed hours. A three-hour
/// self-heal window (not just the previous hour) means a missed cycle — process restart, leader
/// handoff — is repaired on the next run; the upsert is idempotent so re-doing a bucket is free.
/// The current, still-in-progress hour is excluded so a bucket is only ever written once it can no
/// longer change (except `queue_depth_sample`, a rollup-time gauge — see below).
pub async fn rollup_recent_hours(
    db: &DatabaseConnection,
    valkey: &ConnectionManager,
) -> anyhow::Result<()> {
    db.execute_raw(Statement::from_string(
        DbBackend::Postgres,
        "INSERT INTO task_stats_hourly \
           (bucket, task_type, succeeded, failed, timed_out, cancelled, skipped, retries, \
            p50_ms, p95_ms, p99_ms, max_ms, avg_queue_latency_ms, queue_depth_sample) \
         SELECT date_trunc('hour', COALESCE(finished_at, created_at)) AS bucket, \
                task_type, \
                count(*) FILTER (WHERE status='success'), \
                count(*) FILTER (WHERE status='failed'), \
                count(*) FILTER (WHERE status='timed_out'), \
                count(*) FILTER (WHERE status='cancelled'), \
                count(*) FILTER (WHERE status='skipped'), \
                count(*) FILTER (WHERE attempt > 1), \
                COALESCE(percentile_cont(0.50) WITHIN GROUP (ORDER BY duration_ms), 0)::bigint, \
                COALESCE(percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms), 0)::bigint, \
                COALESCE(percentile_cont(0.99) WITHIN GROUP (ORDER BY duration_ms), 0)::bigint, \
                COALESCE(max(duration_ms), 0), \
                COALESCE(avg(EXTRACT(EPOCH FROM (started_at - enqueued_at)) * 1000) \
                          FILTER (WHERE started_at IS NOT NULL AND enqueued_at IS NOT NULL), 0)::bigint, \
                0 \
         FROM conveyor_tasks \
         WHERE status IN ('success','failed','timed_out','cancelled','skipped') \
           AND COALESCE(finished_at, created_at) >= date_trunc('hour', now()) - interval '3 hours' \
           AND COALESCE(finished_at, created_at) <  date_trunc('hour', now()) \
         GROUP BY 1, 2 \
         ON CONFLICT (bucket, task_type) DO UPDATE SET \
            succeeded=EXCLUDED.succeeded, failed=EXCLUDED.failed, timed_out=EXCLUDED.timed_out, \
            cancelled=EXCLUDED.cancelled, skipped=EXCLUDED.skipped, retries=EXCLUDED.retries, \
            p50_ms=EXCLUDED.p50_ms, p95_ms=EXCLUDED.p95_ms, p99_ms=EXCLUDED.p99_ms, \
            max_ms=EXCLUDED.max_ms, avg_queue_latency_ms=EXCLUDED.avg_queue_latency_ms"
            .to_string(),
    ))
    .await?;

    // queue_depth_sample is a live gauge, unknowable from historical rows: sample current depth and
    // stamp it onto the current, still-in-progress bucket, upserting since a task type backing up
    // with nothing terminal yet has no row from the rollup above — and never lowering an existing
    // sample within the bucket.
    for (task_type, depth) in crate::queue::all_queue_depths(valkey)
        .await
        .unwrap_or_default()
    {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO task_stats_hourly (bucket, task_type, queue_depth_sample) \
             VALUES (date_trunc('hour', now()), $1, $2) \
             ON CONFLICT (bucket, task_type) DO UPDATE SET \
                queue_depth_sample = GREATEST(task_stats_hourly.queue_depth_sample, EXCLUDED.queue_depth_sample)",
            [task_type.into(), depth.into()],
        ))
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skip cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use chrono::{Duration as ChronoDuration, Utc};
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use super::*;
    use crate::test_support::*;

    async fn stats_row(
        db: &DatabaseConnection,
        bucket: chrono::DateTime<Utc>,
        task_type: &str,
    ) -> Option<(i32, i32, i32, i64, i64, i64)> {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT succeeded, failed, skipped, p50_ms, p95_ms, max_ms \
                 FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
                [bucket.into(), task_type.into()],
            ))
            .await
            .unwrap()?;
        Some((
            row.try_get("", "succeeded").unwrap(),
            row.try_get("", "failed").unwrap(),
            row.try_get("", "skipped").unwrap(),
            row.try_get("", "p50_ms").unwrap(),
            row.try_get("", "p95_ms").unwrap(),
            row.try_get("", "max_ms").unwrap(),
        ))
    }

    #[tokio::test]
    async fn rollup_aggregates_and_percentiles() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("rollup_agg_{}", uuid::Uuid::new_v4());
        let bucket = hour_bucket(Utc::now() - ChronoDuration::hours(2));
        let finished_at = bucket + ChronoDuration::minutes(10);

        for (status, duration_ms) in [
            ("success", Some(10)),
            ("success", Some(20)),
            ("success", Some(30)),
            ("success", Some(40)),
            ("failed", Some(50)),
        ] {
            insert_terminal_task(&db, &task_type, status, duration_ms, Some(finished_at)).await;
        }
        // Skipped row: only created_at set, no finished_at, no duration.
        insert_task_with_created_at(&db, &task_type, "skipped", finished_at).await;

        rollup_recent_hours(&db, &valkey).await.unwrap();

        let (succeeded, failed, skipped, p50, _p95, max) = stats_row(&db, bucket, &task_type)
            .await
            .expect("rollup must have written a row for this bucket");
        assert_eq!(succeeded, 4);
        assert_eq!(failed, 1);
        assert_eq!(
            skipped, 1,
            "the skipped row must land in the bucket via created_at"
        );
        assert_eq!(max, 50);
        assert!(p50 > 0 && p50 <= 50);
    }

    #[tokio::test]
    async fn rollup_is_idempotent_and_self_heals_within_the_window() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("rollup_idem_{}", uuid::Uuid::new_v4());
        let now = Utc::now();
        let bucket = hour_bucket(now - ChronoDuration::hours(2));
        let finished_at = bucket + ChronoDuration::minutes(5);

        insert_terminal_task(&db, &task_type, "success", Some(10), Some(finished_at)).await;

        rollup_recent_hours(&db, &valkey).await.unwrap();
        let count_query = || {
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT count(*) AS c FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
                [bucket.into(), task_type.clone().into()],
            )
        };
        let row_count: i64 = db
            .query_one_raw(count_query())
            .await
            .unwrap()
            .unwrap()
            .try_get("", "c")
            .unwrap();
        assert_eq!(row_count, 1);

        rollup_recent_hours(&db, &valkey).await.unwrap();
        let row_count_again: i64 = db
            .query_one_raw(count_query())
            .await
            .unwrap()
            .unwrap()
            .try_get("", "c")
            .unwrap();
        assert_eq!(row_count_again, 1, "re-running must not duplicate the row");

        insert_terminal_task(&db, &task_type, "success", Some(20), Some(finished_at)).await;
        rollup_recent_hours(&db, &valkey).await.unwrap();
        let (succeeded, ..) = stats_row(&db, bucket, &task_type).await.unwrap();
        assert_eq!(
            succeeded, 2,
            "a bucket within the self-heal window must be re-aggregated"
        );
    }

    #[tokio::test]
    async fn current_hour_is_excluded_from_rollup() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("rollup_current_{}", uuid::Uuid::new_v4());
        let now = Utc::now();
        insert_terminal_task(&db, &task_type, "success", Some(5), Some(now)).await;

        rollup_recent_hours(&db, &valkey).await.unwrap();

        let current_bucket = hour_bucket(now);
        assert!(
            stats_row(&db, current_bucket, &task_type).await.is_none(),
            "the in-progress hour must not be rolled up"
        );
    }

    /// The rollup's own `INSERT ... SELECT` only writes rows for task types with terminal tasks in
    /// the window — a task type whose queue is backing up but has completed nothing produces no
    /// such row. `queue_depth_sample`'s upsert must still record it, against the current hour.
    #[tokio::test]
    async fn queue_depth_is_recorded_for_a_task_type_with_no_completed_tasks() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("rollup_depth_{}", uuid::Uuid::new_v4());
        let key = crate::queue::queue_key(&task_type);
        let mut conn = valkey.clone();
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "a")
            .await
            .unwrap();
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "b")
            .await
            .unwrap();
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "c")
            .await
            .unwrap();

        rollup_recent_hours(&db, &valkey).await.unwrap();

        let current_bucket = hour_bucket(Utc::now());
        let sample: i32 = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT queue_depth_sample FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
                [current_bucket.into(), task_type.clone().into()],
            ))
            .await
            .unwrap()
            .expect("a row must exist for the current hour even with no completed tasks")
            .try_get("", "queue_depth_sample")
            .unwrap();
        assert!(
            sample >= 3,
            "queue_depth_sample must reflect the seeded depth, got {sample}"
        );

        let _: i64 = redis::AsyncCommands::del(&mut conn, &key).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
            [current_bucket.into(), task_type.into()],
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_later_sample_never_lowers_the_recorded_peak() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("rollup_depth_peak_{}", uuid::Uuid::new_v4());
        let key = crate::queue::queue_key(&task_type);
        let mut conn = valkey.clone();
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "a")
            .await
            .unwrap();
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "b")
            .await
            .unwrap();
        rollup_recent_hours(&db, &valkey).await.unwrap();

        let current_bucket = hour_bucket(Utc::now());
        let sample: i32 = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT queue_depth_sample FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
                [current_bucket.into(), task_type.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "queue_depth_sample")
            .unwrap();
        assert!(sample >= 2);

        // Drain the queue and re-run — GREATEST must keep the higher sample, not drop to 0.
        let _: Option<String> = redis::AsyncCommands::rpop(&mut conn, &key, None)
            .await
            .unwrap();
        let _: Option<String> = redis::AsyncCommands::rpop(&mut conn, &key, None)
            .await
            .unwrap();
        rollup_recent_hours(&db, &valkey).await.unwrap();
        let sample_after: i32 = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT queue_depth_sample FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
                [current_bucket.into(), task_type.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "queue_depth_sample")
            .unwrap();
        assert!(
            sample_after >= sample,
            "GREATEST must never lower an existing sample"
        );

        let _: i64 = redis::AsyncCommands::del(&mut conn, &key).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM task_stats_hourly WHERE bucket = $1 AND task_type = $2",
            [current_bucket.into(), task_type.into()],
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prune_deletes_only_aged_terminal_rows() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("prune_{}", uuid::Uuid::new_v4());
        let old = Utc::now() - ChronoDuration::days(20);
        let recent = Utc::now() - ChronoDuration::hours(1);

        // (a) terminal success, aged by finished_at.
        let a = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                status: "success",
                ..Default::default()
            },
        )
        .await;
        set_finished_at(&db, a, old).await;

        // (b) skipped, aged, only created_at.
        let b = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                status: "skipped",
                created_at: Some(old),
                ..Default::default()
            },
        )
        .await;

        // (c) terminal, recent — must survive.
        let c = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                status: "success",
                ..Default::default()
            },
        )
        .await;
        set_finished_at(&db, c, recent).await;

        // (d) running, aged — must survive (non-terminal).
        let d = insert_task_full(
            &db,
            InsertTask {
                task_type: &task_type,
                status: "running",
                created_at: Some(old),
                ..Default::default()
            },
        )
        .await;

        let deleted = prune_old_tasks(&db, 14).await.unwrap();
        assert_eq!(deleted, 2, "exactly a and b must be pruned");

        assert!(!task_row_exists(&db, a).await, "a must be deleted");
        assert!(!task_row_exists(&db, b).await, "b must be deleted");
        assert!(task_row_exists(&db, c).await, "c must be kept (recent)");
        assert!(
            task_row_exists(&db, d).await,
            "d must be kept (non-terminal)"
        );
    }

    /// `CONVEYOR_PRUNE_BATCH_LIMIT`/`CONVEYOR_PRUNE_MAX_PER_CYCLE` are process-global env vars —
    /// held for the whole test so a concurrently running test reading the true default doesn't see
    /// this override instead.
    static PRUNE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The shared `conveyor_tasks` test table carries hundreds of leftover rows this prune's
    /// predicate will also match — scope assertions to this test's own seeded ids, never to a total
    /// table count.
    #[tokio::test]
    async fn prune_old_tasks_deletes_in_bounded_chunks() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _env_guard = PRUNE_ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_PRUNE_BATCH_LIMIT", "2");
        std::env::remove_var("CONVEYOR_PRUNE_MAX_PER_CYCLE");

        let task_type = format!("prune_chunked_{}", uuid::Uuid::new_v4());
        let old = Utc::now() - ChronoDuration::days(20);
        let mut ids = Vec::new();
        for _ in 0..5 {
            let id = insert_task_full(
                &db,
                InsertTask {
                    task_type: &task_type,
                    status: "success",
                    ..Default::default()
                },
            )
            .await;
            set_finished_at(&db, id, old).await;
            ids.push(id);
        }

        let deleted = prune_old_tasks(&db, 14).await.unwrap();
        std::env::remove_var("CONVEYOR_PRUNE_BATCH_LIMIT");

        assert!(
            deleted >= 5,
            "the chunked loop must iterate until every aged row (at least our 5) is gone, got {deleted}"
        );
        for id in ids {
            assert!(
                !task_row_exists(&db, id).await,
                "task {id} must have been pruned by the chunked loop"
            );
        }
    }

    #[tokio::test]
    async fn prune_old_tasks_stops_at_the_per_cycle_ceiling() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _env_guard = PRUNE_ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_PRUNE_BATCH_LIMIT", "1");
        std::env::set_var("CONVEYOR_PRUNE_MAX_PER_CYCLE", "2");

        let task_type = format!("prune_ceiling_{}", uuid::Uuid::new_v4());
        let old = Utc::now() - ChronoDuration::days(20);
        let mut ids = Vec::new();
        for _ in 0..5 {
            let id = insert_task_full(
                &db,
                InsertTask {
                    task_type: &task_type,
                    status: "success",
                    ..Default::default()
                },
            )
            .await;
            set_finished_at(&db, id, old).await;
            ids.push(id);
        }

        let deleted = prune_old_tasks(&db, 14).await.unwrap();
        std::env::remove_var("CONVEYOR_PRUNE_BATCH_LIMIT");
        std::env::remove_var("CONVEYOR_PRUNE_MAX_PER_CYCLE");

        // The ceiling is checked after a full batch completes, so the loop can overshoot by at
        // most one batch (here, batch_limit=1) past max_per_cycle=2.
        assert!(
            deleted <= 3,
            "the per-cycle ceiling must bound total deletions to at most one batch past it, got {deleted}"
        );
        let mut survivors = 0;
        for id in &ids {
            if task_row_exists(&db, *id).await {
                survivors += 1;
            }
        }
        assert!(
            survivors > 0,
            "the ceiling must leave some of our seeded rows for the next cycle"
        );
    }

    // --- local helpers -----------------------------------------------------------------------

    use chrono::Timelike;

    fn hour_bucket(t: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
        t.date_naive()
            .and_hms_opt(t.time().hour(), 0, 0)
            .unwrap()
            .and_utc()
    }

    async fn insert_terminal_task(
        db: &DatabaseConnection,
        task_type: &str,
        status: &str,
        duration_ms: Option<i64>,
        finished_at: Option<chrono::DateTime<Utc>>,
    ) -> i64 {
        let id = insert_task_full(
            db,
            InsertTask {
                task_type,
                status,
                ..Default::default()
            },
        )
        .await;
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET finished_at = $1, duration_ms = $2, \
             started_at = $1, enqueued_at = $1 WHERE id = $3",
            [finished_at.into(), duration_ms.into(), id.into()],
        ))
        .await
        .unwrap();
        id
    }

    async fn insert_task_with_created_at(
        db: &DatabaseConnection,
        task_type: &str,
        status: &str,
        created_at: chrono::DateTime<Utc>,
    ) -> i64 {
        insert_task_full(
            db,
            InsertTask {
                task_type,
                status,
                created_at: Some(created_at),
                ..Default::default()
            },
        )
        .await
    }

    async fn set_finished_at(db: &DatabaseConnection, id: i64, finished_at: chrono::DateTime<Utc>) {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET finished_at = $1 WHERE id = $2",
            [finished_at.into(), id.into()],
        ))
        .await
        .unwrap();
    }

    async fn task_row_exists(db: &DatabaseConnection, id: i64) -> bool {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS one FROM conveyor_tasks WHERE id = $1",
            [id.into()],
        ))
        .await
        .unwrap()
        .is_some()
    }
}
