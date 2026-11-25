//! `query_results.data_sha256` — hex SHA-256 of the exact stored object bytes, checked before an
//! object read from storage is decoded. Nullable: rows written before the digest existed have none.

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
                "ALTER TABLE query_results ADD COLUMN IF NOT EXISTS data_sha256 TEXT;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE query_results DROP COLUMN IF EXISTS data_sha256;")
            .await?;
        Ok(())
    }
}
