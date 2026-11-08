//! Per-user daily-digest send hour + IANA timezone, so each user receives the digest at a local
//! time rather than at a single fixed 08:00 UTC.
//!
//! Both columns are nullable rather than defaulted to 8/UTC — `NULL` is how the backend tells a
//! first-time enable from a return visit and requires the browser-detected timezone to be submitted
//! alongside `hour` on that first enable, instead of silently landing everyone on UTC.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE users \
             ADD COLUMN digest_hour SMALLINT, \
             ADD COLUMN digest_timezone TEXT;",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE users \
             DROP COLUMN IF EXISTS digest_hour, \
             DROP COLUMN IF EXISTS digest_timezone;",
        )
        .await?;

        Ok(())
    }
}
