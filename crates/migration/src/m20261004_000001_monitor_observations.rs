//! Per-check observation history for monitors. `run_id` / `query_result_id` carry no FK: runs are
//! pruned and query results replaced, while observations outlive both.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS monitor_observations ( \
               id bigserial PRIMARY KEY, \
               org_id bigint NOT NULL REFERENCES organizations(id) ON DELETE CASCADE, \
               monitor_id bigint NOT NULL REFERENCES monitors(id) ON DELETE CASCADE, \
               observed_at timestamptz NOT NULL DEFAULT now(), \
               value double precision, \
               state text NOT NULL, \
               run_id bigint, \
               query_result_id bigint, \
               explanation text);",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_monitor_observations_monitor_observed \
             ON monitor_observations (monitor_id, observed_at DESC);",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS monitor_observations;")
            .await?;
        Ok(())
    }
}
