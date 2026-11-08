//! Schema definitions a host migrator applies; also applied by this crate's own test harness.
//!
//! Conveyor's own schema: `conveyor_tasks` + `conveyor_schedules`. `CreateConveyorTables` is the
//! single source of truth for this DDL — applied both by this crate's own test harness
//! (`Migrator::up`, tracked in `conveyor_seaql_migrations`) and, delegated from
//! `crates/migration`, by the host application's real migrator. Every `create_index` call below
//! is `.if_not_exists()` so both migrators can apply it against the same database without either
//! erroring on the other's already-created index.

// `async_trait`'s `MigrationTrait` impls can't name `SchemaManager`'s lifetime (E0195).
#![allow(elided_lifetimes_in_paths)]

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(CreateConveyorTables),
            Box::new(CreateTaskStatsHourly),
            Box::new(ConveyorNullEntityIndexes),
            Box::new(ConveyorLiveIndexResourceKey),
            Box::new(ConveyorDropJobId),
        ]
    }

    // A shared `TEST_DATABASE_URL` already carries the host application's own migration
    // history in the default `seaql_migrations` table; tracking this crate's one migration in
    // that same table makes `up()` reject every row it doesn't recognize as "missing". A
    // distinctly-named table keeps this crate's bookkeeping (one row, idempotent) fully separate.
    fn migration_table_name() -> sea_orm::DynIden {
        Alias::new("conveyor_seaql_migrations").into_iden()
    }
}

pub struct CreateConveyorTables;

impl MigrationName for CreateConveyorTables {
    fn name(&self) -> &'static str {
        "m_create_conveyor_tables"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateConveyorTables {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ConveyorTasks::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ConveyorTasks::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ConveyorTasks::TenantId).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::TaskType).text().not_null())
                    .col(ColumnDef::new(ConveyorTasks::EntityId).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::ScheduleId).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::Payload).json_binary())
                    .col(ColumnDef::new(ConveyorTasks::ResourceKey).text())
                    .col(ColumnDef::new(ConveyorTasks::ResourceCap).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::ScheduledFor).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorTasks::TriggeredBy).text().not_null())
                    .col(
                        ColumnDef::new(ConveyorTasks::Status)
                            .text()
                            .not_null()
                            .default("pending"),
                    )
                    .col(
                        ColumnDef::new(ConveyorTasks::Attempt)
                            .integer()
                            .not_null()
                            .default(1),
                    )
                    .col(ColumnDef::new(ConveyorTasks::WaitingOnTaskId).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::JobId).text())
                    .col(ColumnDef::new(ConveyorTasks::WorkerId).text())
                    .col(ColumnDef::new(ConveyorTasks::LeaseExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorTasks::EnqueuedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorTasks::StartedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorTasks::FinishedAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorTasks::DurationMs).big_integer())
                    .col(ColumnDef::new(ConveyorTasks::Error).text())
                    .col(ColumnDef::new(ConveyorTasks::ResultRef).json_binary())
                    .col(
                        ColumnDef::new(ConveyorTasks::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // Fire-freely: entity_id nullable means NULL rows never collide in this partial index.
        manager
            .create_index(
                Index::create()
                    .name("uq_conveyor_tasks_inflight")
                    .if_not_exists()
                    .table(ConveyorTasks::Table)
                    .col(ConveyorTasks::TaskType)
                    .col(ConveyorTasks::EntityId)
                    .unique()
                    .and_where(Expr::col(ConveyorTasks::Status).is_in(["pending", "running"]))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("uq_conveyor_tasks_entity_tick")
                    .if_not_exists()
                    .table(ConveyorTasks::Table)
                    .col(ConveyorTasks::TaskType)
                    .col(ConveyorTasks::EntityId)
                    .col(ConveyorTasks::ScheduledFor)
                    .unique()
                    .and_where(Expr::col(ConveyorTasks::ScheduleId).is_not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_conveyor_tasks_live")
                    .if_not_exists()
                    .table(ConveyorTasks::Table)
                    .col(ConveyorTasks::Status)
                    .col(ConveyorTasks::LeaseExpiresAt)
                    .and_where(Expr::col(ConveyorTasks::Status).is_in(["pending", "running"]))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_conveyor_tasks_waiting")
                    .if_not_exists()
                    .table(ConveyorTasks::Table)
                    .col(ConveyorTasks::WaitingOnTaskId)
                    .and_where(Expr::col(ConveyorTasks::WaitingOnTaskId).is_not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ConveyorSchedules::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ConveyorSchedules::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ConveyorSchedules::TenantId).big_integer())
                    .col(
                        ColumnDef::new(ConveyorSchedules::TaskType)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ConveyorSchedules::EntityId).big_integer())
                    .col(
                        ColumnDef::new(ConveyorSchedules::Payload)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ConveyorSchedules::Spec)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ConveyorSchedules::Timezone)
                            .text()
                            .not_null()
                            .default("UTC"),
                    )
                    .col(
                        ColumnDef::new(ConveyorSchedules::Enabled)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(ColumnDef::new(ConveyorSchedules::PausedUntil).timestamp_with_time_zone())
                    .col(ColumnDef::new(ConveyorSchedules::AutoPausedAt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(ConveyorSchedules::OverlapPolicy)
                            .text()
                            .not_null()
                            .default("skip"),
                    )
                    .col(ColumnDef::new(ConveyorSchedules::NextRunAt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(ConveyorSchedules::ConsecutiveFailures)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(ColumnDef::new(ConveyorSchedules::ResourceKey).text())
                    .col(ColumnDef::new(ConveyorSchedules::ResourceCap).big_integer())
                    .col(
                        ColumnDef::new(ConveyorSchedules::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(ConveyorSchedules::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("uq_conveyor_schedules_entity")
                    .if_not_exists()
                    .table(ConveyorSchedules::Table)
                    .col(ConveyorSchedules::TaskType)
                    .col(ConveyorSchedules::EntityId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_conveyor_schedules_deadline")
                    .if_not_exists()
                    .table(ConveyorSchedules::Table)
                    .col(ConveyorSchedules::NextRunAt)
                    .and_where(Expr::col(ConveyorSchedules::Enabled).eq(true))
                    .and_where(Expr::col(ConveyorSchedules::AutoPausedAt).is_null())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ConveyorTasks::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ConveyorSchedules::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum ConveyorTasks {
    Table,
    Id,
    TenantId,
    TaskType,
    EntityId,
    ScheduleId,
    Payload,
    ResourceKey,
    ResourceCap,
    ScheduledFor,
    TriggeredBy,
    Status,
    Attempt,
    WaitingOnTaskId,
    JobId,
    WorkerId,
    LeaseExpiresAt,
    EnqueuedAt,
    StartedAt,
    FinishedAt,
    DurationMs,
    Error,
    ResultRef,
    CreatedAt,
}

#[derive(DeriveIden)]
enum ConveyorSchedules {
    Table,
    Id,
    TenantId,
    TaskType,
    EntityId,
    Payload,
    Spec,
    Timezone,
    Enabled,
    PausedUntil,
    AutoPausedAt,
    OverlapPolicy,
    NextRunAt,
    ConsecutiveFailures,
    ResourceKey,
    ResourceCap,
    CreatedAt,
    UpdatedAt,
}

pub struct CreateTaskStatsHourly;

impl MigrationName for CreateTaskStatsHourly {
    fn name(&self) -> &'static str {
        "m_create_task_stats_hourly"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateTaskStatsHourly {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(TaskStatsHourly::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TaskStatsHourly::Bucket)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TaskStatsHourly::TaskType).text().not_null())
                    .col(
                        ColumnDef::new(TaskStatsHourly::Succeeded)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::Failed)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::TimedOut)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::Cancelled)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::Skipped)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::Retries)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::P50Ms)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::P95Ms)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::P99Ms)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::MaxMs)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::AvgQueueLatencyMs)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TaskStatsHourly::QueueDepthSample)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .primary_key(
                        Index::create()
                            .col(TaskStatsHourly::Bucket)
                            .col(TaskStatsHourly::TaskType),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(TaskStatsHourly::Table).to_owned())
            .await
    }
}

