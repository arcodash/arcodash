//! `query_results.data_sha256` is a nullable text column added by the digest migration.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_migration::Migrator;
use sea_orm_migration::sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;

#[tokio::test]
async fn data_sha256_is_nullable_text_and_reversible() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping digest migration test");
        return;
    };
    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "qr_digest").await;
    let outcome = tokio::spawn(async move {
        let probe = || {
            Statement::from_string(
                DbBackend::Postgres,
                "SELECT data_type, is_nullable FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = 'query_results' \
                 AND column_name = 'data_sha256'"
                    .to_string(),
            )
        };
        Migrator::up(&db, None).await.unwrap();
        let row = db
            .query_one_raw(probe())
            .await
            .unwrap()
            .expect("column missing");
        assert_eq!(row.try_get::<String>("", "data_type").unwrap(), "text");
        assert_eq!(row.try_get::<String>("", "is_nullable").unwrap(), "YES");
        Migrator::down(&db, Some(1)).await.unwrap();
        assert!(db.query_one_raw(probe()).await.unwrap().is_none());
        Migrator::up(&db, None).await.unwrap();
        assert!(db.query_one_raw(probe()).await.unwrap().is_some());
    })
    .await;
    drop_throwaway_db(&admin_url, &db_name).await;
    if let Err(err) = outcome {
        std::panic::resume_unwind(err.into_panic());
    }
}
