//! `users.session_token_version` — bumped by `reset_password`/`change_password` on every credential
//! change.
//!
//! The session cookie's JWT carries the version it was issued under (`SessionClaims.ver`);
//! `resolve_from_session_cookie` requires it to match the row's current value, so a stolen or
//! previously-issued cookie stops resolving the instant the password changes, without needing a
//! server-side session table to revoke against.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE users ADD COLUMN session_token_version INTEGER NOT NULL DEFAULT 0;",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE users DROP COLUMN IF EXISTS session_token_version;")
            .await?;
        Ok(())
    }
}