/// Indexes that keep a NULL `entity_id` from bypassing schedule overlap and upsert checks.
///
/// A NULL `entity_id` never collides in a plain unique index, so `uq_conveyor_schedules_entity`
/// would let a caller register the same entity-less `task_type` more than once, and `plan_tick`'s
/// `entity_id = $2` overlap check (`schedule_id IS NOT NULL AND entity_id = NULL` never matches)
/// would let it run concurrently with itself.
///
/// This migration keys on `schedule_id` and an expression index instead of the nullable
/// `entity_id`, so a NULL is not a wildcard against overlap enforcement or upsert idempotency. See
/// `scheduler.rs::plan_tick` and `conveyor.rs::upsert_schedule` for the read/write sides that key
/// on these indexes.
pub struct ConveyorNullEntityIndexes;

impl MigrationName for ConveyorNullEntityIndexes {
    fn name(&self) -> &'static str {
        "m_conveyor_null_entity_indexes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ConveyorNullEntityIndexes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Collapse entity-less schedule duplicates onto the oldest row before the expression
        // index below can enforce one schedule per (task_type, NULL entity_id).
        db.execute_unprepared(
            "DELETE FROM conveyor_schedules a \
             WHERE a.entity_id IS NULL \
               AND EXISTS (SELECT 1 FROM conveyor_schedules b \
                            WHERE b.entity_id IS NULL AND b.task_type = a.task_type AND b.id < a.id)",
        )
        .await?;

