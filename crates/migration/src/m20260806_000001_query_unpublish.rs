//! `queries.unpublished_at` — a query returned to draft (`POST /queries/<id>/unpublish`) without
//! losing its publish history.
//!
//! `published_change_id` intentionally stays set (it still answers "has this query ever been
//! published", which version restore depends on); this column answers the separate "is it live
//! right now" question. NULL means live (or never published); set means an editor explicitly took
//! it back to draft.
//!
//! Dashboards need no equivalent column — `dashboards.published_snapshot` is a full rebuildable
//! snapshot rather than a set of columns kept forever, so a dashboard unpublish just clears it back
//! to NULL, identical to "never published" with no separate history to preserve.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE queries ADD COLUMN unpublished_at TIMESTAMPTZ;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE queries DROP COLUMN IF EXISTS unpublished_at;")
            .await?;
        Ok(())
    }
}
