use sea_orm_migration::sea_orm::{ConnectionTrait, Database, DatabaseConnection};

/// Creates a uniquely-named database on `TEST_DATABASE_URL`'s server. Returns its connection,
/// the admin URL and the db name, for [`drop_throwaway_db`].
pub async fn create_throwaway_db(
    base_url: &str,
    tag: &str,
) -> (DatabaseConnection, String, String) {
    let (prefix, _) = base_url
        .rsplit_once('/')
        .expect("TEST_DATABASE_URL must be a postgres:// URL with a /<dbname> path segment");
    let admin_url = format!("{prefix}/postgres");
    // The counter keeps names unique when concurrent tests read the same coarse clock.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let db_name = format!(
        "arcodash_{tag}_test_{}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let admin_db = Database::connect(&admin_url)
        .await
        .expect("connect to admin (postgres) db");
    admin_db
        .execute_unprepared(&format!("CREATE DATABASE {db_name}"))
        .await
        .expect("create throwaway db");
    let db = Database::connect(&format!("{prefix}/{db_name}"))
        .await
        .expect("connect to throwaway db");
    (db, admin_url, db_name)
}

pub async fn drop_throwaway_db(admin_url: &str, db_name: &str) {
    if let Ok(admin_db) = Database::connect(admin_url).await {
        let _ = admin_db
            .execute_unprepared(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .await;
    }
}
