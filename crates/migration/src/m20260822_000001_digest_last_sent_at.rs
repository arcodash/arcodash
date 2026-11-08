//! `users.digest_last_sent_at` — watermark for the daily digest send window.
//!
//! The digest reports
//! monitor state changes since this timestamp (capped at 24h), not always the last 24 hours, so
//! an already-reported change isn't re-sent on every send within the same day.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE users ADD COLUMN digest_last_sent_at TIMESTAMPTZ NULL;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE users DROP COLUMN IF EXISTS digest_last_sent_at;")
            .await?;
        Ok(())
    }
}
