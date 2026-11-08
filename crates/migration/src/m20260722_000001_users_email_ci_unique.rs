//! Adds a case-insensitive uniqueness guard on `(org_id, lower(email))`.
//!
//! With it, `User@X.com` and `user@x.com` collide instead of creating two rows for the same person
//! (SSO logins vary case depending on IdP/browser).
//!
//! The existing plain `idx_users_org_id_email` (`org_id`, email) index is deliberately KEPT: it is
//! the conflict-arbiter target for the `ON CONFLICT (org_id, email)` upserts several call sites
//! use (sea-orm `OnConflict::columns([OrgId, Email])` can only name plain columns, not a
//! functional index). The new functional index is strictly stricter, so it subsumes exact-dup
//! enforcement while also blocking case-variant duplicates.
//!
//! Deployments with pre-existing case-variant duplicate emails (both `a@b.com` and `A@b.com`
//! under one org) must dedup before this runs — the `CREATE UNIQUE INDEX` fails loudly on any
//! such collision.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE UNIQUE INDEX idx_users_org_id_email_ci ON public.users (org_id, lower(email));",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_users_org_id_email_ci;")
            .await?;
        Ok(())
    }
}
