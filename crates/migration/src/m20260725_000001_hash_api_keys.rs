//! Replaces the plaintext `users.api_key` and `api_keys.api_key` columns with SHA-256 digests, so
//! a database read can no longer yield a usable credential.
//!
//! Existing keys keep working: every row's digest is backfilled from its current plaintext value
//! before that column is dropped, and the app hashes the presented key before looking it up
//! (`arcodash_core::hash::hash_api_key`, which must produce the same hex as the `encode(sha256(...))`
//! expression used here). What changes is that a key can no longer be *displayed* after creation —
//! the value is unrecoverable once hashed, so the UI shows it once at generation time.
//!
//! `queries.api_key` is intentionally untouched: it is a non-authenticating uniqueness token, never
//! accepted as a credential.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

/// Each table holding an API key, with the name of its unique index on the plaintext column.
const TABLES: [(&str, &str); 2] = [
    ("users", "idx_users_api_key"),
    ("api_keys", "idx_api_keys_api_key"),
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        for (table, index) in TABLES {
            db.execute_unprepared(&format!(
                "ALTER TABLE public.{table} ADD COLUMN IF NOT EXISTS api_key_hash text"
            ))
            .await?;
            // `sha256()` is core Postgres (11+); no pgcrypto extension required.
            let result = db
                .execute_unprepared(&format!(
                    "UPDATE public.{table} SET api_key_hash = encode(sha256(api_key::bytea), 'hex') \
                     WHERE api_key_hash IS NULL"
                ))
                .await?;
            println!(
                "hash_api_keys: backfilled {} row(s) in {table}",
                result.rows_affected()
            );
            db.execute_unprepared(&format!(
                "ALTER TABLE public.{table} ALTER COLUMN api_key_hash SET NOT NULL"
            ))
            .await?;
            db.execute_unprepared(&format!("DROP INDEX IF EXISTS public.{index}"))
                .await?;
            db.execute_unprepared(&format!(
                "CREATE UNIQUE INDEX {index}_hash ON public.{table} USING btree (api_key_hash)"
            ))
            .await?;
            db.execute_unprepared(&format!("ALTER TABLE public.{table} DROP COLUMN api_key"))
                .await?;
        }

        Ok(())
    }

    /// Restores the column and its unique index, but NOT the key values — a digest cannot be
    /// reversed. Rolling back therefore leaves every row with a placeholder key that authenticates
    /// nobody; each user must regenerate.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if std::env::var("ARCODASH_ALLOW_DESTRUCTIVE_DOWN").as_deref() != Ok("1") {
            return Err(DbErr::Custom(
                "rolling this back invalidates every API key (digests cannot be reversed); \
                 set ARCODASH_ALLOW_DESTRUCTIVE_DOWN=1 to proceed and have every key regenerated"
                    .into(),
            ));
        }
        let db = manager.get_connection();

        for (table, index) in TABLES {
            db.execute_unprepared(&format!(
                "ALTER TABLE public.{table} ADD COLUMN IF NOT EXISTS api_key text"
            ))
            .await?;
            db.execute_unprepared(&format!(
                "UPDATE public.{table} SET api_key = \
                 encode(sha256((random()::text || id::text)::bytea), 'hex') \
                 WHERE api_key IS NULL"
            ))
            .await?;
            db.execute_unprepared(&format!(
                "ALTER TABLE public.{table} ALTER COLUMN api_key SET NOT NULL"
            ))
            .await?;
            db.execute_unprepared(&format!("DROP INDEX IF EXISTS public.{index}_hash"))
                .await?;
            db.execute_unprepared(&format!(
                "CREATE UNIQUE INDEX {index} ON public.{table} USING btree (api_key)"
            ))
            .await?;
            db.execute_unprepared(&format!(
                "ALTER TABLE public.{table} DROP COLUMN api_key_hash"
            ))
            .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    use super::Migration;

    #[tokio::test]
    async fn down_refuses_without_the_destructive_env_var() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        std::env::remove_var("ARCODASH_ALLOW_DESTRUCTIVE_DOWN");
        let conn = arcodash_core::db::connect(&database_url).await.unwrap();
        let manager = SchemaManager::new(&conn);
        let err = Migration
            .down(&manager)
            .await
            .expect_err("down() must refuse to run without the env var set");
        assert!(err.to_string().contains("ARCODASH_ALLOW_DESTRUCTIVE_DOWN"));
    }
}
