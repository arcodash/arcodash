//! Producer-side routing helper for the `execute_query` job, like `monitor_eval.rs`.
//!
//! Unlike `evaluate_monitor`, `run_id = Some` executions (scheduled ticks, manual runs) also route
//! through conveyor when the flag is on, since they are the long-tier traffic the tiers protect.
//! Conveyor is dispatch only there: the `runs` row's CAS/heartbeat/finalize stays the durability
//! source of truth.

use serde::{Deserialize, Serialize};

use arcodash_conveyor::{AlreadyInflight, Conveyor, ConveyorBuilder, EnqueueRequest, Enqueued};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

use crate::queue::{JobMessage, Queue, QueueName};

/// Build a produce-only `Conveyor` handle (`enqueue()`/`status()` only), so a producer like
/// `apps/backend` can depend on `arcodash-core` alone.
#[must_use]
pub fn build_producer_conveyor(
    db: sea_orm::DatabaseConnection,
    valkey: redis::aio::ConnectionManager,
    redis_url: String,
) -> std::sync::Arc<Conveyor> {
    ConveyorBuilder::new(db, valkey, redis_url).build()
}

/// `c.enqueue(req)`, with conveyor's [`AlreadyInflight`] (a task for the same entity is already
/// pending|running) mapped to `Ok(None)`, which every producer treats as a no-op.
pub(crate) async fn enqueue_unless_inflight(
    c: &Conveyor,
    req: EnqueueRequest,
) -> anyhow::Result<Option<Enqueued>> {
    match c.enqueue(req).await {
        Ok(res) => Ok(Some(res)),
        Err(e) if e.downcast_ref::<AlreadyInflight>().is_some() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Mirrors the worker's `DEFAULT_DS_CONCURRENCY`, which is unreachable from this crate.
const DEFAULT_DS_CONCURRENCY: i64 = 4;

/// The `execute_query` handler payload, routed through conveyor or the legacy queue (same shape as
/// `JobMessage::ExecuteQuery`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteQueryPayload {
    pub job_id: String,
    pub org_id: i64,
    pub data_source_id: i64,
    pub query_text: String,
    pub parameters: serde_json::Value,
    pub user_id: i64,
    pub query_id: Option<i64>,
    pub scheduled: bool,
    #[serde(default)]
    pub run_id: Option<i64>,
    #[serde(default)]
    pub repoint_published: bool,
    /// See `JobMessage::ExecuteQuery.max_rows`.
    #[serde(default)]
    pub max_rows: Option<u32>,
}

/// Route `execute_query` enqueues through the conveyor SDK instead of the legacy `queries`/
/// `scheduled_queries` queues. Off/unset keeps the legacy queue.
#[must_use]
pub fn conveyor_execute_query_enabled() -> bool {
    crate::config::conveyor_flag("CONVEYOR_EXECUTE_QUERY_ENABLED")
}

/// `data_sources.max_concurrent_runs`, or [`DEFAULT_DS_CONCURRENCY`] when unset/the row is gone
/// (same COALESCE-default cap query as the worker's `ds_cap`).
async fn ds_concurrency_cap(db: &impl ConnectionTrait, data_source_id: i64) -> anyhow::Result<i64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT COALESCE(max_concurrent_runs, $2) AS cap FROM data_sources WHERE id = $1",
            [data_source_id.into(), DEFAULT_DS_CONCURRENCY.into()],
        ))
        .await?;
    match row {
        Some(r) => Ok(r.try_get("", "cap")?),
        None => Ok(DEFAULT_DS_CONCURRENCY),
    }
}

