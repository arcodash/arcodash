use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        arcodash_conveyor::schema::ConveyorNullEntityIndexes
            .up(manager)
            .await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        arcodash_conveyor::schema::ConveyorNullEntityIndexes
            .down(manager)
            .await
    }
}
