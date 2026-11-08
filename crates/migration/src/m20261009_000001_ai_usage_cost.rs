//! Attributes each AI call to a user and records its USD cost for budget enforcement.

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
                "ALTER TABLE ai_usage \
                 ADD COLUMN IF NOT EXISTS user_id bigint NULL, \
                 ADD COLUMN IF NOT EXISTS cost_usd double precision NULL;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE ai_usage \
                 DROP COLUMN IF EXISTS user_id, \
                 DROP COLUMN IF EXISTS cost_usd;",
            )
            .await?;
        Ok(())
    }
}
