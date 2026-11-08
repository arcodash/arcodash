use sea_orm_migration::prelude::*;

/// Backfills the dedicated snippet/destination permission keys onto existing orgs' builtin
/// Editor/Viewer roles: `view_snippets`, `manage_snippets` and `view_destinations`.
///
/// Editor gets all three; Viewer gets the two view keys only. Scoped to `type = 'builtin'` so a
/// custom role sharing the same name is untouched. Idempotent via dedup; `down()` removes the same
/// keys for reversibility.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        use sea_orm::{ConnectionTrait, Statement};
        let db = manager.get_connection();
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            UPDATE roles
            SET permissions = ARRAY(
                SELECT DISTINCT unnest(
                    permissions || ARRAY['view_snippets', 'manage_snippets', 'view_destinations']
                )
            )
            WHERE type = 'builtin' AND name = 'Editor';
            "
            .to_string(),
        ))
        .await?;
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            UPDATE roles
            SET permissions = ARRAY(
                SELECT DISTINCT unnest(
                    permissions || ARRAY['view_snippets', 'view_destinations']
                )
            )
            WHERE type = 'builtin' AND name = 'Viewer';
            "
            .to_string(),
        ))
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        use sea_orm::{ConnectionTrait, Statement};
        let db = manager.get_connection();
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            UPDATE roles
            SET permissions = ARRAY(
                SELECT unnest(permissions)
                EXCEPT
                SELECT unnest(ARRAY['view_snippets', 'manage_snippets', 'view_destinations'])
            )
            WHERE type = 'builtin' AND name = 'Editor';
            "
            .to_string(),
        ))
        .await?;
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            UPDATE roles
            SET permissions = ARRAY(
                SELECT unnest(permissions)
                EXCEPT
                SELECT unnest(ARRAY['view_snippets', 'view_destinations'])
            )
            WHERE type = 'builtin' AND name = 'Viewer';
            "
            .to_string(),
        ))
        .await?;
        Ok(())
    }
}
