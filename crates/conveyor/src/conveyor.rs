//! The `Conveyor` handle: enqueue/schedule tasks, run the consumer+scheduler loops, and read
//! status/stats/live snapshots.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, QuerySelect, Statement,
};
use serde::Serialize;
use tokio::sync::watch;

use crate::consumer;
use crate::entities::{schedule as schedule_entity, task as task_entity};
use crate::handler::{TaskHandler, WorkerHeartbeat};
use crate::ids::{ScheduleId, TaskId, TaskType};
use crate::queue;
use crate::resource;
use crate::schedule_spec::{self, ScheduleSpec};
use crate::scheduler;

#[derive(Clone)]
pub struct TierSpec {
    pub(crate) name: String,
    pub(crate) task_types: Vec<TaskType>,
    pub(crate) concurrency: usize,
    /// Grace period a loop in this tier waits for in-flight tasks on shutdown before abandoning
    /// them to the lease-expiry sweep (see `ConveyorBuilder::tier_with_drain`).
    pub(crate) drain_secs: u64,
}

pub struct Conveyor {
    pub(crate) db: DatabaseConnection,
    pub(crate) valkey: ConnectionManager,
    pub(crate) redis_url: String,
    pub(crate) registry: Arc<HashMap<TaskType, Arc<dyn TaskHandler>>>,
    pub(crate) tiers: Vec<TierSpec>,
}

/// `enqueue` conflict: a task of this `task_type`/`entity_id` is already `pending`/`running`
/// (`uq_conveyor_tasks_inflight`). Downcast via `anyhow::Error::downcast_ref::<AlreadyInflight>()`.
#[derive(Debug, thiserror::Error)]
#[error("a task is already pending/running for this (task_type, entity_id)")]
pub struct AlreadyInflight;

/// Result of [`Conveyor::enqueue`]: the inserted row, and whether it landed on its queue.
///
/// `queued = false` means it was parked `pending` over its resource cap; callers must not stamp
/// their own "queued" state then.
#[derive(Debug)]
pub struct Enqueued {
    pub task_id: TaskId,
    pub queued: bool,
}

pub struct EnqueueRequest {
    pub task_type: TaskType,
    pub payload: serde_json::Value,
    pub tenant_id: Option<i64>,
    pub entity_id: Option<i64>,
    pub resource_key: Option<String>,
    pub resource_cap: Option<i64>,
}

pub struct ScheduleRequest {
    pub task_type: TaskType,
    pub entity_id: Option<i64>,
    pub payload: serde_json::Value,
    pub spec: ScheduleSpec,
    pub timezone: String,
    pub overlap_policy: OverlapPolicy,
    pub tenant_id: Option<i64>,
    pub resource_key: Option<String>,
    pub resource_cap: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    Skip,
    Coalesce,
}

impl OverlapPolicy {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Coalesce => "coalesce",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running,
    Success,
    Failed,
    TimedOut,
    Cancelled,
    Skipped,
}

impl TaskState {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub(crate) fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "success" => Self::Success,
            "failed" => Self::Failed,
            "timed_out" => Self::TimedOut,
            "cancelled" => Self::Cancelled,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }

    pub(crate) const fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }
}

#[derive(Serialize)]
pub struct TaskStatus {
    pub task_id: TaskId,
    pub task_type: TaskType,
    pub state: TaskState,
    pub attempt: u32,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct LiveSnapshot {
    pub queue_depths: Vec<(TaskType, i64)>,
    pub workers: Vec<WorkerHeartbeat>,
    pub running: Vec<TaskStatus>,
    pub stuck_pending: Vec<TaskStatus>,
    pub dead_letter_count: i64,
}

impl Conveyor {
    /// Run the consumer tiers plus the scheduler/sweeper loop until `shutdown` fires; returns once
    /// every tier has drained (or hit its grace budget).
    pub async fn run(&self, shutdown: watch::Receiver<bool>) {
        let consumers = consumer::run_tiers(
            self.db.clone(),
            self.valkey.clone(),
            self.redis_url.clone(),
            self.registry.clone(),
            self.tiers.clone(),
            shutdown.clone(),
        );
        let sched = scheduler::run(
            self.db.clone(),
            self.valkey.clone(),
            self.redis_url.clone(),
            shutdown,
        );
        tokio::join!(consumers, sched);
    }

