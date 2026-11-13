//! A manual run is refused up front while the org's storage is switching (409) or unreachable
//! (503), and no run row is left behind.

mod common;
use common::{ok_json, spawn_app_with_storage};

use arcodash_core::entities::run;
use arcodash_core::org_storage::StorageMode;
use arcodash_core::test_support::blob::{MemoryBlobStore, StaticResolver};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use std::sync::Arc;

fn resolver(build: impl FnOnce(StaticResolver) -> StaticResolver) -> Arc<StaticResolver> {
    Arc::new(build(StaticResolver::with_store(
        StorageMode::ArcoDash,
        Arc::new(MemoryBlobStore::default()),
        "orgs/test/",
    )))
}

/// Posts `/run` for a fresh query and returns `(status, body, query_id)`.
async fn manual_run(
    storage: Arc<StaticResolver>,
) -> Option<(
    u16,
    serde_json::Value,
    i64,
    sea_orm::DatabaseConnection,
    Arc<StaticResolver>,
)> {
    let app = spawn_app_with_storage(storage.clone()).await?;
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "storage-err-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let query = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds["id"].as_i64().unwrap(),
                "name": "Storage Err Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    let resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Some((status, body, query_id, app.db.clone(), storage))
}

async fn run_rows(db: &sea_orm::DatabaseConnection, query_id: i64) -> u64 {
    run::Entity::find()
        .filter(run::Column::EntityType.eq("query"))
        .filter(run::Column::EntityId.eq(query_id))
        .count(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn manual_run_during_switch_returns_409() {
    for storage in [
        resolver(StaticResolver::paused),
        resolver(StaticResolver::switching),
    ] {
        let (status, body, query_id, db, _) = skip_if_none!(manual_run(storage).await);
        assert_eq!(status, 409, "{body:?}");
        assert_eq!(body["error_code"], "storage_switching", "{body:?}");
        assert_eq!(
            run_rows(&db, query_id).await,
            0,
            "no run row may be created"
        );
    }
}

#[tokio::test]
async fn manual_run_with_unreachable_storage_returns_503() {
    let (status, body, query_id, db, storage) =
        skip_if_none!(manual_run(resolver(StaticResolver::unreachable)).await);
    assert_eq!(status, 503, "{body:?}");
    assert_eq!(body["error_code"], "storage_unreachable", "{body:?}");
    assert_eq!(run_rows(&db, query_id).await, 0);
    assert_eq!(storage.failures(), 1, "report_failure must be called once");
}

async fn queued_and_job_keys(redis: &redis::aio::ConnectionManager) -> (i64, usize) {
    let mut conn = redis.clone();
    let mut queued = 0;
    for q in arcodash_core::queue::QueueName::ALL {
        let n: i64 = redis::cmd("LLEN")
            .arg(q.queue_key())
            .query_async(&mut conn)
            .await
            .unwrap();
        queued += n;
    }
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("job:*")
        .query_async(&mut conn)
        .await
        .unwrap();
    (queued, keys.len())
}

/// A resolver over `store` whose writes are paused, plus a ready one over the same store for seeding.
fn paused_and_ready() -> (Arc<StaticResolver>, StaticResolver) {
    let store = Arc::new(MemoryBlobStore::default());
    let build = || StaticResolver::with_store(StorageMode::ArcoDash, store.clone(), "orgs/test/");
    (Arc::new(build().paused()), build())
}

async fn seed_ds_and_query(client: &reqwest::Client, address: &str) -> (i64, i64, String) {
    let ds = ok_json(
        client
            .post(format!("{address}/api/data_sources"))
            .json(&serde_json::json!({ "name": "exec-switch-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let query = ok_json(
        client
            .post(format!("{address}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Exec Switch Query",
                "query": "SELECT 1",
                "options": {},
                "is_draft": false
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    (
        ds_id,
        query["id"].as_i64().unwrap(),
        query["query_hash"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn execute_during_switch_returns_409_without_enqueueing() {
    let (paused, _) = paused_and_ready();
    let app = skip_if_none!(spawn_app_with_storage(paused).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_, query_id, _) = seed_ds_and_query(&client, &app.address).await;
    let before = queued_and_job_keys(&app.redis).await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 0 }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();

    assert_eq!(status, 409, "{body:?}");
    assert_eq!(body["error_code"], "storage_switching", "{body:?}");
    assert_eq!(
        queued_and_job_keys(&app.redis).await,
        before,
        "no job status or queue message may be created"
    );
}

#[tokio::test]
async fn execute_cache_hit_is_served_during_switch() {
    let (paused, ready) = paused_and_ready();
    let app = skip_if_none!(spawn_app_with_storage(paused).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = common::default_org_id(&app.db).await;
    let (ds_id, query_id, hash) = seed_ds_and_query(&client, &app.address).await;

    let data = serde_json::json!({
        "columns": [{ "name": "id", "friendly_name": "ID" }],
        "rows": [{ "id": 1 }]
    });
    let storage = arcodash_core::org_storage::OrgStorageResolver::resolve(&ready, &app.db, org_id)
        .await
        .unwrap();
    let location = arcodash_core::result_store::save_result(&storage, ds_id, &hash, &data)
        .await
        .unwrap()
        .location;
    let now = chrono::Utc::now().into();
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
    };
    let result_id = arcodash_core::entities::query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set(hash.clone()),
        query: Set("SELECT 1".to_string()),
        data: Set(None),
        data_location: Set(Some(location)),
        data_size_bytes: Set(Some(1)),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        last_used_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&app.db)
    .await
    .unwrap()
    .id;

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute (cache hit during switch)",
    )
    .await;
    assert_eq!(
        body["query_result"]["id"].as_i64(),
        Some(result_id),
        "{body}"
    );
    assert_eq!(body["query_result"]["data"], data);
    assert!(body.get("job").is_none(), "{body}");
}
