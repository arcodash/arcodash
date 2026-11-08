//! Two independent additions:
//!
//! - `data_sources.health_state`/`health_error`/`health_checked_at` — a second axis alongside
//!   `paused_at` (On/Off). Health is never polled; it's written reactively by a query run
//!   connecting/failing to connect (`apps/worker/src/handlers/execute_query.rs`) or a
//!   create/update connectivity probe (`apps/backend/src/services/data_source_service.rs`).
//!   `health_state` defaults to `'unknown'` — a data source that has not been health-checked
//!   yet — and otherwise holds `'ok'`/`'failing'`.
//! - `monitors.unevaluable_reason` — why the most recent check couldn't resolve a condition
//!   (e.g. the configured column is missing from the result), set only while
//!   `state = 'unevaluable'` and cleared on any other transition.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE data_sources \
             ADD COLUMN health_state TEXT NOT NULL DEFAULT 'unknown', \
             ADD COLUMN health_error TEXT, \
             ADD COLUMN health_checked_at TIMESTAMPTZ;",
        )
        .await?;

        db.execute_unprepared("ALTER TABLE monitors ADD COLUMN unevaluable_reason TEXT;")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE data_sources \
             DROP COLUMN IF EXISTS health_state, \
             DROP COLUMN IF EXISTS health_error, \
             DROP COLUMN IF EXISTS health_checked_at;",
        )
        .await?;

        db.execute_unprepared("ALTER TABLE monitors DROP COLUMN IF EXISTS unevaluable_reason;")
            .await?;

        Ok(())
    }
}