    /// Insert a `pending` task row and (unless its resource cap is saturated) push it onto its
    /// queue. A `Some` `entity_id` already in flight returns [`AlreadyInflight`]; `None` never
    /// conflicts.
    pub async fn enqueue(&self, req: EnqueueRequest) -> anyhow::Result<Enqueued> {
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks \
                 (tenant_id, task_type, entity_id, payload, resource_key, resource_cap, triggered_by, status, attempt, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'enqueue', 'pending', 1, now()) \
                 ON CONFLICT (task_type, entity_id) WHERE status IN ('pending', 'running') \
                 DO NOTHING \
                 RETURNING id",
                [
                    req.tenant_id.into(),
                    req.task_type.0.clone().into(),
                    req.entity_id.into(),
                    req.payload.clone().into(),
                    req.resource_key.clone().into(),
                    req.resource_cap.into(),
                ],
            ))
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Err(AlreadyInflight.into());
        };
        let task_id = TaskId(row.try_get("", "id")?);

        if resource::over_cap(
            &self.db,
            req.resource_key.as_deref(),
            req.resource_cap,
            task_id.0,
        )
        .await?
        {
            return Ok(Enqueued {
                task_id,
                queued: false,
            });
        }
        queue::stamp_and_push(
            &self.db,
            &self.valkey,
            task_id,
            req.task_type,
            req.payload,
            1,
        )
        .await?;
        Ok(Enqueued {
            task_id,
            queued: true,
        })
    }

    /// Create a recurring schedule; `next_run_at` is computed immediately (no jitter, since there
    /// is no id yet).
    pub async fn schedule(&self, req: ScheduleRequest) -> anyhow::Result<ScheduleId> {
        schedule_spec::validate(&req.spec, schedule_spec::EVERY_SECS_FLOOR)?;
        let now = Utc::now();
        let next_run_at = schedule_spec::compute_next(&req.spec, &req.timezone, now, 0, 0)?;

        let model = schedule_entity::ActiveModel {
            tenant_id: Set(req.tenant_id),
            task_type: Set(req.task_type.0),
            entity_id: Set(req.entity_id),
            payload: Set(req.payload),
            spec: Set(serde_json::to_value(&req.spec)?),
            timezone: Set(req.timezone),
            enabled: Set(true),
            overlap_policy: Set(req.overlap_policy.as_db_str().to_string()),
            next_run_at: Set(Some(next_run_at.into())),
            resource_key: Set(req.resource_key),
            resource_cap: Set(req.resource_cap),
            ..Default::default()
        };
        let inserted = model.insert(&self.db).await?;
        Ok(ScheduleId(inserted.id))
    }

    /// Idempotent variant of [`Conveyor::schedule`] per `(task_type, entity_id)`: an existing row's
    /// spec/payload/timezone/overlap/resource fields are updated in place. `enabled` and
    /// `paused_until` are left alone so a boot never un-pauses an operator's pause.
    /// `auto_paused_at`, `consecutive_failures` and `next_run_at` reset only when `spec` changed
    /// (read as the operator fixing the cause); otherwise restarts would resurrect a poison
    /// schedule. Use [`Conveyor::resume_schedule`] for an explicit clear.
    pub async fn upsert_schedule(&self, req: ScheduleRequest) -> anyhow::Result<ScheduleId> {
        schedule_spec::validate(&req.spec, schedule_spec::EVERY_SECS_FLOOR)?;
        let now = Utc::now();
        let next_run_at = schedule_spec::compute_next(&req.spec, &req.timezone, now, 0, 0)?;
        let spec_json = serde_json::to_value(&req.spec)?;

        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_schedules \
                 (tenant_id, task_type, entity_id, payload, spec, timezone, enabled, \
                  overlap_policy, next_run_at, resource_key, resource_cap) \
                 VALUES ($1, $2, $3, $4, $5, $6, true, $7, $8, $9, $10) \
                 ON CONFLICT (task_type, COALESCE(entity_id, -1)) DO UPDATE SET \
                   payload = EXCLUDED.payload, \
                   spec = EXCLUDED.spec, \
                   timezone = EXCLUDED.timezone, \
                   overlap_policy = EXCLUDED.overlap_policy, \
                   resource_key = EXCLUDED.resource_key, \
                   resource_cap = EXCLUDED.resource_cap, \
                   updated_at = now(), \
                   next_run_at = CASE WHEN conveyor_schedules.spec IS DISTINCT FROM EXCLUDED.spec \
                                       THEN EXCLUDED.next_run_at \
                                       ELSE conveyor_schedules.next_run_at END, \
                   auto_paused_at = CASE WHEN conveyor_schedules.spec IS DISTINCT FROM EXCLUDED.spec \
                                          THEN NULL \
                                          ELSE conveyor_schedules.auto_paused_at END, \
                   consecutive_failures = CASE WHEN conveyor_schedules.spec IS DISTINCT FROM EXCLUDED.spec \
                                               THEN 0 \
                                               ELSE conveyor_schedules.consecutive_failures END \
                 RETURNING id",
                [
                    req.tenant_id.into(),
                    req.task_type.0.into(),
                    req.entity_id.into(),
                    req.payload.into(),
                    spec_json.into(),
                    req.timezone.into(),
                    req.overlap_policy.as_db_str().into(),
                    next_run_at.into(),
                    req.resource_key.into(),
                    req.resource_cap.into(),
                ],
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("conveyor: upsert_schedule returned no row"))?;
        Ok(ScheduleId(row.try_get("", "id")?))
    }

    /// Apply one schedule mutation, reporting whether it matched a row; a stale id warns and
    /// returns `false`.
    async fn update_schedule(
        &self,
        stmt: Statement,
        id: ScheduleId,
        op: &'static str,
    ) -> anyhow::Result<bool> {
        let matched = self.db.execute_raw(stmt).await?.rows_affected() == 1;
        if !matched {
            tracing::warn!(schedule_id = id.0, op, "no matching schedule row");
        }
        Ok(matched)
    }

    /// Pause `id` until `until`. Returns whether `id` matched a row.
    pub async fn pause_schedule_until(
        &self,
        id: ScheduleId,
        until: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        self.update_schedule(
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_schedules SET paused_until = $1, updated_at = now() WHERE id = $2",
                [Some(until).into(), id.0.into()],
            ),
            id,
            "pause_schedule_until",
        )
        .await
    }

    /// Clears an auto-pause and the failure counter so the schedule fires again from its existing
    /// `next_run_at`. Leaves `enabled` alone. Returns whether `id` matched a row.
    pub async fn resume_schedule(&self, id: ScheduleId) -> anyhow::Result<bool> {
        self.update_schedule(
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_schedules \
                 SET auto_paused_at = NULL, paused_until = NULL, consecutive_failures = 0, updated_at = now() \
                 WHERE id = $1",
                [id.0.into()],
            ),
            id,
            "resume_schedule",
        )
        .await
    }

    /// Returns whether `id` matched a row.
    pub async fn cancel_schedule(&self, id: ScheduleId) -> anyhow::Result<bool> {
        self.update_schedule(
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_schedules SET enabled = false, updated_at = now() WHERE id = $1",
                [id.0.into()],
            ),
            id,
            "cancel_schedule",
        )
        .await
    }

    pub async fn status(&self, task_id: TaskId) -> anyhow::Result<Option<TaskStatus>> {
        let Some(row) = task_entity::Entity::find_by_id(task_id.0)
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        let Some(state) = TaskState::from_db_str(&row.status) else {
            return Ok(None);
        };
        Ok(Some(TaskStatus::from_row(row, state)))
    }

    /// A diagnostic snapshot, capped at [`crate::config::live_snapshot_limit`] per category.
    pub async fn live(&self) -> anyhow::Result<LiveSnapshot> {
        let queue_depths = queue::all_queue_depths(&self.valkey)
            .await?
            .into_iter()
            .map(|(name, depth)| (TaskType(name), depth))
            .collect();

        let workers = queue::worker_heartbeats(&self.valkey)
            .await?
            .into_iter()
            .map(|(worker_id, last_seen)| WorkerHeartbeat {
                worker_id,
                last_seen,
            })
            .collect();

        let running = self.query_running().await?;
        let stuck_pending = self.query_stuck_pending().await?;
        let dead_letter_count = {
            let mut conn = self.valkey.clone();
            redis::AsyncCommands::llen(&mut conn, queue::DEAD_LETTER_QUEUE).await?
        };

        Ok(LiveSnapshot {
            queue_depths,
            workers,
            running,
            stuck_pending,
            dead_letter_count,
        })
    }

    async fn query_running(&self) -> anyhow::Result<Vec<TaskStatus>> {
        let rows = task_entity::Entity::find()
            .filter(task_entity::Column::Status.eq("running"))
            .limit(crate::config::live_snapshot_limit())
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| TaskStatus::from_row(row, TaskState::Running))
            .collect())
    }

    async fn query_stuck_pending(&self) -> anyhow::Result<Vec<TaskStatus>> {
        let cutoff = Utc::now() - chrono::Duration::seconds(crate::config::stale_pending_secs());
        let rows = task_entity::Entity::find()
            .filter(task_entity::Column::Status.eq("pending"))
            .filter(task_entity::Column::CreatedAt.lt(cutoff))
            .limit(crate::config::live_snapshot_limit())
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| TaskStatus::from_row(row, TaskState::Pending))
            .collect())
    }
}

