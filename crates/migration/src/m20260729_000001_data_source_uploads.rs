//! `data_source_uploads`: metadata for CSV/SQLite files backing a file-upload data source.
//!
//! The actual bytes live in object storage (`ObjectStore`, `crates/shared/src/storage.rs`) at
//! `object_key`; this row is what a connector resolves an `upload_id` to. `data_source_id` starts
//! NULL (set once the upload is linked to a saved data source) and is cleared (not cascaded) when
//! that data source is deleted, so the worker's orphan sweep can find and reclaim the object.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DataSourceUploads::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DataSourceUploads::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DataSourceUploads::OrgId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DataSourceUploads::ObjectKey)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DataSourceUploads::Filename)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DataSourceUploads::ByteSize)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DataSourceUploads::ContentType)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(DataSourceUploads::CreatedBy).big_integer())
                    .col(ColumnDef::new(DataSourceUploads::DataSourceId).big_integer())
                    .col(
                        ColumnDef::new(DataSourceUploads::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_data_source_uploads_org")
                            .from(DataSourceUploads::Table, DataSourceUploads::OrgId)
                            .to(Organizations::Table, Organizations::Id),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_data_source_uploads_created_by")
                            .from(DataSourceUploads::Table, DataSourceUploads::CreatedBy)
                            .to(Users::Table, Users::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_data_source_uploads_data_source")
                            .from(DataSourceUploads::Table, DataSourceUploads::DataSourceId)
                            .to(DataSources::Table, DataSources::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_data_source_uploads_org")
                    .if_not_exists()
                    .table(DataSourceUploads::Table)
                    .col(DataSourceUploads::OrgId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_data_source_uploads_data_source")
                    .if_not_exists()
                    .table(DataSourceUploads::Table)
                    .col(DataSourceUploads::DataSourceId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_data_source_uploads_created_at")
                    .if_not_exists()
                    .table(DataSourceUploads::Table)
                    .col(DataSourceUploads::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(DataSourceUploads::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum DataSourceUploads {
    Table,
    Id,
    OrgId,
    ObjectKey,
    Filename,
    ByteSize,
    ContentType,
    CreatedBy,
    DataSourceId,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Organizations {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum DataSources {
    Table,
    Id,
}
