//! Run history and manual "run now". Reads are gated by `schedule_service::authorize_entity` on the
//! run's `(entity_type, entity_id)`.

use std::sync::Arc;

use chrono::Utc;
use redis::aio::ConnectionManager;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, Statement,
};
use serde::Serialize;
use uuid::Uuid;

use arcodash_core::entities::{monitor, run, user};
use arcodash_core::events::record_event;
use arcodash_core::queue::{JobMessage, JobStatus, Queue, QueueName};

use crate::db::Db;
use crate::error::ApiError;
use crate::services::result_service::ResultFormat;
use crate::services::{query_service, result_service, schedule_service};

/// One notified destination for a monitor run; `[]` for a query run or a monitor run that sent
/// nothing. Also serves as the API DTO.
#[derive(Debug, Clone, Serialize, FromQueryResult)]
pub struct NotifiedDto {
    pub destination_name: String,
    pub status: String,
    /// Already-redacted send failure text (`notification_deliveries.error`); `None` for a
    /// `sent`/in-progress row.
    pub error: Option<String>,
}

/// One loaded run plus its derived `notified` list.
pub struct RunRow {
    pub run: run::Model,
    pub notified: Vec<NotifiedDto>,
}

/// Deliveries tied to `run_id`, joined to the destination name: `'Email (you)'` for the
/// null-destination sentinel, `'(deleted destination)'` if the destination row is gone.
async fn notified_for_run(
    db: &impl ConnectionTrait,
    run_id: i64,
) -> Result<Vec<NotifiedDto>, ApiError> {
    Ok(
        NotifiedDto::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT nd.status, nd.error, \
         CASE WHEN nd.destination_id IS NULL THEN 'Email (you)' \
              ELSE COALESCE(dst.name, '(deleted destination)') END AS destination_name \
         FROM notification_deliveries nd \
         LEFT JOIN notification_destinations dst ON dst.id = nd.destination_id \
         WHERE nd.run_id = $1 ORDER BY nd.id",
            [run_id.into()],
        ))
        .all(db)
        .await?,
    )
}

/// The last (at most 10, per the prune bound) runs for `(entity_type, entity_id)`, newest first.
/// Only monitor runs carry `notified`.
pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    entity_type: &str,
    entity_id: i64,
) -> Result<Vec<RunRow>, ApiError> {
    let runs = run::Entity::find()
        .filter(run::Column::OrgId.eq(org_id))
        .filter(run::Column::EntityType.eq(entity_type))
        .filter(run::Column::EntityId.eq(entity_id))
        .order_by_desc(run::Column::Id)
        .limit(10)
        .all(db)
        .await?;

    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        let notified = if entity_type == "monitor" {
            notified_for_run(db, r.id).await?
        } else {
            Vec::new()
        };
        out.push(RunRow { run: r, notified });
    }
    Ok(out)
}

/// The query name a CSV filename is slugged from (the run's query, or the monitor's underlying
/// query).
async fn query_name_for_run(
    db: &impl ConnectionTrait,
    org_id: i64,
    run: &run::Model,
) -> Result<String, ApiError> {
    match run.entity_type.as_str() {
        "query" => Ok(schedule_service::load_query_org(db, org_id, run.entity_id)
            .await?
            .name),
        "monitor" => {
            let m = monitor::Entity::find_by_id(run.entity_id)
                .one(db)
                .await?
                .filter(|m| m.org_id == org_id)
                .ok_or_else(|| ApiError::not_found("monitor not found"))?;
            Ok(schedule_service::load_query_org(db, org_id, m.query_id)
                .await?
                .name)
        }
        _ => Ok("query".to_string()),
    }
}