impl TaskStatus {
    /// Project a task row onto the public status shape; `state` is supplied by the caller.
    fn from_row(row: task_entity::Model, state: TaskState) -> Self {
        Self {
            task_id: TaskId(row.id),
            task_type: row.task_type.into(),
            state,
            attempt: row.attempt.max(0).cast_unsigned(),
            error: row.error,
            started_at: row.started_at.map(|t| t.with_timezone(&Utc)),
            finished_at: row.finished_at.map(|t| t.with_timezone(&Utc)),
        }
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end tests against a real `Conveyor::run()` loop; real Postgres + Redis only.
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::watch;

    use super::*;
    use crate::builder::ConveyorBuilder;
    use crate::handler::{TaskContext, TaskOutcome, TaskResult, TypedTaskHandler};
    use crate::test_support::*;

    #[derive(serde::Deserialize)]
    struct TogglePayload {
        #[serde(default)]
        n: i64,
    }

    /// Records that it ran and finalizes as `Success`, echoing the payload's `n` in `result_ref`.
    struct TrackingHandler {
        ran: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TypedTaskHandler for TrackingHandler {
        type Payload = TogglePayload;
        async fn run(&self, _ctx: TaskContext, payload: TogglePayload) -> TaskResult {
            self.ran.fetch_add(1, Ordering::SeqCst);
            Ok(TaskOutcome::Success {
                result_ref: Some(serde_json::json!({"n": payload.n})),
            })
        }
    }

    async fn wait_for_terminal(
        conveyor: &Conveyor,
        task_id: TaskId,
        deadline: Duration,
    ) -> TaskStatus {
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = conveyor.status(task_id).await.unwrap() {
                if matches!(
                    status.state,
                    TaskState::Success
                        | TaskState::Failed
                        | TaskState::TimedOut
                        | TaskState::Cancelled
                        | TaskState::Skipped
                ) {
                    return status;
                }
            }
            assert!(
                start.elapsed() < deadline,
                "task {task_id:?} never reached a terminal state within {deadline:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `enqueue`'s push path must stamp `enqueued_at` before the envelope reaches the queue.
    #[tokio::test]
    async fn enqueue_stamps_enqueued_at_before_pushing() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db.clone(), valkey.clone(), redis_url).build();
        let task_type = format!("stamp_toy_{}", uuid::Uuid::new_v4());

        let enqueued = conveyor
            .enqueue(EnqueueRequest {
                task_type: task_type.as_str().into(),
                payload: serde_json::Value::Null,
                tenant_id: None,
                entity_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap();
        assert!(enqueued.queued, "an uncapped task must actually queue");

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT enqueued_at FROM conveyor_tasks WHERE id = $1",
                [enqueued.task_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let enqueued_at: Option<DateTime<Utc>> = row.try_get("", "enqueued_at").unwrap();
        assert!(
            enqueued_at.is_some(),
            "enqueued_at must be stamped by the time enqueue() returns"
        );

        let key = queue::queue_key(&task_type);
        let popped = queue::blpop(&valkey, &key, 2.0).await.unwrap();
        assert!(
            popped.is_some(),
            "exactly one envelope must be on the queue"
        );
        let popped_again = queue::blpop(&valkey, &key, 0.1).await.unwrap();
        assert!(
            popped_again.is_none(),
            "exactly one envelope must have been pushed, not more"
        );
    }

    #[tokio::test]
    async fn enqueue_conflict_returns_typed_already_inflight_not_a_raw_db_error() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let task_type: TaskType = format!("inflight_{}", uuid::Uuid::new_v4()).into();
        let entity_id = Some(unique_entity_id());

        let mk_req = || EnqueueRequest {
            task_type: task_type.clone(),
            payload: serde_json::Value::Null,
            tenant_id: None,
            entity_id,
            resource_key: None,
            resource_cap: None,
        };

        conveyor.enqueue(mk_req()).await.unwrap();
        let err = conveyor
            .enqueue(mk_req())
            .await
            .expect_err("a second enqueue for the same (task_type, entity_id) must fail");
        assert!(
            err.downcast_ref::<AlreadyInflight>().is_some(),
            "must be the typed AlreadyInflight error, not a raw unique-violation: {err:#}"
        );

        let req_no_entity = EnqueueRequest {
            task_type,
            payload: serde_json::Value::Null,
            tenant_id: None,
            entity_id: None,
            resource_key: None,
            resource_cap: None,
        };
        conveyor.enqueue(req_no_entity).await.unwrap();
    }

    fn interval_req(task_type: &TaskType, every_secs: i64) -> ScheduleRequest {
        ScheduleRequest {
            task_type: task_type.clone(),
            entity_id: None,
            payload: serde_json::Value::Null,
            spec: ScheduleSpec::Interval { every_secs },
            timezone: "UTC".to_string(),
            overlap_policy: OverlapPolicy::Skip,
            tenant_id: None,
            resource_key: None,
            resource_cap: None,
        }
    }

    /// A NULL `entity_id` never collides in a plain unique index; the `COALESCE(entity_id, -1)`
    /// expression index makes a repeated call update the first row.
    #[tokio::test]
    async fn upsert_schedule_is_idempotent_for_an_entity_less_schedule() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let task_type: TaskType = format!("upsert_no_entity_{}", uuid::Uuid::new_v4()).into();

        let first_id = conveyor
            .upsert_schedule(interval_req(&task_type, 300))
            .await
            .unwrap();
        let second_id = conveyor
            .upsert_schedule(interval_req(&task_type, 600))
            .await
            .unwrap();
        assert_eq!(
            first_id.0, second_id.0,
            "two upserts for the same entity-less task_type must return the same schedule id, \
             not create a second row"
        );

        let reloaded = schedule_entity::Entity::find_by_id(second_id.0)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        let spec: ScheduleSpec = serde_json::from_value(reloaded.spec).unwrap();
        assert_eq!(
            spec,
            ScheduleSpec::Interval { every_secs: 600 },
            "the second upsert's spec must have overwritten the first"
        );
    }

    #[tokio::test]
    async fn resume_schedule_clears_an_auto_pause_and_the_failure_counter() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let schedule_id = insert_schedule(
            &db,
            &format!("resume_toy_{}", uuid::Uuid::new_v4()),
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now(),
            5,
        )
        .await;
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_schedules SET auto_paused_at = now() WHERE id = $1",
            [schedule_id.into()],
        ))
        .await
        .unwrap();

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let matched = conveyor
            .resume_schedule(ScheduleId(schedule_id))
            .await
            .unwrap();
        assert!(matched);

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        assert!(reloaded.auto_paused_at.is_none());
        assert_eq!(reloaded.consecutive_failures, 0);
        assert!(
            reloaded.enabled,
            "resume_schedule must never touch enabled — only cancel_schedule does"
        );
    }

