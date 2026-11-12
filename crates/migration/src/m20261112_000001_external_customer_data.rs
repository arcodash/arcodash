//! Metadata columns that point at customer data held in object storage.

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
                "ALTER TABLE query_results \
                   ADD COLUMN IF NOT EXISTS last_used_at timestamptz NOT NULL DEFAULT now(); \
                 UPDATE query_results SET last_used_at = retrieved_at; \
                 CREATE INDEX IF NOT EXISTS ix_query_results_last_used_at_external \
                   ON query_results (last_used_at) WHERE data IS NULL; \
                 ALTER TABLE dashboard_narratives \
                   ADD COLUMN IF NOT EXISTS data_location text, \
                   ALTER COLUMN summary DROP NOT NULL, \
                   ALTER COLUMN highlights DROP NOT NULL, \
                   ALTER COLUMN digest DROP NOT NULL; \
                 ALTER TABLE mail_deliveries ADD COLUMN IF NOT EXISTS body_location text; \
                 ALTER TABLE data_sources \
                   ADD COLUMN IF NOT EXISTS ai_profile_values_location text;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // SET NOT NULL fails if rows with NULLs were written since `up`.
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE data_sources DROP COLUMN IF EXISTS ai_profile_values_location; \
                 ALTER TABLE mail_deliveries DROP COLUMN IF EXISTS body_location; \
                 ALTER TABLE dashboard_narratives \
                   DROP COLUMN IF EXISTS data_location, \
                   ALTER COLUMN summary SET NOT NULL, \
                   ALTER COLUMN highlights SET NOT NULL, \
                   ALTER COLUMN digest SET NOT NULL; \
                 DROP INDEX IF EXISTS ix_query_results_last_used_at_external; \
                 ALTER TABLE query_results DROP COLUMN IF EXISTS last_used_at;",
            )
            .await?;
        Ok(())
    }
}
