//! Three sweeps over `conveyor_tasks` had no supporting index, so each full-table-scanned every
//! cycle:
//! - the lease-expiry sweep (`crates/conveyor/src/sweeper.rs`, every cycle) filters
//!   `status = 'running' AND lease_expires_at < now() AND waiting_on_task_id IS NULL` — the only
//!   existing live-row index (`idx_conveyor_tasks_live`) is ordered `(status, resource_key,
//!   lease_expires_at)` for a resource-cap check, so within the `running` partition it is sorted
//!   by `resource_key` first and can't range-seek on `lease_expires_at`;
//! - `fail_abandoned_pending` filters `status = 'pending' AND enqueued_at IS NOT NULL AND
//!   enqueued_at < now() - interval`;
//! - `requeue_stale_pending` filters `status = 'pending' AND created_at < now() - interval`.
//!
//! Each gets its own narrow partial index rather than reordering `idx_conveyor_tasks_live`,
//! which would fix the lease sweep but pessimize the resource-cap check it was ordered for. All
//! three predicates match only live/recent rows, so the indexes stay small.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_conveyor_tasks_expired_lease \
             ON conveyor_tasks (lease_expires_at) \
             WHERE status = 'running' AND waiting_on_task_id IS NULL;",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_conveyor_tasks_abandoned_pending \
             ON conveyor_tasks (enqueued_at) \
             WHERE status = 'pending';",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_conveyor_tasks_stale_pending \
             ON conveyor_tasks (created_at) \
             WHERE status = 'pending';",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_conveyor_tasks_expired_lease;")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_conveyor_tasks_abandoned_pending;")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_conveyor_tasks_stale_pending;")
            .await?;
        Ok(())
    }
}
