//! `ai_usage`: one row per real AI-provider call (Gemini/OpenAI/Anthropic), for org-scoped usage
//! metering.
//!
//! `feature` is stored (not hardcoded to text-to-SQL) so a future AI feature doesn't need a schema
//! change. Raw rows are never exposed to any client — only aggregate rollups read this table (see
//! `ai_usage.rs`'s admin endpoint); this migration only creates the storage.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AiUsage::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AiUsage::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(AiUsage::OrgId).big_integer().not_null())
                    .col(ColumnDef::new(AiUsage::Provider).text().not_null())
                    .col(ColumnDef::new(AiUsage::Model).text().not_null())
                    .col(ColumnDef::new(AiUsage::Feature).text().not_null())
                    .col(ColumnDef::new(AiUsage::Status).text().not_null())
                    .col(ColumnDef::new(AiUsage::TokensIn).big_integer())
                    .col(ColumnDef::new(AiUsage::TokensOut).big_integer())
                    .col(
                        ColumnDef::new(AiUsage::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_ai_usage_org")
                            .from(AiUsage::Table, AiUsage::OrgId)
                            .to(Organizations::Table, Organizations::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_ai_usage_org_created_at")
                    .if_not_exists()
                    .table(AiUsage::Table)
                    .col(AiUsage::OrgId)
                    .col(AiUsage::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AiUsage::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum AiUsage {
    Table,
    Id,
    OrgId,
    Provider,
    Model,
    Feature,
    Status,
    TokensIn,
    TokensOut,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Organizations {
    Table,
    Id,
}
