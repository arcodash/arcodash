//! Asserts `m20260910_000001_notification_deliveries_stale_retrying_index` actually lands: the
//! sweeper's `retrying` branch (`apps/worker/src/scheduler/sweeper.rs`,
//! `requeue_stale_pending_notifications`) has no supporting index without it.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

#[tokio::test]
async fn stale_retrying_index_exists_at_head() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "[SKIP] TEST_DATABASE_URL not set — skipping stale-retrying index migration test"
        );
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "stale_retrying_idx").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    arcodash_migration::Migrator::up(db, None).await?;

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM pg_indexes WHERE indexname = 'idx_notification_deliveries_stale_retrying'"
                .to_string(),
        ))
        .await?;
    assert!(
        row.is_some(),
        "idx_notification_deliveries_stale_retrying must exist after migrating to head"
    );
    Ok(())
}
