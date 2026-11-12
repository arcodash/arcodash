//! Schema checks for the metadata columns that point at externally stored customer data.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_migration::Migrator;
use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;

const ADDED_COLUMNS: [(&str, &str); 4] = [
    ("query_results", "last_used_at"),
    ("dashboard_narratives", "data_location"),
    ("mail_deliveries", "body_location"),
    ("data_sources", "ai_profile_values_location"),
];
/// Runs `body` against a fresh throwaway DB and drops it even when `body` panics.
async fn with_throwaway_db<F, Fut>(tag: &str, body: F)
where
    F: FnOnce(DatabaseConnection) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping external data migration test");
        return;
    };
    let (db, admin_url, db_name) = create_throwaway_db(&base_url, tag).await;
    let outcome = tokio::spawn(body(db)).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    if let Err(err) = outcome {
        std::panic::resume_unwind(err.into_panic());
    }
}

const NARRATIVE_COLUMNS: [&str; 3] = ["summary", "highlights", "digest"];
const INDEX: &str = "ix_query_results_last_used_at_external";

/// `(is_nullable, column_default.is_some())` or `None` when the column is absent.
async fn column(db: &DatabaseConnection, table: &str, col: &str) -> Option<(bool, bool)> {
    let row = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT is_nullable = 'YES' AS nullable, column_default IS NOT NULL AS has_default \
                 FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = '{table}' AND column_name = '{col}'"
            ),
        ))
        .await
        .unwrap()?;
    Some((
        row.try_get("", "nullable").unwrap(),
        row.try_get("", "has_default").unwrap(),
    ))
}

async fn index_exists(db: &DatabaseConnection) -> bool {
    db.query_one_raw(Statement::from_string(
        DbBackend::Postgres,
        format!("SELECT 1 AS x FROM pg_indexes WHERE indexname = '{INDEX}'"),
    ))
    .await
    .unwrap()
    .is_some()
}

async fn assert_up_state(db: &DatabaseConnection) {
    for (table, col) in ADDED_COLUMNS {
        assert!(
            column(db, table, col).await.is_some(),
            "{table}.{col} missing"
        );
    }
    assert_eq!(
        column(db, "query_results", "last_used_at").await,
        Some((false, true)),
        "last_used_at must be NOT NULL with a default"
    );
    for col in NARRATIVE_COLUMNS {
        let (nullable, _) = column(db, "dashboard_narratives", col).await.unwrap();
        assert!(nullable, "dashboard_narratives.{col} must be nullable");
    }
    assert!(index_exists(db).await, "partial index missing");
}

#[tokio::test]
async fn migration_adds_storage_columns() {
    with_throwaway_db("external_data", |db| async move {
        Migrator::up(&db, None).await.expect("up");
        assert_up_state(&db).await;

        Migrator::down(&db, Some(1)).await.expect("down");
        for (table, col) in ADDED_COLUMNS {
            assert!(
                column(&db, table, col).await.is_none(),
                "{table}.{col} survived down"
            );
        }
        assert!(!index_exists(&db).await, "index survived down");
        for col in NARRATIVE_COLUMNS {
            let (nullable, _) = column(&db, "dashboard_narratives", col).await.unwrap();
            assert!(
                !nullable,
                "dashboard_narratives.{col} must be NOT NULL after down"
            );
        }

        Migrator::up(&db, None).await.expect("re-up");
        assert_up_state(&db).await;
    })
    .await;
}

#[tokio::test]
async fn last_used_at_backfills_from_retrieved_at() {
    with_throwaway_db("external_data_bf", |db| async move {
        let total = u32::try_from(Migrator::migrations().len()).unwrap();
        Migrator::up(&db, Some(total - 1))
            .await
            .expect("up to previous");
        assert!(column(&db, "query_results", "last_used_at").await.is_none());

        // Raw SQL: entities carry columns the previous schema does not have yet.
        db.execute_unprepared(
            "INSERT INTO organizations (name, slug, settings) VALUES ('Org', 'org', '{}'); \
             INSERT INTO data_sources (org_id, name, type) \
             SELECT id, 'ds', 'pg' FROM organizations; \
             INSERT INTO query_results (org_id, data_source_id, query_hash, query, retrieved_at) \
             SELECT org_id, id, 'h', 'select 1', now() - interval '3 days' FROM data_sources",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.expect("up");
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT last_used_at = retrieved_at AS same, \
                 last_used_at < now() - interval '2 days' AS old FROM query_results"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(row.try_get::<bool>("", "same").unwrap());
        assert!(row.try_get::<bool>("", "old").unwrap());
    })
    .await;
}
