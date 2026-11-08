//! `schedules.entity_id` is polymorphic (no FK to `queries`/`monitors`), so any path that removes a
//! query or monitor row without an explicit app-side delete leaks a `schedules` row.
//!
//! This adds an `AFTER DELETE` trigger on both tables that removes the matching schedule, covering
//! every current and future cascade into `queries`/`monitors`, not just the ones the application
//! code deletes schedules for explicitly.
//!
//! `SECURITY INVOKER` (the default — not specified) is deliberate: the trigger runs as whatever
//! role performed the DELETE on `queries`/`monitors`, so on a restricted app-role connection it is
//! subject to the same RLS policy on `schedules` as any other statement that role runs — and since
//! the deleted entity and its schedule always share `org_id`, the policy that allowed the entity
//! delete also allows the schedule delete.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "CREATE OR REPLACE FUNCTION delete_entity_schedules() RETURNS trigger AS $$ \
             BEGIN \
               DELETE FROM schedules WHERE entity_type = TG_ARGV[0] AND entity_id = OLD.id; \
               RETURN OLD; \
             END $$ LANGUAGE plpgsql \
             SET search_path = pg_catalog, public;",
        )
        .await?;

        db.execute_unprepared(
            "CREATE OR REPLACE TRIGGER queries_delete_schedules AFTER DELETE ON queries \
             FOR EACH ROW EXECUTE FUNCTION delete_entity_schedules('query');",
        )
        .await?;

        db.execute_unprepared(
            "CREATE OR REPLACE TRIGGER monitors_delete_schedules AFTER DELETE ON monitors \
             FOR EACH ROW EXECUTE FUNCTION delete_entity_schedules('monitor');",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared("DROP TRIGGER IF EXISTS queries_delete_schedules ON queries;")
            .await?;
        db.execute_unprepared("DROP TRIGGER IF EXISTS monitors_delete_schedules ON monitors;")
            .await?;
        db.execute_unprepared("DROP FUNCTION IF EXISTS delete_entity_schedules();")
            .await?;

        Ok(())
    }
}
