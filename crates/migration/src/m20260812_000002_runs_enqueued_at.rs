//! `runs.enqueued_at` — set the moment a `pending` run actually lands on a queue (mirrors
//! `conveyor_tasks.enqueued_at`, which already exists).
//!
//! Cap-blocked runs deliberately stay `pending` with this left `NULL` — the concurrency-cap count
//! needs to tell "queued, waiting for a worker to pick it up" apart from "not yet queued at all,
//! still parked over the cap", and `status` alone can't make that distinction. Additive, nullable,
//! no backfill: existing rows simply read as never-enqueued, which is harmless — the sweeper's
//! `requeue_stale_pending` re-derives the real state on its next pass regardless.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE runs ADD COLUMN enqueued_at TIMESTAMPTZ;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE runs DROP COLUMN IF EXISTS enqueued_at;")
            .await?;
        Ok(())
    }
}
