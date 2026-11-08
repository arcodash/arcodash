use sea_orm_migration::prelude::*;

/// Makes `ai_usage.org_id` and `data_source_uploads.org_id` cascade on org delete, matching every
/// other org-scoped FK in the schema (both tables were created with `NO ACTION`).
///
/// The application purges both tables explicitly before deleting an org (to record deletion-log row
/// counts); the cascade means the delete succeeds even without that purge.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // Each ADD CONSTRAINT below takes ACCESS EXCLUSIVE on its table for a full validating
        // scan — on a production-sized `ai_usage` that must fail fast and let an operator retry
        // in a quiet window, not queue behind (and block) every writer for however long the scan
        // takes.
        db.execute_unprepared("SET LOCAL lock_timeout = '5s';")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE public.ai_usage DROP CONSTRAINT IF EXISTS fk_ai_usage_org;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.ai_usage \
             ADD CONSTRAINT fk_ai_usage_org FOREIGN KEY (org_id) \
             REFERENCES public.organizations(id) ON DELETE CASCADE;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.data_source_uploads DROP CONSTRAINT IF EXISTS fk_data_source_uploads_org;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.data_source_uploads \
             ADD CONSTRAINT fk_data_source_uploads_org FOREIGN KEY (org_id) \
             REFERENCES public.organizations(id) ON DELETE CASCADE;",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE public.ai_usage DROP CONSTRAINT IF EXISTS fk_ai_usage_org;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.ai_usage \
             ADD CONSTRAINT fk_ai_usage_org FOREIGN KEY (org_id) \
             REFERENCES public.organizations(id);",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.data_source_uploads DROP CONSTRAINT IF EXISTS fk_data_source_uploads_org;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.data_source_uploads \
             ADD CONSTRAINT fk_data_source_uploads_org FOREIGN KEY (org_id) \
             REFERENCES public.organizations(id);",
        )
        .await?;
        Ok(())
    }
}
