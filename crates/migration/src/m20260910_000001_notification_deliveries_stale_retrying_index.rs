//! Indexes the `retrying` branch of the stale-notification sweep.
//!
//! `requeue_stale_pending_notifications` (`apps/worker/src/scheduler/sweeper.rs`) sweeps
//! `notification_deliveries` for both `status = 'pending'` (covered by the baseline's
//! `idx_notification_deliveries_stale_pending`) and `status = 'retrying'` staled off
//! `COALESCE(started_at, created_at)`.
//!
//! Nothing indexed the `retrying` branch, so that half of the sweep's `WHERE` clause scanned the
//! whole table every cycle. Mirrors the baseline index's shape: partial on the status it serves,
//! ordered so the sweep's `< now() - interval` predicate can range-seek.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notification_deliveries_stale_retrying \
             ON notification_deliveries (started_at, created_at) \
             WHERE status = 'retrying';",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_notification_deliveries_stale_retrying;")
            .await?;
        Ok(())
    }
}