/// Enqueue one `execute_query`.
///
/// Routes through conveyor iff `conveyor` is `Some` AND [`conveyor_execute_query_enabled`];
/// otherwise the legacy queue selected by `job.scheduled`. Does NOT write the initial
/// `job:<job_id>` Redis status placeholder; that stays the caller's job.
///
/// Tier: `job.scheduled` selects `execute_query` (fast) vs `execute_query_scheduled` (long).
///
/// `entity_id`: `query_id` when `scheduled`, mirroring `uq_runs_inflight`'s one-run-in-flight
/// invariant; `None` for interactive, which has no in-flight dedup (a saved query can run twice
/// concurrently) and would otherwise serialize against scheduled runs.
///
/// `resource_key/resource_cap`: the data source id + its `max_concurrent_runs`, generalizing the
/// worker's per-DS gate. Redundant for scheduled runs (the `runs`-table gate is never more
/// permissive), but the only protection for interactive runs.
pub async fn enqueue_execute_query(
    db: &impl ConnectionTrait,
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    job: ExecuteQueryPayload,
) -> anyhow::Result<bool> {
    if let Some(c) = conveyor.filter(|_| conveyor_execute_query_enabled()) {
        let task_type = if job.scheduled {
            "execute_query_scheduled"
        } else {
            "execute_query"
        };
        let entity_id = if job.scheduled { job.query_id } else { None };
        let cap = ds_concurrency_cap(db, job.data_source_id).await?;
        let resource_key = job.data_source_id.to_string();
        let payload = serde_json::to_value(&job)?;

        let enqueued = enqueue_unless_inflight(
            c,
            EnqueueRequest {
                task_type: task_type.into(),
                payload,
                tenant_id: Some(job.org_id),
                entity_id,
                resource_key: Some(resource_key),
                resource_cap: Some(cap),
            },
        )
        .await
        .inspect_err(|_| crate::metrics::record_execute_enqueue("conveyor", "error"))?;
        // A run for this query is already pending|running (should not happen given the caller's
        // runs-table CAS): a no-op, but warn in case the CAS let two enqueues through.
        let Some(res) = enqueued else {
            tracing::warn!(job_id = %job.job_id, query_id = ?job.query_id, "enqueue_execute_query: conveyor task already in flight for this query, skipping enqueue");
            crate::metrics::record_execute_enqueue("conveyor", "already_inflight");
            return Ok(false);
        };
        crate::metrics::record_execute_enqueue(
            "conveyor",
            if res.queued { "queued" } else { "parked" },
        );
        return Ok(res.queued);
    }

    let target_queue = if job.scheduled {
        QueueName::ScheduledQueries
    } else {
        QueueName::Queries
    };
    let result = queue
        .enqueue(
            target_queue,
            JobMessage::ExecuteQuery {
                job_id: job.job_id,
                org_id: job.org_id,
                data_source_id: job.data_source_id,
                query_text: job.query_text,
                parameters: job.parameters,
                user_id: job.user_id,
                query_id: job.query_id,
                scheduled: job.scheduled,
                run_id: job.run_id,
                repoint_published: job.repoint_published,
                max_rows: job.max_rows,
            },
        )
        .await;
    let outcome = if result.is_ok() { "queued" } else { "error" };
    crate::metrics::record_execute_enqueue("legacy", outcome);
    result.map(|()| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingQueue;

    /// `CONVEYOR_EXECUTE_QUERY_ENABLED` is process-global, so tests that set it must not run
    /// concurrently; held across `.await`, hence `tokio::sync::Mutex`.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn sample_job(scheduled: bool, run_id: Option<i64>) -> ExecuteQueryPayload {
        ExecuteQueryPayload {
            job_id: "job-1".to_string(),
            org_id: 1,
            data_source_id: 2,
            query_text: "SELECT 1".to_string(),
            parameters: serde_json::json!({}),
            user_id: 3,
            query_id: Some(4),
            scheduled,
            run_id,
            repoint_published: false,
            max_rows: None,
        }
    }

    #[tokio::test]
    async fn legacy_branch_routes_interactive_to_queries_queue() {
        let queue = RecordingQueue::default();
        let db = sea_orm::DatabaseConnection::default();
        let queued = enqueue_execute_query(&db, &queue, None, sample_job(false, None))
            .await
            .unwrap();
        assert!(queued, "the legacy queue push always actually queues");

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, QueueName::Queries);
        match &sent[0].1 {
            JobMessage::ExecuteQuery {
                scheduled, run_id, ..
            } => {
                assert!(!scheduled);
                assert_eq!(*run_id, None);
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_branch_routes_scheduled_to_scheduled_queries_queue() {
        let queue = RecordingQueue::default();
        let db = sea_orm::DatabaseConnection::default();
        let queued = enqueue_execute_query(&db, &queue, None, sample_job(true, Some(99)))
            .await
            .unwrap();
        assert!(queued, "the legacy queue push always actually queues");

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, QueueName::ScheduledQueries);
        match &sent[0].1 {
            JobMessage::ExecuteQuery {
                scheduled, run_id, ..
            } => {
                assert!(*scheduled);
                assert_eq!(*run_id, Some(99));
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    /// A conveyor task row is retained 14 days, so a payload written by an old binary and read by a
    /// newer one is a real, days-long window.
    #[test]
    fn execute_query_payload_deserializes_without_run_id_or_repoint_published() {
        let json = serde_json::json!({
            "job_id": "job-1",
            "org_id": 1,
            "data_source_id": 2,
            "query_text": "SELECT 1",
            "parameters": {},
            "user_id": 3,
            "query_id": 4,
            "scheduled": false,
        });
        let parsed: ExecuteQueryPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.run_id, None);
        assert!(!parsed.repoint_published);
    }

    #[test]
    fn payload_round_trips_through_json() {
        let payload = sample_job(true, Some(7));
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ExecuteQueryPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.job_id, payload.job_id);
        assert_eq!(parsed.run_id, payload.run_id);
        assert_eq!(parsed.scheduled, payload.scheduled);
    }

    // One test, not two: a process-global env var can't safely split across parallel tests.
    #[test]
    fn enabled_reflects_the_env_var() {
        let _guard = ENV_LOCK.blocking_lock();
        std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");
        assert!(!conveyor_execute_query_enabled());
        for (raw, expected) in [("false", false), ("0", false), ("true", true), ("1", true)] {
            std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", raw);
            assert_eq!(conveyor_execute_query_enabled(), expected, "{raw:?}");
        }
        std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");
    }

    struct ConveyorFixture {
        db: sea_orm::DatabaseConnection,
        conveyor: std::sync::Arc<Conveyor>,
        queue: crate::queue::ValkeyQueue,
        ds_id: i64,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    /// Needs Postgres + Redis (`None` skips). Holds [`ENV_LOCK`] with
    /// `CONVEYOR_EXECUTE_QUERY_ENABLED=true` and seeds a fresh org + data source.
    async fn conveyor_fixture() -> Option<ConveyorFixture> {
        let (db, valkey, redis_url) = crate::test_support::test_db_and_valkey().await?;
        let guard = ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
        let conveyor = ConveyorBuilder::new(db.clone(), valkey.clone(), redis_url).build();
        let queue = crate::queue::ValkeyQueue::new(valkey);

        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let mut org_active = crate::test_support::new_organization();
        org_active.slug = Set(format!("test-org-{}", uuid::Uuid::new_v4()));
        let org = org_active.insert(&db).await.unwrap();
        let ds_id = crate::test_support::new_data_source(org.id)
            .insert(&db)
            .await
            .unwrap()
            .id;
        Some(ConveyorFixture {
            db,
            conveyor,
            queue,
            ds_id,
            _guard: guard,
        })
    }

    /// A conveyor-branch job: scheduled, on `ds_id`, for `query_id`.
    fn conveyor_job(job_id: &str, ds_id: i64, query_id: i64, run_id: i64) -> ExecuteQueryPayload {
        ExecuteQueryPayload {
            job_id: job_id.to_string(),
            data_source_id: ds_id,
            user_id: 1,
            query_id: Some(query_id),
            ..sample_job(true, Some(run_id))
        }
    }

    /// A random unique `query_id` per run: a hardcoded one would collide with a `pending` row left
    /// by an interrupted run and trip `uq_conveyor_tasks_inflight`.
    fn unique_query_id() -> i64 {
        (uuid::Uuid::new_v4().as_u128() % (i64::MAX as u128 / 2)) as i64
    }

    /// Needs Postgres + Redis: the conveyor branch enqueues a real task with the right task
    /// type/entity_id/resource_key, and the legacy `job:<job_id>` status key stays the caller's
    /// responsibility.
    #[tokio::test]
    async fn conveyor_branch_enqueues_on_the_scheduled_task_type_with_ds_resource_key() {
        let Some(ConveyorFixture {
            db,
            conveyor,
            queue,
            ds_id,
            _guard,
        }) = conveyor_fixture().await
        else {
            return;
        };
        let query_id = unique_query_id();
        let job = conveyor_job("job-conveyor-1", ds_id, query_id, 777);

        let queued = enqueue_execute_query(&db, &queue, Some(&conveyor), job)
            .await
            .unwrap();
        assert!(
            queued,
            "a task under its resource cap must report queued = true"
        );

        std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT task_type, entity_id, resource_key, resource_cap FROM conveyor_tasks \
                 WHERE task_type = 'execute_query_scheduled' AND entity_id = $1 \
                 ORDER BY id DESC LIMIT 1",
                [query_id.into()],
            ))
            .await
            .unwrap()
            .expect("a conveyor_tasks row must exist for the scheduled task type");
        let task_type: String = row.try_get("", "task_type").unwrap();
        let entity_id: Option<i64> = row.try_get("", "entity_id").unwrap();
        let resource_key: Option<String> = row.try_get("", "resource_key").unwrap();
        assert_eq!(task_type, "execute_query_scheduled");
        assert_eq!(entity_id, Some(query_id));
        assert_eq!(resource_key, Some(ds_id.to_string()));
    }

    /// A task conveyor parks over its data source's resource cap must report `queued = false`, so
    /// `run_lifecycle::enqueue_run` doesn't stamp `runs.enqueued_at` on work that was never queued.
    #[tokio::test]
    async fn conveyor_branch_reports_false_when_parked_over_the_data_source_cap() {
        let Some(ConveyorFixture {
            db,
            conveyor,
            queue,
            ds_id,
            _guard,
        }) = conveyor_fixture().await
        else {
            return;
        };
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE data_sources SET max_concurrent_runs = 1 WHERE id = $1",
            [ds_id.into()],
        ))
        .await
        .unwrap();

        // Saturate the cap with one running conveyor task on this resource_key.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (task_type, resource_key, resource_cap, status, attempt, triggered_by, created_at) \
             VALUES ('execute_query_scheduled', $1, 1, 'running', 1, 'enqueue', now())",
            [ds_id.to_string().into()],
        ))
        .await
        .unwrap();

        let query_id = unique_query_id();
        let job = conveyor_job("job-conveyor-overcap-1", ds_id, query_id, 778);

        let queued = enqueue_execute_query(&db, &queue, Some(&conveyor), job)
            .await
            .unwrap();

        std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

        assert!(
            !queued,
            "a task parked over its data source's cap must report queued = false"
        );

        // A fresh query_id can't be inflight, so `queued = false` can only come from the over-cap
        // park; prove a row exists with `enqueued_at` unstamped.
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT enqueued_at FROM conveyor_tasks \
                 WHERE task_type = 'execute_query_scheduled' AND entity_id = $1 \
                 ORDER BY id DESC LIMIT 1",
                [query_id.into()],
            ))
            .await
            .unwrap()
            .expect("the over-cap task must still have been parked as a real conveyor_tasks row");
        let enqueued_at: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("", "enqueued_at").unwrap();
        assert_eq!(
            enqueued_at, None,
            "a parked-over-cap row must not be stamped enqueued_at"
        );
    }

    /// The `AlreadyInflight` branch: a conveyor task already `pending` for the same `(task_type,
    /// entity_id)` makes `enqueue_execute_query` return `Ok(false)` rather than an `Err`.
    #[tokio::test]
    async fn conveyor_branch_reports_false_on_already_inflight_entity() {
        let Some(ConveyorFixture {
            db,
            conveyor,
            queue,
            ds_id,
            _guard,
        }) = conveyor_fixture().await
        else {
            return;
        };
        let query_id = unique_query_id();

        // Occupy the (task_type, entity_id) slot with a real 'pending' row for this query_id.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (task_type, entity_id, status, attempt, triggered_by, created_at) \
             VALUES ('execute_query_scheduled', $1, 'pending', 1, 'enqueue', now())",
            [query_id.into()],
        ))
        .await
        .unwrap();

        let job = conveyor_job("job-conveyor-already-inflight-1", ds_id, query_id, 779);

        let queued = enqueue_execute_query(&db, &queue, Some(&conveyor), job)
            .await
            .unwrap();

        std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

        assert!(
            !queued,
            "a second enqueue for an already-inflight entity must report queued = false, not error"
        );
    }
}