/// Lowercase, non-alphanumeric runs collapsed to `-`, trimmed; `fallback` if nothing remains.
pub(crate) fn slugify(name: &str, fallback: &str) -> String {
    let slug = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

/// The `query_results` row a run produced, as CSV bytes + a filename. 404s "result no longer
/// retained" when the run had no result or it was GC'd.
pub async fn run_result_csv(
    db: &impl ConnectionTrait,
    user: &user::Model,
    run: &run::Model,
) -> Result<(Vec<u8>, String), ApiError> {
    let qr_id = run
        .query_result_id
        .ok_or_else(|| ApiError::not_found("result no longer retained"))?;
    // Data-source-access-gated load: refuses a caller who has entity read but not data-source
    // reach.
    let result = result_service::get(db, user.org_id, user, qr_id).await?;
    let data = result
        .data
        .as_ref()
        .ok_or_else(|| ApiError::not_found("result no longer retained"))?;
    let (bytes, _content_type) = result_service::format_result(data, ResultFormat::Csv)?;

    let query_name = query_name_for_run(db, user.org_id, run).await?;
    let started = run.started_at.unwrap_or(run.created_at).with_timezone(&Utc);
    // Colons are illegal in `Content-Disposition` filenames on some clients, so the ISO instant
    // uses `-`.
    let filename = format!(
        "{}_{}.csv",
        slugify(&query_name, "query"),
        started.format("%Y-%m-%dT%H-%M-%SZ")
    );
    Ok((bytes, filename))
}

/// Marks the row as actually queued (the per-data-source cap counts enqueued-but-unclaimed
/// `pending` runs); `COALESCE` keeps it a "first pushed at" stamp.
async fn stamp_enqueued(db: &impl ConnectionTrait, run_id: i64) -> Result<(), ApiError> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE runs SET enqueued_at = COALESCE(enqueued_at, now()) WHERE id = $1",
        [run_id.into()],
    ))
    .await?;
    Ok(())
}

async fn publish_wake(redis: &ConnectionManager, run_id: i64) {
    if let Err(e) = arcodash_core::redis_util::publish_sched_wake(redis).await {
        tracing::warn!(run_id, error = %e, "scheduler wake publish failed; the loop falls back to its poll cap");
    }
}

/// Resolves content like the worker's `run_lifecycle::enqueue_run` (via
/// `query_service::resolve_serving_content`): a query run serves published content once ever
/// published, else the draft; a monitor run enqueues `EvaluateMonitor`.
///
/// DB writes a reader needs run on `db` before the enclosing transaction commits; only the queue
/// push + wake nudge are deferred via [`Db::after_commit_fallible`], so a job is never dispatched
/// before its `runs` row is visible. In `Db::Pool` mode the deferral is a no-op and errors still
/// propagate, so `run_now`'s stranded-`pending` cleanup fires.
async fn enqueue_manual_run(
    db: &Db,
    queue: Arc<dyn Queue>,
    conveyor: Option<Arc<arcodash_core::Conveyor>>,
    redis: ConnectionManager,
    producer_db: DatabaseConnection,
    user_id: i64,
    run: &run::Model,
) -> Result<(), ApiError> {
    let job_id = run
        .job_id
        .clone()
        .ok_or_else(|| ApiError::internal("manual run has no job_id"))?;
    let (run_org_id, run_entity_id, run_id) = (run.org_id, run.entity_id, run.id);
    match run.entity_type.as_str() {
        "query" => {
            let q = schedule_service::load_query_org(db, run.org_id, run.entity_id).await?;
            let resolved = query_service::resolve_serving_content(&q);
            arcodash_core::queue::set_job_status(
                &redis,
                &job_id,
                run.org_id,
                Some(user_id),
                JobStatus::Pending,
                None,
                None,
            )
            .await
            .map_err(|e| ApiError::internal(format!("failed to record job status: {e}")))?;
            // Must commit atomically with the `runs` row: the per-data-source cap counts
            // enqueued-but-unclaimed `pending` runs by this column.
            stamp_enqueued(db, run.id).await?;

            let payload = arcodash_core::query_exec::ExecuteQueryPayload {
                job_id: job_id.clone(),
                org_id: run.org_id,
                data_source_id: resolved.data_source_id,
                query_text: resolved.query,
                parameters: serde_json::Value::Null,
                user_id,
                query_id: Some(run.entity_id),
                scheduled: true,
                run_id: Some(run.id),
                repoint_published: resolved.repoint_published,
                max_rows: None,
            };
            db.after_commit_fallible(Box::pin(async move {
                let queued = arcodash_core::query_exec::enqueue_execute_query(
                    &producer_db,
                    queue.as_ref(),
                    conveyor.as_deref(),
                    payload,
                )
                .await
                .map_err(|e| ApiError::internal(format!("failed to enqueue run: {e}")))?;
                if !queued {
                    // The `runs` row is `pending` and nothing will execute it. In `Db::Pool` mode
                    // this error reaches `run_now`, which deletes the row; in `Db::Txn` mode it
                    // leaks a `pending` row for the stale-pending sweep.
                    tracing::error!(
                        job_id = %job_id,
                        org_id = run_org_id,
                        query_id = run_entity_id,
                        run_id,
                        "manual run enqueue reported not-queued; job status is stale"
                    );
                    return Err(ApiError::internal(
                        "a run for this query is already in flight",
                    ));
                }
                publish_wake(&redis, run_id).await;
                Ok(())
            }))
            .await?;
        }
        "monitor" => {
            // A monitor run carries no data_source_id so never counts toward a cap; stamped anyway
            // so the column means the same on every row.
            stamp_enqueued(db, run.id).await?;
            db.after_commit_fallible(Box::pin(async move {
                queue
                    .enqueue(
                        QueueName::Default,
                        JobMessage::EvaluateMonitor {
                            org_id: run_org_id,
                            monitor_id: run_entity_id,
                            run_id: Some(run_id),
                        },
                    )
                    .await
                    .map_err(|e| ApiError::internal(format!("failed to enqueue run: {e}")))?;
                publish_wake(&redis, run_id).await;
                Ok(())
            }))
            .await?;
        }
        _ => return Err(ApiError::bad_request("unknown entity_type")),
    }
    Ok(())
}

