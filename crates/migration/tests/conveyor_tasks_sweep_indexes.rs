//! Asserts `m20260910_000002_conveyor_tasks_sweep_indexes` actually lands all three partial
//! indexes: the lease-expiry sweep, `fail_abandoned_pending`, and `requeue_stale_pending`
//! (`crates/conveyor/src/sweeper.rs`) each had no supporting index without them.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

#[tokio::test]
async fn conveyor_sweep_indexes_exist_at_head() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "[SKIP] TEST_DATABASE_URL not set — skipping conveyor sweep index migration test"
        );
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "conveyor_sweep_idx").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    arcodash_migration::Migrator::up(db, None).await?;

    for indexname in [
        "idx_conveyor_tasks_expired_lease",
        "idx_conveyor_tasks_abandoned_pending",
        "idx_conveyor_tasks_stale_pending",
    ] {
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
                format!("SELECT 1 FROM pg_indexes WHERE indexname = '{indexname}'"),
            ))
            .await?;
        assert!(
            row.is_some(),
            "{indexname} must exist after migrating to head"
        );
    }
    Ok(())
}
