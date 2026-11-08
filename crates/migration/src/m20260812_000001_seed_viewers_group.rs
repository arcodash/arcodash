use sea_orm_migration::prelude::*;

/// Backfills the builtin `viewers` group (→ builtin Viewer role) into every existing org — the
/// only reachable way to place a user in a read-only role.
///
/// This migration must NOT: modify any row's `users.groups` (nobody is demoted or promoted by
/// running it); modify or delete any existing `group_roles` row (`default`→Editor survives
/// verbatim); touch a pre-existing non-builtin group already named `viewers` in some org; or
/// invent a Viewer role for an org that doesn't already have one — such an org is skipped.
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
            INSERT INTO groups (org_id, name, permissions, type)
            SELECT r.org_id, 'viewers', r.permissions, 'builtin'
            FROM roles r
            WHERE r.type = 'builtin' AND r.name = 'Viewer'
              AND NOT EXISTS (
                  SELECT 1 FROM groups g WHERE g.org_id = r.org_id AND g.name = 'viewers'
              );
            "
            .to_string(),
        ))
        .await?;

        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            INSERT INTO group_roles (group_id, role_id)
            SELECT g.id, r.id
            FROM groups g
            JOIN roles r ON r.org_id = g.org_id AND r.type = 'builtin' AND r.name = 'Viewer'
            WHERE g.name = 'viewers' AND g.type = 'builtin'
              AND NOT EXISTS (
                  SELECT 1 FROM group_roles gr WHERE gr.group_id = g.id AND gr.role_id = r.id
              );
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
            DELETE FROM group_roles
            WHERE group_id IN (SELECT id FROM groups WHERE type = 'builtin' AND name = 'viewers');
            "
            .to_string(),
        ))
        .await?;

        // users.groups is a plain bigint[] with no FK, so a dangling id would otherwise linger
        // for anyone placed in the group — strip it from every user before the group row goes.
        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            UPDATE users u
            SET groups = array_remove(u.groups, g.id)
            FROM groups g
            WHERE g.org_id = u.org_id AND g.type = 'builtin' AND g.name = 'viewers'
              AND g.id = ANY(u.groups);
            "
            .to_string(),
        ))
        .await?;

        db.execute_raw(Statement::from_string(
            db.get_database_backend(),
            r"
            DELETE FROM groups WHERE type = 'builtin' AND name = 'viewers';
            "
            .to_string(),
        ))
        .await?;

        Ok(())
    }
}