    #[tokio::test]
    async fn pause_schedule_until_makes_a_due_schedule_not_due() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let schedule_id = insert_schedule(
            &db,
            &format!("pause_until_toy_{}", uuid::Uuid::new_v4()),
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now() - chrono::Duration::seconds(5),
            0,
        )
        .await;
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();

        let until = Utc::now() + chrono::Duration::days(1);
        assert!(conveyor
            .pause_schedule_until(ScheduleId(schedule_id), until)
            .await
            .unwrap());

        let planned = crate::scheduler::claim_and_plan(&conveyor.db, Utc::now())
            .await
            .unwrap();
        assert!(
            planned.iter().all(|t| t.schedule_id != Some(schedule_id)),
            "a schedule paused until the future must not be claimed as due"
        );
    }

    #[tokio::test]
    async fn resume_schedule_makes_a_paused_schedule_due_again() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let schedule_id = insert_schedule(
            &db,
            &format!("resume_due_toy_{}", uuid::Uuid::new_v4()),
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now() - chrono::Duration::seconds(5),
            0,
        )
        .await;
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();

        let until = Utc::now() + chrono::Duration::days(1);
        conveyor
            .pause_schedule_until(ScheduleId(schedule_id), until)
            .await
            .unwrap();
        conveyor
            .resume_schedule(ScheduleId(schedule_id))
            .await
            .unwrap();

        let planned = crate::scheduler::claim_and_plan(&conveyor.db, Utc::now())
            .await
            .unwrap();
        assert!(
            planned.iter().any(|t| t.schedule_id == Some(schedule_id)),
            "a resumed schedule must be claimable as due again"
        );
    }

    #[tokio::test]
    async fn resume_schedule_reports_a_missing_row() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let matched = conveyor.resume_schedule(ScheduleId(-1)).await.unwrap();
        assert!(!matched);
    }

    /// The schedule mutations share one `rows_affected() == 1` check via `update_schedule`; this
    /// guards it.
    #[tokio::test]
    async fn pause_schedule_until_reports_a_missing_row() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let matched = conveyor
            .pause_schedule_until(ScheduleId(-1), Utc::now() + chrono::Duration::days(1))
            .await
            .unwrap();
        assert!(!matched);
    }

    #[tokio::test]
    async fn cancel_schedule_returns_false_for_an_unknown_id() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let matched = conveyor.cancel_schedule(ScheduleId(-1)).await.unwrap();
        assert!(!matched);
    }

    #[tokio::test]
    async fn cancel_schedule_disables_a_real_schedule() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let schedule_id = insert_schedule(
            &db,
            &format!("cancel_toy_{}", uuid::Uuid::new_v4()),
            Some(unique_entity_id()),
            serde_json::json!({"kind":"interval","every_secs":300}),
            "skip",
            Utc::now(),
            0,
        )
        .await;
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();

        let matched = conveyor
            .cancel_schedule(ScheduleId(schedule_id))
            .await
            .unwrap();
        assert!(matched);

        let reloaded = schedule_entity::Entity::find_by_id(schedule_id)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        assert!(!reloaded.enabled);
    }

    /// An `upsert_schedule` with an unchanged spec (every restart) must never resurrect a poison
    /// schedule's auto-pause.
    #[tokio::test]
    async fn upsert_schedule_leaves_an_auto_pause_intact_when_the_spec_is_unchanged() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let task_type: TaskType = format!("upsert_unchanged_{}", uuid::Uuid::new_v4()).into();

        let id = conveyor
            .upsert_schedule(interval_req(&task_type, 300))
            .await
            .unwrap();
        conveyor
            .db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_schedules SET auto_paused_at = now(), consecutive_failures = 10 WHERE id = $1",
                [id.0.into()],
            ))
            .await
            .unwrap();

        conveyor
            .upsert_schedule(interval_req(&task_type, 300))
            .await
            .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(id.0)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.auto_paused_at.is_some(),
            "an upsert with an unchanged spec must leave an existing auto-pause alone"
        );
        assert_eq!(reloaded.consecutive_failures, 10);
    }

    /// A changed spec resets the auto-pause (read as the operator fixing the cause).
    #[tokio::test]
    async fn upsert_schedule_clears_an_auto_pause_when_the_spec_changes() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let task_type: TaskType = format!("upsert_changed_{}", uuid::Uuid::new_v4()).into();

        let id = conveyor
            .upsert_schedule(interval_req(&task_type, 300))
            .await
            .unwrap();
        let before = schedule_entity::Entity::find_by_id(id.0)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        conveyor
            .db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE conveyor_schedules SET auto_paused_at = now(), consecutive_failures = 10 WHERE id = $1",
                [id.0.into()],
            ))
            .await
            .unwrap();

        conveyor
            .upsert_schedule(interval_req(&task_type, 600))
            .await
            .unwrap();

        let reloaded = schedule_entity::Entity::find_by_id(id.0)
            .one(&conveyor.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.auto_paused_at.is_none(),
            "a changed spec must clear an existing auto-pause"
        );
        assert_eq!(reloaded.consecutive_failures, 0);
        assert!(
            reloaded.next_run_at > before.next_run_at,
            "a changed spec must also advance next_run_at, same as the pre-existing rule"
        );
    }

    #[tokio::test]
    async fn register_pickup_and_run_through_a_real_conveyor_run_loop() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("toy_ok_{}", uuid::Uuid::new_v4());
        let ran = Arc::new(AtomicUsize::new(0));

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url)
            .register(task_type.as_str(), TrackingHandler { ran: ran.clone() })
            .tier("fast", &[task_type.as_str()], 2)
            .build();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let task_id = conveyor
            .enqueue(EnqueueRequest {
                task_type: task_type.as_str().into(),
                payload: serde_json::json!({"n": 42}),
                tenant_id: None,
                entity_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap()
            .task_id;

        let status = wait_for_terminal(&conveyor, task_id, Duration::from_secs(10)).await;
        assert_eq!(status.state, TaskState::Success);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the handler must actually have run"
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(10), run_handle)
            .await
            .expect("run() must return once shutdown fires")
            .unwrap();
    }

    struct ResourceCapHandler {
        release: Arc<tokio::sync::Notify>,
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TypedTaskHandler for ResourceCapHandler {
        type Payload = serde_json::Value;
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            Ok(TaskOutcome::Success { result_ref: None })
        }
    }

    #[tokio::test]
    async fn resource_cap_gate_actually_caps_concurrency() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("cap_task_{}", uuid::Uuid::new_v4());
        let key = format!("cap_res_{}", uuid::Uuid::new_v4());
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(AtomicUsize::new(0));

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url)
            .register(
                task_type.as_str(),
                ResourceCapHandler {
                    release: release.clone(),
                    started: started.clone(),
                },
            )
            .tier("fast", &[task_type.as_str()], 4)
            .build();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let mk_req = || EnqueueRequest {
            task_type: task_type.as_str().into(),
            payload: serde_json::Value::Null,
            tenant_id: None,
            entity_id: None,
            resource_key: Some(key.clone()),
            resource_cap: Some(1),
        };

        let a = conveyor.enqueue(mk_req()).await.unwrap().task_id;
        let start = std::time::Instant::now();
        while started.load(Ordering::SeqCst) < 1 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "task A never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let b = conveyor.enqueue(mk_req()).await.unwrap().task_id;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let b_status = conveyor.status(b).await.unwrap().unwrap();
        assert_eq!(
            b_status.state,
            TaskState::Pending,
            "B must stay parked at cap 1"
        );
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "B's handler must not have started"
        );

        // Release A, then run a stale-pending sweep tick directly to prove the freed slot lets B
        // through.
        release.notify_one();
        wait_for_terminal(&conveyor, a, Duration::from_secs(5)).await;
        crate::scheduler::try_enqueue(
            &conveyor.db,
            &conveyor.valkey,
            &crate::entities::task::Entity::find_by_id(b.0)
                .one(&conveyor.db)
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();

        release.notify_one();
        let b_final = wait_for_terminal(&conveyor, b, Duration::from_secs(5)).await;
        assert_eq!(b_final.state, TaskState::Success);
        assert_eq!(
            started.load(Ordering::SeqCst),
            2,
            "B's handler must have run once the slot freed"
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(10), run_handle)
            .await
            .expect("run() must return once shutdown fires")
            .unwrap();
    }

    /// `enqueue`'s `queued` flag must reflect whether the row landed on its queue. Uses a
    /// producer-only handle, so an under-cap task stays deterministically `pending`.
    #[tokio::test]
    async fn enqueue_reports_whether_it_actually_queued_and_leaves_a_parked_row_unstamped() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db.clone(), valkey, redis_url).build();
        let task_type = format!("parked_{}", uuid::Uuid::new_v4());
        let key = format!("parked_res_{}", uuid::Uuid::new_v4());

        let mk_req = || EnqueueRequest {
            task_type: task_type.as_str().into(),
            payload: serde_json::Value::Null,
            tenant_id: None,
            entity_id: None,
            resource_key: Some(key.clone()),
            resource_cap: Some(1),
        };

        let a = conveyor.enqueue(mk_req()).await.unwrap();
        assert!(a.queued, "the first task under the cap must actually queue");

        let b = conveyor.enqueue(mk_req()).await.unwrap();
        assert!(
            !b.queued,
            "a task parked over the cap must report queued = false"
        );

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT enqueued_at FROM conveyor_tasks WHERE id = $1",
                [b.task_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let enqueued_at: Option<DateTime<Utc>> = row.try_get("", "enqueued_at").unwrap();
        assert!(
            enqueued_at.is_none(),
            "a parked task must not have enqueued_at stamped"
        );
    }

    /// Blocks until released, so the waiter parks before it finalizes (parking on an
    /// already-terminal target is a known gap left to a domain-specific sweep).
    struct WaitOnTargetHandler {
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl TypedTaskHandler for WaitOnTargetHandler {
        type Payload = serde_json::Value;
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            self.release.notified().await;
            Ok(TaskOutcome::Success { result_ref: None })
        }
    }

    struct WaitOnWaiterHandler {
        target_id: Arc<std::sync::OnceLock<TaskId>>,
        resumed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TaskHandler for WaitOnWaiterHandler {
        async fn run(&self, ctx: TaskContext, payload: serde_json::Value) -> TaskResult {
            if payload.get("done_task_id").is_some() {
                self.resumed.store(true, Ordering::SeqCst);
                return Ok(TaskOutcome::Success { result_ref: None });
            }
            let _ = ctx;
            let target_id = *self
                .target_id
                .get()
                .expect("target_id must be set before the waiter is ever dispatched");
            Ok(TaskOutcome::WaitOn { task_id: target_id })
        }
    }

    async fn waiting_on(conveyor: &Conveyor, task_id: TaskId) -> Option<i64> {
        let row = conveyor
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT waiting_on_task_id FROM conveyor_tasks WHERE id = $1",
                [task_id.0.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "waiting_on_task_id").unwrap()
    }

    #[tokio::test]
    async fn wait_on_parks_and_resumes_through_a_real_fan_out() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let target_type = format!("wo_target_{}", uuid::Uuid::new_v4());
        let waiter_type = format!("wo_waiter_{}", uuid::Uuid::new_v4());

        let release = Arc::new(tokio::sync::Notify::new());
        let resumed = Arc::new(AtomicBool::new(false));
        let target_id_cell: Arc<std::sync::OnceLock<TaskId>> = Arc::new(std::sync::OnceLock::new());

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url)
            .register(
                target_type.as_str(),
                WaitOnTargetHandler {
                    release: release.clone(),
                },
            )
            .register(
                waiter_type.as_str(),
                WaitOnWaiterHandler {
                    target_id: target_id_cell.clone(),
                    resumed: resumed.clone(),
                },
            )
            .tier("fast", &[target_type.as_str(), waiter_type.as_str()], 4)
            .build();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let target_id = conveyor
            .enqueue(EnqueueRequest {
                task_type: target_type.as_str().into(),
                payload: serde_json::Value::Null,
                tenant_id: None,
                entity_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap()
            .task_id;
        target_id_cell.set(target_id).unwrap();

        let waiter_id = conveyor
            .enqueue(EnqueueRequest {
                task_type: waiter_type.as_str().into(),
                payload: serde_json::Value::Null,
                tenant_id: None,
                entity_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap()
            .task_id;

        // Confirm the waiter actually parks before releasing the target.
        let start = std::time::Instant::now();
        loop {
            if waiting_on(&conveyor, waiter_id).await == Some(target_id.0) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "waiter never parked on the target"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let target_status = conveyor.status(target_id).await.unwrap().unwrap();
        assert_eq!(
            target_status.state,
            TaskState::Running,
            "target must still be in flight when the waiter parks"
        );

        release.notify_one();

        let final_status = wait_for_terminal(&conveyor, waiter_id, Duration::from_secs(10)).await;
        assert_eq!(final_status.state, TaskState::Success);
        assert!(
            resumed.load(Ordering::SeqCst),
            "the waiter must have been resumed through the fan-out"
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(10), run_handle)
            .await
            .expect("run() must return once shutdown fires")
            .unwrap();
    }

    struct LatencyHandler {
        started_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    }

    #[async_trait]
    impl TypedTaskHandler for LatencyHandler {
        type Payload = serde_json::Value;
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            *self.started_at.lock().unwrap() = Some(std::time::Instant::now());
            Ok(TaskOutcome::Success { result_ref: None })
        }
    }

    /// Pickup must be bound by `BLPOP` wakeup, not a fixed 100ms poll. Averages several samples to
    /// cancel Postgres jitter while still separating instant wakeup from poll-bound pickup.
    #[tokio::test]
    async fn pickup_latency_is_not_bound_by_a_fixed_poll_interval() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("latency_{}", uuid::Uuid::new_v4());
        let started_at: Arc<std::sync::Mutex<Option<std::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(None));

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url)
            .register(
                task_type.as_str(),
                LatencyHandler {
                    started_at: started_at.clone(),
                },
            )
            .tier("fast", &[task_type.as_str()], 1)
            .build();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        // Let the loop open its BLPOP connection first so latency excludes connection setup.
        tokio::time::sleep(Duration::from_millis(200)).await;

        const SAMPLES: u32 = 5;
        let mut total = Duration::ZERO;
        for _ in 0..SAMPLES {
            *started_at.lock().unwrap() = None;

            conveyor
                .enqueue(EnqueueRequest {
                    task_type: task_type.as_str().into(),
                    payload: serde_json::Value::Null,
                    tenant_id: None,
                    entity_id: None,
                    resource_key: None,
                    resource_cap: None,
                })
                .await
                .unwrap();
            // Start the clock only after `enqueue()` returns, so enqueue latency isn't measured.
            let enqueued_at = std::time::Instant::now();

            let wait_start = std::time::Instant::now();
            loop {
                if started_at.lock().unwrap().is_some() {
                    break;
                }
                assert!(
                    wait_start.elapsed() < Duration::from_secs(5),
                    "handler never started"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            total += started_at
                .lock()
                .unwrap()
                .unwrap()
                .duration_since(enqueued_at);
        }

        let avg = total / SAMPLES;
        assert!(
            avg < Duration::from_millis(50),
            "average pickup latency {avg:?} over {SAMPLES} samples suggests poll-bound \
             dispatch, not real BLPOP wakeup"
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(10), run_handle)
            .await
            .expect("run() must return once shutdown fires")
            .unwrap();
    }

    #[tokio::test]
    async fn live_queue_depths_are_registry_independent() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey.clone(), redis_url).build();

        let task_type = format!("live_depth_{}", uuid::Uuid::new_v4());
        let key = queue::queue_key(&task_type);
        let mut conn = valkey.clone();
        for _ in 0..3 {
            redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, &key, "raw")
                .await
                .unwrap();
        }
        redis::AsyncCommands::lpush::<_, _, ()>(&mut conn, queue::DEAD_LETTER_QUEUE, "raw")
            .await
            .unwrap();

        let snapshot = conveyor.live().await.unwrap();

        let depth = snapshot
            .queue_depths
            .iter()
            .find(|(t, _)| t.0 == task_type)
            .map(|(_, d)| *d);
        assert_eq!(
            depth,
            Some(3),
            "queue depth must be visible even though nothing was registered: {:?}",
            snapshot.queue_depths
        );
        assert!(
            snapshot.queue_depths.iter().all(|(t, _)| t.0 != "dead"),
            "the dead-letter queue must be excluded from queue_depths"
        );
        assert!(
            snapshot.dead_letter_count >= 1,
            "the dead-letter queue must still be reflected in dead_letter_count"
        );

        let _: i64 = redis::AsyncCommands::del(&mut conn, &key).await.unwrap();
    }

    /// `CONVEYOR_LIVE_SNAPSHOT_LIMIT` is a process-global env var; held for the whole test.
    static LIVE_SNAPSHOT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn live_snapshot_is_capped_at_the_configured_limit() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _env_guard = LIVE_SNAPSHOT_ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_LIVE_SNAPSHOT_LIMIT", "1");

        insert_task_full(
            &db,
            InsertTask {
                task_type: "live_cap_a",
                entity_id: Some(unique_entity_id()),
                status: "running",
                ..Default::default()
            },
        )
        .await;
        insert_task_full(
            &db,
            InsertTask {
                task_type: "live_cap_b",
                entity_id: Some(unique_entity_id()),
                status: "running",
                ..Default::default()
            },
        )
        .await;

        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();
        let snapshot = conveyor.live().await.unwrap();
        std::env::remove_var("CONVEYOR_LIVE_SNAPSHOT_LIMIT");

        assert_eq!(
            snapshot.running.len(),
            1,
            "the running snapshot must be capped at the configured limit"
        );
    }

    #[tokio::test]
    async fn live_workers_populated_and_tolerant_of_garbage() {
        let Some((db, valkey, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor = ConveyorBuilder::new(db, valkey.clone(), redis_url).build();

        let valid_id = format!("live_wk_{}", uuid::Uuid::new_v4());
        let valid_key = format!("worker:{valid_id}");
        let garbage_key = format!("worker:live_wk_garbage_{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let valid_payload = serde_json::json!({
            "id": valid_id,
            "hostname": "test-host",
            "pid": 1,
            "started_at": now,
            "concurrency": 1,
            "last_heartbeat": now,
        })
        .to_string();

        let mut conn = valkey.clone();
        redis::AsyncCommands::set_ex::<_, _, ()>(&mut conn, &valid_key, valid_payload, 60)
            .await
            .unwrap();
        redis::AsyncCommands::set_ex::<_, _, ()>(&mut conn, &garbage_key, "not json", 60)
            .await
            .unwrap();

        let snapshot = conveyor.live().await.unwrap();

        let _: i64 = redis::AsyncCommands::del(&mut conn, &valid_key)
            .await
            .unwrap();
        let _: i64 = redis::AsyncCommands::del(&mut conn, &garbage_key)
            .await
            .unwrap();

        assert!(
            snapshot.workers.iter().any(|w| w.worker_id == valid_id),
            "the valid heartbeat must appear: {:?}",
            snapshot
                .workers
                .iter()
                .map(|w| &w.worker_id)
                .collect::<Vec<_>>()
        );
        assert!(
            snapshot
                .workers
                .iter()
                .all(|w| !w.worker_id.contains("garbage")),
            "the garbage key must be skipped, not surfaced"
        );
    }
}
