use sea_orm_migration::prelude::*;

/// Removes `view_users` from the builtin Viewer role, which would otherwise grant Viewer read
/// access to the full org user directory.
///
/// Scoped to `type = 'builtin' AND name = 'Viewer'` so a custom role sharing the same name is
/// untouched. Idempotent via `EXCEPT`; `down()` restores the key for reversibility.
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
                SELECT unnest(permissions)
                EXCEPT
                SELECT unnest(ARRAY['view_users'])
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
                SELECT DISTINCT unnest(
                    permissions || ARRAY['view_users']
                )
            )
            WHERE type = 'builtin' AND name = 'Viewer';
            "
            .to_string(),
        ))
        .await?;
        Ok(())
    }
}
