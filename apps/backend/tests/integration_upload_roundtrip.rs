//! Full file-upload round trip through the real HTTP handlers: multipart upload → object lands
//! in the (fake-gcs-server-backed) object store → `POST /api/data_sources` links it → schema
//! refresh → query execution returns the uploaded file's actual rows. Proves the upload_id →
//! object_key → connector chain end to end, not just its individual pieces.
//!
//! Requires the docker-compose.testing.yml stack (`postgres`, `gcs`) plus
//! `TEST_DATABASE_URL`/`TEST_REDIS_URL`/`STORAGE_EMULATOR_HOST`/`UPLOAD_BUCKET` exported —
//! skips cleanly via `skip_if_none!` otherwise.

mod common;
use common::spawn_app;

use arcodash_core::connectors::http_connectors::CsvConnector;
use arcodash_core::connectors::QueryRunner;
use arcodash_core::storage::ObjectStore;

const SAMPLE_CSV: &[u8] = b"name,age\nalice,30\nbob,40\n";
const AGG_CSV: &[u8] =
    b"name,department,salary\nalice,eng,120000\nbob,eng,110000\ncarol,sales,90000\n";

#[tokio::test]
async fn csv_upload_create_schema_and_query_round_trip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    // 1. Upload the CSV via the real multipart endpoint.
    let form = reqwest::multipart::Form::new().text("type", "csv").part(
        "file",
        reqwest::multipart::Part::bytes(SAMPLE_CSV.to_vec())
            .file_name("data.csv")
            .mime_str("text/csv")
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
    assert_eq!(status.as_u16(), 200, "upload response: {body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();

    let upload_id = body["upload_id"]
        .as_str()
        .expect("upload_id present")
        .to_string();
    assert_eq!(body["filename"], "data.csv");
    let columns = body["columns"].as_array().expect("columns array");
    assert!(
        columns.iter().any(|c| c["name"] == "name"),
        "columns must include 'name': {columns:?}"
    );
    assert!(
        columns.iter().any(|c| c["name"] == "age"),
        "columns must include 'age': {columns:?}"
    );
    let preview_rows = body["preview_rows"].as_array().expect("preview_rows array");
    assert_eq!(preview_rows.len(), 2, "preview must show both data rows");
    // The preview is built straight from the uploaded bytes (not from a second read of the
    // just-inserted `data_source_upload` row through a separate DB connection, which can't see the
    // request's own uncommitted per-request transaction), so it must be populated with no warning
    // on a first, clean upload.
    assert!(
        body.get("warning").is_none(),
        "a fresh, valid upload must not carry a preview warning: {body:?}"
    );

    // 2. The object actually landed in the object store, and the upload row is unlinked so far.
    use arcodash_core::entities::data_source_upload;
    use sea_orm::EntityTrait;
    let upload_uuid = uuid::Uuid::parse_str(&upload_id).unwrap();
    let upload_row = data_source_upload::Entity::find_by_id(upload_uuid)
        .one(&app.db)
        .await
        .unwrap()
        .expect("upload row must exist");
    assert!(
        upload_row.data_source_id.is_none(),
        "upload must be unlinked before the data source is created"
    );

    let store = ObjectStore::from_env().expect("object store from env");
    let stored_bytes = store
        .get(&upload_row.object_key)
        .await
        .expect("uploaded object must be fetchable from the object store");
    assert_eq!(
        stored_bytes.as_ref(),
        SAMPLE_CSV,
        "stored object bytes must match what was uploaded"
    );

    // 3. Create the data source referencing the upload_id → the upload row must get linked.
    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "csv-roundtrip-ds",
            "type": "csv",
            "options": { "upload_id": upload_id, "delimiter": "," }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create data source: {body_text}");
    let ds: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let reloaded_upload = data_source_upload::Entity::find_by_id(upload_uuid)
        .one(&app.db)
        .await
        .unwrap()
        .expect("upload row must still exist");
    assert_eq!(
        reloaded_upload.data_source_id,
        Some(ds_id),
        "upload row must be linked to the created data source"
    );

    // 4. GET schema?refresh=true only queues the worker job and answers from the (empty) cache.
    let pending_key = arcodash_core::models::ai_profile_pending_key(ds_id);
    redis::AsyncCommands::del::<_, ()>(&mut app.redis.clone(), &pending_key)
        .await
        .unwrap();
    let resp = client
        .get(format!(
            "{}/api/data_sources/{ds_id}/schema?refresh=true",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["refreshing"], true, "{body}");
    assert_eq!(body["schema"]["tables"], serde_json::json!([]), "{body}");
    let queued: Vec<String> =
        redis::AsyncCommands::lrange(&mut app.redis.clone(), "arcodash:queue:schemas", 0, -1)
            .await
            .unwrap();
    assert!(
        queued.iter().any(|j| {
            let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
            v["type"] == "ProfileDataSource"
                && v["data_source_id"].as_i64() == Some(ds_id)
                && v["data"] == "changed"
        }),
        "a ProfileDataSource(changed) job must be queued for the data source"
    );
    redis::AsyncCommands::del::<_, ()>(&mut app.redis.clone(), &pending_key)
        .await
        .unwrap();

    // 5. Execute real SQL through the real connector (the same code the worker's ExecuteQuery
    // job would run) — no live worker process is spun up here, so this calls the connector
    // directly rather than through the async job queue, same precedent as
    // `integration_query_results_connector.rs`'s direct-connector proofs.
    // `org_id` mirrors what `query_results::inject_caller` adds to `opts` before any real caller
    // invokes this connector — the org filter added to `CsvConnector::fetch` requires it.
    let connector = CsvConnector::new(app.db.clone(), store.clone());
    let org_id = upload_row.org_id;
    let opts = serde_json::json!({ "upload_id": upload_id, "delimiter": ",", "org_id": org_id });

    // The submitted SQL — not a raw file dump — must drive the result.
    let one = connector
        .run_query(&opts, "SELECT 1 AS x")
        .await
        .expect("csv run_query failed");
    assert_eq!(one.rows.len(), 1);
    assert_eq!(one.columns.len(), 1);
    assert_eq!(one.rows[0]["x"], 1);

    // A numeric predicate is the affinity trap the no-declared-column-type decision exists for:
    // an all-TEXT column would compare `age > 35` as TEXT vs INTEGER, where every INTEGER sorts
    // below every TEXT, so the predicate would silently match every row instead of just bob's.
    let predicate = connector
        .run_query(&opts, "SELECT name FROM data WHERE age > 35")
        .await
        .expect("numeric predicate query failed");
    assert_eq!(
        predicate.rows.len(),
        1,
        "only bob (40) is over 35: {:?}",
        predicate.rows
    );
    assert_eq!(predicate.rows[0]["name"], "bob");

    // Invalid SQL must surface as a real error, not a silent success.
    connector
        .run_query(&opts, "SELECT * FROM nope")
        .await
        .expect_err("querying a table that doesn't exist must fail");

    // Two runs against the same data source must return identical results — cache reuse is not
    // observable as a behaviour change.
    let first_count = connector
        .run_query(&opts, "SELECT count(*) AS n FROM data")
        .await
        .expect("first count query failed");
    let second_count = connector
        .run_query(&opts, "SELECT count(*) AS n FROM data")
        .await
        .expect("second count query failed");
    assert_eq!(first_count.rows[0]["n"], 2);
    assert_eq!(first_count.rows[0]["n"], second_count.rows[0]["n"]);

    // Changing the delimiter for the same upload_id must yield a differently-parsed table — the
    // cache-key guard. If the delimiter weren't part of the cache key, this would
    // wrongly reuse the ','-parsed, 2-column materialization instead of building a fresh one.
    let semicolon_opts =
        serde_json::json!({ "upload_id": upload_id, "delimiter": ";", "org_id": org_id });
    let semicolon_result = connector
        .run_query(&semicolon_opts, "SELECT * FROM data")
        .await
        .expect("semicolon-delimited materialization must succeed");
    assert_eq!(
        semicolon_result.columns.len(),
        1,
        "with ';' as the delimiter, the whole comma-joined line is one column: {:?}",
        semicolon_result.columns
    );

    // 6. A group-by aggregate against a richer second upload returns the grouped/aggregated
    // rows, not a raw dump.
    let agg_form = reqwest::multipart::Form::new().text("type", "csv").part(
        "file",
        reqwest::multipart::Part::bytes(AGG_CSV.to_vec())
            .file_name("agg_data.csv")
            .mime_str("text/csv")
            .unwrap(),
    );
    let agg_resp = client
        .post(format!("{}/api/data_sources/uploads", app.address))
        .multipart(agg_form)
        .send()
        .await
        .expect("agg upload request failed");
    assert_eq!(agg_resp.status().as_u16(), 200);
    let agg_body: serde_json::Value = agg_resp.json().await.unwrap();
    let agg_upload_id = agg_body["upload_id"].as_str().unwrap().to_string();
    let agg_opts =
        serde_json::json!({ "upload_id": agg_upload_id, "delimiter": ",", "org_id": org_id });
    let grouped = connector
        .run_query(
            &agg_opts,
            "SELECT department, count(*) AS n, avg(salary) AS avg_salary FROM agg_data \
             GROUP BY department ORDER BY n DESC",
        )
        .await
        .expect("grouped aggregate query failed");
    assert_eq!(
        grouped.rows.len(),
        2,
        "one row per department: {:?}",
        grouped.rows
    );
    assert_eq!(grouped.rows[0]["department"], "eng");
    assert_eq!(grouped.rows[0]["n"], 2);
    assert_eq!(grouped.rows[0]["avg_salary"], 115000.0);
    assert_eq!(grouped.rows[1]["department"], "sales");
    assert_eq!(grouped.rows[1]["n"], 1);
    assert_eq!(grouped.rows[1]["avg_salary"], 90000.0);

    // 7. A foreign org_id must never see this upload — the connector's own tenant boundary,
    // defense-in-depth on top of the backend's pre-flight data-source-access check.
    let foreign_opts =
        serde_json::json!({ "upload_id": upload_id, "delimiter": ",", "org_id": org_id + 1 });
    let foreign_err = connector
        .run_query(&foreign_opts, "SELECT 1")
        .await
        .expect_err("a foreign org_id must not be able to read this upload");
    assert!(
        foreign_err.to_string().contains("not found"),
        "must fail as not-found, not leak a different error: {foreign_err:#}"
    );

    eprintln!(
        "roundtrip: linked ds {ds_id}, grouped rows {}",
        grouped.rows.len()
    );
}

/// A materialization failure (here: a duplicate CSV header, which `CREATE TABLE` rejects) must
/// not leave its half-built temp file behind in the upload cache directory — that directory has
/// no eviction, so a leaked file per bad attempt is an unbounded disk leak.
#[tokio::test]
async fn a_materialization_failure_leaves_no_temp_file_behind() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let cache_dir = std::env::temp_dir().join(format!(
        "arcodash-uploads-leak-test-{}",
        uuid::Uuid::new_v4()
    ));
    // SAFETY: this process's env, mutated only for the duration of this test and cleared before
    // returning; the crate's integration suite always runs with `--test-threads=1`.
    unsafe {
        std::env::set_var("UPLOAD_CACHE_DIR", &cache_dir);
    }

    let client = app.admin_client().await;
    let dup_header_csv: &[u8] = b"id,id\n1,2\n";
    let form = reqwest::multipart::Form::new().text("type", "csv").part(
        "file",
        reqwest::multipart::Part::bytes(dup_header_csv.to_vec())
            .file_name("dup.csv")
            .mime_str("text/csv")
            .unwrap(),
    );
    let resp = client
        .post(format!("{}/api/data_sources/uploads", app.address))
        .multipart(form)
        .send()
        .await
        .expect("upload request failed");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "upload itself must still succeed"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    let upload_id = body["upload_id"].as_str().unwrap().to_string();

    use arcodash_core::entities::data_source_upload;
    use sea_orm::EntityTrait;
    let upload_uuid = uuid::Uuid::parse_str(&upload_id).unwrap();
    let org_id = data_source_upload::Entity::find_by_id(upload_uuid)
        .one(&app.db)
        .await
        .unwrap()
        .expect("upload row must exist")
        .org_id;

    let store = ObjectStore::from_env().expect("object store from env");
    let connector = CsvConnector::new(app.db.clone(), store);
    let opts = serde_json::json!({ "upload_id": upload_id, "delimiter": ",", "org_id": org_id });
    connector
        .run_query(&opts, "SELECT 1")
        .await
        .expect_err("a duplicate CSV header must fail materialization, not silently dedupe");

    let mut entries = tokio::fs::read_dir(&cache_dir)
        .await
        .expect("cache dir must have been created even though the build inside it failed");
    let mut leftover = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        leftover.push(entry.file_name());
    }

    unsafe {
        std::env::remove_var("UPLOAD_CACHE_DIR");
    }
    let _ = tokio::fs::remove_dir_all(&cache_dir).await;

    assert!(
        leftover.is_empty(),
        "a failed materialization must not leave a temp file behind: {leftover:?}"
    );
}