        db.execute_unprepared("DROP INDEX IF EXISTS uq_conveyor_schedules_entity")
            .await?;
        // -1 is the entity-less bucket: entity ids in this codebase come from positive serial
        // sequences, so it can never collide with a genuine one.
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_conveyor_schedules_entity \
             ON conveyor_schedules (task_type, COALESCE(entity_id, -1))",
        )
        .await?;

        // At most one in-flight task per schedule — terminalize the surplus so the new index can
        // be built.
        db.execute_unprepared(
            "UPDATE conveyor_tasks a \
                SET status = 'cancelled', finished_at = now(), lease_expires_at = NULL \
              WHERE a.schedule_id IS NOT NULL AND a.status IN ('pending', 'running') \
                AND EXISTS (SELECT 1 FROM conveyor_tasks b \
                             WHERE b.schedule_id = a.schedule_id AND b.status IN ('pending', 'running') \
                               AND b.id < a.id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_conveyor_tasks_schedule_inflight \
             ON conveyor_tasks (schedule_id) \
             WHERE schedule_id IS NOT NULL AND status IN ('pending', 'running')",
        )
        .await?;

        // One task row per (schedule, tick) regardless of status — drop exact duplicates before
        // the tick index below can enforce it.
        db.execute_unprepared(
            "DELETE FROM conveyor_tasks a \
              WHERE a.schedule_id IS NOT NULL \
                AND EXISTS (SELECT 1 FROM conveyor_tasks b \
                             WHERE b.schedule_id = a.schedule_id AND b.scheduled_for = a.scheduled_for \
                               AND b.id < a.id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_conveyor_tasks_schedule_tick \
             ON conveyor_tasks (schedule_id, scheduled_for) \
             WHERE schedule_id IS NOT NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS uq_conveyor_tasks_schedule_tick")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS uq_conveyor_tasks_schedule_inflight")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS uq_conveyor_schedules_entity")
            .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_conveyor_schedules_entity \
             ON conveyor_schedules (task_type, entity_id)",
        )
        .await?;
        Ok(())
    }
}

/// `idx_conveyor_tasks_live` covers `(status, lease_expires_at, resource_key)`.
///
/// Including `resource_key` lets the planner use this index for the resource-cap check
/// (`resource.rs::count_running`, filtered on `resource_key` + status) instead of scanning every
/// live row. Uses the same DROP+CREATE-IF-NOT-EXISTS pattern as `ConveyorNullEntityIndexes`, safe
/// to re-run.
pub struct ConveyorLiveIndexResourceKey;

impl MigrationName for ConveyorLiveIndexResourceKey {
    fn name(&self) -> &'static str {
        "m_conveyor_live_index_resource_key"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ConveyorLiveIndexResourceKey {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_conveyor_tasks_live")
            .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_conveyor_tasks_live \
             ON conveyor_tasks (status, resource_key, lease_expires_at) \
             WHERE status IN ('pending', 'running')",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_conveyor_tasks_live")
            .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_conveyor_tasks_live \
             ON conveyor_tasks (status, lease_expires_at) \
             WHERE status IN ('pending', 'running')",
        )
        .await?;
        Ok(())
    }
}

/// `job_id` is never populated or read anywhere in this crate or any host application (only
/// `worker_id` is written, at `lifecycle::start`) — dead weight on every row.
///
/// Applied after `CreateConveyorTables`, which still creates the column on a fresh database, so
/// both a fresh migrate and an incremental one converge on the same final schema. `down` re-adds it
/// nullable so a rollback can't fail against existing rows.
pub struct ConveyorDropJobId;

impl MigrationName for ConveyorDropJobId {
    fn name(&self) -> &'static str {
        "m_conveyor_drop_job_id"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ConveyorDropJobId {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE conveyor_tasks DROP COLUMN IF EXISTS job_id")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE conveyor_tasks ADD COLUMN IF NOT EXISTS job_id text")
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum TaskStatsHourly {
    Table,
    Bucket,
    TaskType,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Skipped,
    Retries,
    P50Ms,
    P95Ms,
    P99Ms,
    MaxMs,
    AvgQueueLatencyMs,
    QueueDepthSample,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use sea_orm::{DbBackend, Statement};
    use sea_orm_migration::SchemaManager;

    /// `setup()` already runs the full `Migrator` (including both migrations below) once; a
    /// second `up()` against that already-migrated database must still be a no-op, not an error —
    /// the same guarantee `conveyor_null_entity_indexes_migration_is_idempotent`
    /// (`scheduler.rs`) checks for the earlier migration.
    #[tokio::test]
    async fn conveyor_live_index_resource_key_migration_is_idempotent() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let manager = SchemaManager::new(&db);
        ConveyorLiveIndexResourceKey.up(&manager).await.unwrap();
        ConveyorLiveIndexResourceKey
            .up(&manager)
            .await
            .expect("a second up() against an already-migrated database must not error");
    }

    #[tokio::test]
    async fn conveyor_drop_job_id_migration_is_idempotent_and_the_column_is_actually_gone() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let manager = SchemaManager::new(&db);
        ConveyorDropJobId.up(&manager).await.unwrap();
        ConveyorDropJobId
            .up(&manager)
            .await
            .expect("a second up() against an already-migrated database must not error");

        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*) AS c FROM information_schema.columns \
                 WHERE table_name = 'conveyor_tasks' AND column_name = 'job_id'",
            ))
            .await
            .unwrap()
            .unwrap();
        let c: i64 = row.try_get("", "c").unwrap();
        assert_eq!(c, 0, "job_id must actually be gone from the table");
    }
}
