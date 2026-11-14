//! `queries.verified_at` / `verified_by` — an admin's attestation that a query is trustworthy.
//!
//! Independent of the publish lifecycle: toggling it neither bumps `working_version` nor writes a
//! `changes` row. `verified_by` is nulled if the verifying user is deleted.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE queries \
             ADD COLUMN IF NOT EXISTS verified_at TIMESTAMPTZ, \
             ADD COLUMN IF NOT EXISTS verified_by BIGINT REFERENCES users(id) ON DELETE SET NULL;",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE queries DROP COLUMN IF EXISTS verified_at, DROP COLUMN IF EXISTS verified_by;",
        )
        .await?;
        Ok(())
    }
}
