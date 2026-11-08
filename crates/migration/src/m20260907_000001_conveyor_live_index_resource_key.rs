use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        arcodash_conveyor::schema::ConveyorLiveIndexResourceKey
            .up(manager)
            .await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        arcodash_conveyor::schema::ConveyorLiveIndexResourceKey
            .down(manager)
            .await
    }
}
