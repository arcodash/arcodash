//! `m20260730_000001_schedule_entity_cleanup`'s `up()` uses `CREATE OR REPLACE` for its function
//! and both triggers, so a retry that reruns `up()` from the start after a partial failure
//! succeeds. Re-invoking the migration's own `up()` against a database that already has the
//! function and triggers must succeed instead of erroring "already exists".

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use sea_orm_migration::sea_orm::DatabaseConnection;
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

#[tokio::test]
async fn recreating_the_trigger_function_does_not_error() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping schedule-cleanup idempotency test");
        return;
    };

    let (db, admin_url, db_name) =
        create_throwaway_db(&base_url, "schedule_cleanup_idempotent").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    arcodash_migration::Migrator::up(db, None).await?;

    // Re-run the migration's own `up()` directly (bypassing `Migrator`'s applied-migrations
    // tracking, which would otherwise just skip it) against a database that already has the
    // function and triggers — this must succeed, not error, so a retry after a partial failure
    // is safe.
    let manager = SchemaManager::new(db);
    arcodash_migration::m20260730_000001_schedule_entity_cleanup::Migration
        .up(&manager)
        .await?;

    Ok(())
}
