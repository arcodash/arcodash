//! Records why the last AI profile job failed as a whole, so the UI can surface it.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE data_sources \
                 ADD COLUMN IF NOT EXISTS ai_profile_last_error text;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE data_sources \
                 DROP COLUMN IF EXISTS ai_profile_last_error;",
            )
            .await?;
        Ok(())
    }
}
