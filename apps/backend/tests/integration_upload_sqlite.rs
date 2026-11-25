//! SQLite upload read-only proof: a SQLite file uploaded through the real
//! HTTP endpoint must be queryable for reads but reject any write — the connector opens its
//! cache file `.read_only(true).immutable(true)` (see `sqlite_connector.rs`). A passing INSERT/
//! UPDATE here would be a security regression (an editor of a "read-only" file source could
//! otherwise corrupt the uploaded database on disk).

mod common;
use common::default_org_id;
use common::spawn_app;

use arcodash_core::connectors::sqlite_connector::SqliteConnector;
use arcodash_core::connectors::QueryRunner;
use arcodash_core::storage::ObjectStore;

/// Builds a real SQLite database file (via sqlx, already a workspace dep) with one seeded row,
/// returns its bytes.
async fn build_sqlite_bytes() -> Vec<u8> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    let path = std::env::temp_dir().join(format!(
        "upload-sqlite-test-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    {
        let mut conn = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("create test sqlite db");
        sqlx::query("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, n INTEGER)")
            .execute(&mut conn)
            .await
            .expect("create table");
        sqlx::query("INSERT INTO items (name, n) VALUES ('widget', 7)")
            .execute(&mut conn)
            .await
            .expect("seed row");
    }
    let bytes = tokio::fs::read(&path).await.expect("read test db");
    let _ = tokio::fs::remove_file(&path).await;
    bytes
}

#[tokio::test]
async fn sqlite_upload_read_only_enforced() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let bytes = build_sqlite_bytes().await;

    // 1. Upload via the real multipart endpoint — magic-byte + open validation must pass.
    let form = reqwest::multipart::Form::new().text("type", "sqlite").part(
        "file",
        reqwest::multipart::Part::bytes(bytes)
            .file_name("data.sqlite")
            .mime_str("application/octet-stream")
            .unwrap(),
    );
    let resp = client
        .post(format!("{}/api/data_sources/uploads", app.address))
        .multipart(form)
        .send()
        .await
        .expect("upload request failed");
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status.as_u16(), 200, "sqlite upload response: {body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let upload_id = body["upload_id"]
        .as_str()
        .expect("upload_id present")
        .to_string();

    // 2. Create the data source referencing the upload.
    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "sqlite-readonly-ds",
            "type": "sqlite",
            "options": { "upload_id": upload_id }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "create data source failed");

    // 3. Read query succeeds through the real connector (same instantiation path
    // `ConnectorRegistry::with_db` builds, just constructed directly since no worker process
    // runs here — matches `integration_upload_roundtrip.rs`'s CSV proof and the established
    // `integration_query_results_connector.rs` precedent).
    let store = ObjectStore::from_env().expect("object store from env");
    let connector = SqliteConnector::new(
        app.db.clone(),
        std::sync::Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            store.clone(),
        )),
    );
    let org_id = default_org_id(&app.db).await;
    let opts = serde_json::json!({ "upload_id": upload_id, "org_id": org_id });

    let read = connector
        .run_query(&opts, "SELECT id, name, n FROM items ORDER BY id")
        .await
        .expect("read query must succeed against a read-only-opened database");
    assert_eq!(read.rows.len(), 1);
    assert_eq!(read.rows[0]["name"], "widget");
    assert_eq!(read.rows[0]["n"], 7);

    // An aggregate has no declared column type at all (no direct table-column reference) — prove
    // it decodes correctly through the real upload path, not just a hand-built file.
    let aggregate = connector
        .run_query(
            &opts,
            "SELECT count(*) AS cnt, sum(n) AS total_n FROM items",
        )
        .await
        .expect("aggregate query must decode through the real upload path");
    assert_eq!(aggregate.rows[0]["cnt"], 1);
    assert_eq!(aggregate.rows[0]["total_n"], 7);

    // 4. Write queries MUST fail — the load-bearing assertion. A passing INSERT/UPDATE here
    // would mean the connector opened the cache file writable, defeating the whole point of the
    // read-only/immutable open.
    let insert_err = connector
        .run_query(&opts, "INSERT INTO items (name, n) VALUES ('evil', 1)")
        .await
        .expect_err("INSERT against an uploaded SQLite file must fail — read-only enforcement");
    eprintln!("sqlite: INSERT correctly rejected: {insert_err:#}");

    let update_err = connector
        .run_query(&opts, "UPDATE items SET n = 99 WHERE id = 1")
        .await
        .expect_err("UPDATE against an uploaded SQLite file must fail — read-only enforcement");
    eprintln!("sqlite: UPDATE correctly rejected: {update_err:#}");

    // Confirm the row genuinely never changed (belt-and-suspenders on top of the Err checks).
    let reread = connector
        .run_query(&opts, "SELECT n FROM items WHERE id = 1")
        .await
        .expect("re-read after failed writes must still succeed");
    assert_eq!(
        reread.rows[0]["n"], 7,
        "a failed write must never have touched the underlying data"
    );

    // 5. A foreign org_id must never be able to read this upload — the connector's own tenant
    // boundary, defense-in-depth on top of the backend's pre-flight data-source-access check.
    let foreign_opts = serde_json::json!({ "upload_id": upload_id, "org_id": org_id + 1 });
    let foreign_err = connector
        .run_query(&foreign_opts, "SELECT 1")
        .await
        .expect_err("a foreign org_id must not be able to read this upload");
    assert!(
        foreign_err.to_string().contains("not found"),
        "must fail as not-found, not leak a different error: {foreign_err:#}"
    );
}

#[tokio::test]
async fn missing_sqlite_upload_reports_file_unavailable() {
    use arcodash_core::entities::data_source_upload;
    use sea_orm::EntityTrait;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let form = reqwest::multipart::Form::new().text("type", "sqlite").part(
        "file",
        reqwest::multipart::Part::bytes(build_sqlite_bytes().await)
            .file_name("gone.sqlite")
            .mime_str("application/octet-stream")
            .unwrap(),
    );
    let resp = client
        .post(format!("{}/api/data_sources/uploads", app.address))
        .multipart(form)
        .send()
        .await
        .expect("upload request failed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let upload_id = body["upload_id"].as_str().unwrap().to_string();
    let row = data_source_upload::Entity::find_by_id(uuid::Uuid::parse_str(&upload_id).unwrap())
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let store = ObjectStore::from_env().expect("object store from env");
    store
        .delete(&format!("orgs/{}/uploads/{}", row.org_id, row.object_key))
        .await
        .unwrap();

    let connector = SqliteConnector::new(
        app.db.clone(),
        std::sync::Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            store,
        )),
    );
    let opts = serde_json::json!({ "upload_id": upload_id, "org_id": row.org_id });
    let err = connector.run_query(&opts, "SELECT 1").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("file unavailable, upload again"),
        "{err:#}"
    );
}
