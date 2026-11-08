//! AI narratives per dashboard: each row stores the generated summary plus the compacted data
//! digest it was derived from, so the next run can describe what changed.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS dashboard_narratives ( \
               id bigserial PRIMARY KEY, \
               org_id bigint NOT NULL REFERENCES organizations(id) ON DELETE CASCADE, \
               dashboard_id bigint NOT NULL REFERENCES dashboards(id) ON DELETE CASCADE, \
               user_id bigint REFERENCES users(id) ON DELETE SET NULL, \
               created_at timestamptz NOT NULL DEFAULT now(), \
               source text NOT NULL CHECK (source IN ('manual', 'digest')), \
               summary text NOT NULL, \
               highlights jsonb NOT NULL DEFAULT '[]'::jsonb, \
               digest jsonb NOT NULL, \
               confidence text NOT NULL CHECK (confidence IN ('low', 'medium', 'high')), \
               model text NOT NULL);",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_dashboard_narratives_dashboard_created \
             ON dashboard_narratives (dashboard_id, created_at DESC);",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS dashboard_narratives;")
            .await?;
        Ok(())
    }
}
