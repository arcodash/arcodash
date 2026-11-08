//! Adds the sampled-data AI profile to `data_sources`; kept out of `schema`, whose raw shape
//! reaches the frontend unparsed.

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
                 ADD COLUMN IF NOT EXISTS ai_profile jsonb, \
                 ADD COLUMN IF NOT EXISTS ai_profile_built_at timestamptz;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE data_sources \
                 DROP COLUMN IF EXISTS ai_profile, \
                 DROP COLUMN IF EXISTS ai_profile_built_at;",
            )
            .await?;
        Ok(())
    }
}