/// Manual "run now": insert a `pending` run (`triggered_by='manual', scheduled_for=NULL`) and
/// enqueue it. `data_source_id` is `Some` for a query run, `None` for a monitor.
///
/// Idempotent under a double click via the `uq_runs_inflight` partial unique index (`INSERT ... ON
/// CONFLICT DO NOTHING RETURNING id`). A row back means we hold the slot: enqueue and return `(run,
/// false)` (201). No row means a run is already in flight: return it as `(run, true)` (200, no
/// second enqueue).
///
/// Race: a fast run can finalize between the failed insert and the follow-up SELECT, so the insert
/// is retried exactly once; a second miss returns a clean 409.
pub async fn run_now(
    db: &Db,
    queue: Arc<dyn Queue>,
    conveyor: Option<Arc<arcodash_core::Conveyor>>,
    redis: ConnectionManager,
    producer_db: DatabaseConnection,
    org_id: i64,
    user_id: i64,
    entity_type: &str,
    entity_id: i64,
    data_source_id: Option<i64>,
) -> Result<(run::Model, bool), ApiError> {
    if let Some(ds_id) = data_source_id {
        crate::services::data_source_service::require_not_paused(db, ds_id).await?;
    }

    // A monitor run gets its own metric label rather than counting as "run".
    let metrics_path = if entity_type == "monitor" {
        "monitor_eval"
    } else {
        "run"
    };

    for attempt in 0..2 {
        let job_id = Uuid::new_v4().to_string();
        let inserted = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, data_source_id, \
                                   scheduled_for, triggered_by, status, job_id, created_at) \
                 VALUES ($1,$2,$3,NULL,$4,NULL,'manual','pending',$5, now()) \
                 ON CONFLICT DO NOTHING RETURNING id",
                [
                    org_id.into(),
                    entity_type.into(),
                    entity_id.into(),
                    data_source_id.into(),
                    job_id.into(),
                ],
            ))
            .await?;

        if let Some(row) = inserted.into_iter().next() {
            let id: i64 = row.try_get("", "id")?;
            let run = run::Entity::find_by_id(id)
                .one(db)
                .await?
                .ok_or_else(|| ApiError::internal("run not found after insert"))?;
            if let Err(e) = enqueue_manual_run(
                db,
                queue.clone(),
                conveyor.clone(),
                redis.clone(),
                producer_db.clone(),
                user_id,
                &run,
            )
            .await
            {
                // Otherwise the row keeps holding `uq_runs_inflight` and blocks every scheduled
                // tick for this entity.
                if let Err(delete_err) = db
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "DELETE FROM runs WHERE id = $1 AND status = 'pending'",
                        [run.id.into()],
                    ))
                    .await
                {
                    tracing::error!(
                        run_id = run.id,
                        error = %delete_err,
                        "failed to remove stranded pending run after enqueue failure"
                    );
                }
                return Err(e);
            }
            record_event(
                db,
                org_id,
                Some(user_id),
                "run.triggered",
                Some("run"),
                Some(run.id),
                serde_json::json!({
                    "entity_type": entity_type,
                    "entity_id": entity_id,
                    "triggered_by": "manual",
                }),
            )
            .await?;
            crate::metrics::incr_query_execution(metrics_path, "enqueued");
            return Ok((run, false));
        }

        let existing = run::Entity::find()
            .filter(run::Column::OrgId.eq(org_id))
            .filter(run::Column::EntityType.eq(entity_type))
            .filter(run::Column::EntityId.eq(entity_id))
            .filter(run::Column::Status.is_in(["pending", "running"]))
            .one(db)
            .await?;
        match existing {
            Some(run) => {
                crate::metrics::incr_query_execution(metrics_path, "idempotent");
                return Ok((run, true));
            }
            None if attempt == 0 => {} // finalized between our insert and select — retry
            None => {
                crate::metrics::incr_query_execution(metrics_path, "conflict");
                return Err(ApiError::conflict("a run just finished; retry"));
            }
        }
    }
    unreachable!("loop always returns within its 2 iterations")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcodash_core::test_support::{new_data_source, new_organization, new_query, new_user};
    use async_trait::async_trait;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
        PaginatorTrait,
    };

    async fn insert_org(
        db: &DatabaseConnection,
        slug: String,
    ) -> arcodash_core::entities::organization::Model {
        let mut org = new_organization();
        org.slug = Set(slug);
        org.insert(db).await.unwrap()
    }

    /// A user with a unique email and API key derived from `label`.
    async fn insert_user(db: &DatabaseConnection, org_id: i64, label: &str) -> user::Model {
        let mut user = new_user(org_id);
        user.email = Set(format!("{label}@test.local"));
        user.api_key_hash = Set(arcodash_core::hash::hash_api_key(label));
        user.insert(db).await.unwrap()
    }

    /// Always fails the enqueue, to exercise `run_now`'s cleanup of the inserted `pending` row.
    struct FailingQueue;

    #[async_trait]
    impl Queue for FailingQueue {
        async fn enqueue(&self, _queue: QueueName, _job: JobMessage) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("simulated enqueue failure"))
        }
    }

    /// Captures the `JobMessage` a manual run enqueues.
    struct CapturingQueue(std::sync::Mutex<Option<JobMessage>>);

    #[async_trait]
    impl Queue for CapturingQueue {
        async fn enqueue(&self, _queue: QueueName, job: JobMessage) -> anyhow::Result<()> {
            *self.0.lock().unwrap() = Some(job);
            Ok(())
        }
    }

    /// A manual run must execute under the triggering caller's identity, not the owner's:
    /// `job.user_id` is the principal a `query_results`-backed query resolves against. Needs real
    /// Postgres + Redis.
    #[tokio::test]
    async fn enqueue_manual_run_stamps_the_caller_not_the_owner() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let tag = uuid::Uuid::new_v4().simple().to_string();
        let org = insert_org(&db, format!("run-svc-caller-{tag}")).await;
        let ds = new_data_source(org.id).insert(&db).await.unwrap();
        let owner = insert_user(&db, org.id, &format!("run-svc-owner-{tag}")).await;
        let caller = insert_user(&db, org.id, &format!("run-svc-caller-{tag}")).await;

        let query = new_query(org.id, ds.id, owner.id)
            .insert(&db)
            .await
            .unwrap();

        let pool_db = Db::Pool(db.clone());
        let queue = Arc::new(CapturingQueue(std::sync::Mutex::new(None)));
        let (_run, already) = run_now(
            &pool_db,
            queue.clone(),
            None,
            redis.clone(),
            db.clone(),
            org.id,
            caller.id,
            "query",
            query.id,
            Some(ds.id),
        )
        .await
        .unwrap();
        assert!(!already, "first manual run must not be an idempotent hit");

        let captured = queue
            .0
            .lock()
            .unwrap()
            .take()
            .expect("legacy queue must have received the enqueued job");
        match captured {
            JobMessage::ExecuteQuery { user_id, .. } => {
                assert_eq!(
                    user_id, caller.id,
                    "manual run must execute under the triggering caller, not the query owner"
                );
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    /// A second `stamp_enqueued` call must not move `enqueued_at` forward ("first pushed at").
    /// Needs a real Postgres.
    #[tokio::test]
    async fn stamp_enqueued_is_idempotent_under_a_re_enqueue() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();

        let tag = uuid::Uuid::new_v4().simple().to_string();
        let org = insert_org(&db, format!("run-svc-stamp-{tag}")).await;
        let ds = new_data_source(org.id).insert(&db).await.unwrap();
        let user = insert_user(&db, org.id, &format!("run-svc-stamp-{tag}")).await;
        let query = new_query(org.id, ds.id, user.id).insert(&db).await.unwrap();

        let now = Utc::now();
        let created = run::ActiveModel {
            id: NotSet,
            org_id: Set(org.id),
            entity_type: Set("query".to_string()),
            entity_id: Set(query.id),
            schedule_id: Set(None),
            data_source_id: Set(Some(ds.id)),
            scheduled_for: Set(None),
            triggered_by: Set("manual".to_string()),
            status: Set("pending".to_string()),
            waiting_on_run_id: Set(None),
            job_id: Set(None),
            worker_id: Set(None),
            lease_expires_at: Set(None),
            started_at: Set(None),
            finished_at: Set(None),
            duration_ms: Set(None),
            row_count: Set(None),
            error: Set(None),
            data_age_secs: Set(None),
            query_result_id: Set(None),
            condition_met: Set(None),
            created_at: Set(now.into()),
            enqueued_at: NotSet,
        }
        .insert(&db)
        .await
        .unwrap();

        stamp_enqueued(&db, created.id).await.unwrap();
        let after_first = run::Entity::find_by_id(created.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let first_stamp = after_first
            .enqueued_at
            .expect("first stamp must set enqueued_at");

        stamp_enqueued(&db, created.id).await.unwrap();
        let after_second = run::Entity::find_by_id(created.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_second.enqueued_at,
            Some(first_stamp),
            "a second stamp must not move enqueued_at forward"
        );
    }

    /// A failed enqueue must not strand the inserted `pending` row, which would hold
    /// `uq_runs_inflight` and block later runs. Needs real Postgres + Redis.
    #[tokio::test]
    async fn run_now_deletes_its_pending_row_when_enqueue_fails() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let tag = uuid::Uuid::new_v4().simple().to_string();
        let org = insert_org(&db, format!("run-svc-fail-{tag}")).await;
        let ds = new_data_source(org.id).insert(&db).await.unwrap();
        let user = insert_user(&db, org.id, &format!("run-svc-fail-{tag}")).await;
        let query = new_query(org.id, ds.id, user.id).insert(&db).await.unwrap();

        let pool_db = Db::Pool(db.clone());
        let result = run_now(
            &pool_db,
            Arc::new(FailingQueue),
            None,
            redis.clone(),
            db.clone(),
            org.id,
            user.id,
            "query",
            query.id,
            Some(ds.id),
        )
        .await;
        assert!(result.is_err(), "a failed enqueue must propagate its error");

        let survivors = run::Entity::find()
            .filter(run::Column::EntityType.eq("query"))
            .filter(run::Column::EntityId.eq(query.id))
            .filter(run::Column::Status.is_in(["pending", "running"]))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            survivors, 0,
            "the pending row must not survive a failed enqueue"
        );

        let queue = Arc::new(arcodash_core::queue::ValkeyQueue::new(redis.clone()));
        let (_run, already) = run_now(
            &pool_db,
            queue,
            None,
            redis.clone(),
            db.clone(),
            org.id,
            user.id,
            "query",
            query.id,
            Some(ds.id),
        )
        .await
        .expect("a following run_now must not collide on uq_runs_inflight");
        assert!(!already, "the retry must be a fresh run, not the dead one");
    }
}
