//! Sets the default of `groups.data_source_access_restricted` to `true`, so an insert that omits
//! the column starts a new group with the narrower posture instead of org-wide data-source reach.
//!
//! Only the column default changes — existing rows are left as they are, since a backfill would
//! revoke data-source access from every group that inherited the previous default, with no way to
//! tell an inherited `false` from a chosen one.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE groups ALTER COLUMN data_source_access_restricted SET DEFAULT true;",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE groups ALTER COLUMN data_source_access_restricted SET DEFAULT false;",
        )
        .await?;
        Ok(())
    }
}
