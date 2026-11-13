//! Deleting a monitor or data source removes its stored objects, only in storage the operator
//! manages, and never blocks the delete.

mod common;
use common::{default_org_id, ok_json, spawn_app_with_storage, TestApp};

use std::sync::Arc;

use arcodash_core::entities::{data_source, data_source_upload, monitor};
use arcodash_core::org_storage::{BlobStore, StorageMode};
use arcodash_core::storage::upload_rel_key;
use arcodash_core::test_support::blob::{FailingBlobStore, MemoryBlobStore, StaticResolver};
use axum::body::Bytes;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, EntityTrait};

async fn app_with(store: Arc<dyn BlobStore>, mode: StorageMode) -> Option<TestApp> {
    spawn_app_with_storage(Arc::new(StaticResolver::with_store(mode, store, ""))).await
}

async fn create_ds(client: &reqwest::Client, base: &str, name: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn create_monitor(client: &reqwest::Client, base: &str, query_id: i64, name: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/monitors"))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": name,
                "options": { "op": ">", "value": 100, "column": "count" },
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn create_published_query(client: &reqwest::Client, base: &str, ds_id: i64) -> i64 {
    let id = ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id, "name": "q", "query": "SELECT 1", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    ok_json(
        client
            .post(format!("{base}/api/queries/{id}/publish"))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;
    id
}

async fn put(store: &MemoryBlobStore, key: &str) {
    store
        .put(key, Bytes::from_static(b"x"), "text/plain")
        .await
        .unwrap();
}

async fn seed_upload(app: &TestApp, store: &MemoryBlobStore, ds_id: i64) -> String {
    let id = uuid::Uuid::new_v4();
    let key = upload_rel_key("csv", id, "a.csv");
    put(store, &format!("uploads/{key}")).await;
    data_source_upload::ActiveModel {
        id: Set(id),
        org_id: Set(default_org_id(&app.db).await),
        object_key: Set(key.clone()),
        filename: Set("a.csv".into()),
        byte_size: Set(1),
        content_type: Set("text/csv".into()),
        data_source_id: Set(Some(ds_id)),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    key
}

async fn delete(client: &reqwest::Client, url: String) -> u16 {
    client.delete(url).send().await.unwrap().status().as_u16()
}

struct MonitorFixture {
    mine: i64,
    other: i64,
    mine_key: String,
    other_key: String,
}

async fn seed_monitors(
    app: &TestApp,
    client: &reqwest::Client,
    mem: &MemoryBlobStore,
) -> MonitorFixture {
    let ds = create_ds(client, &app.address, "cleanup-monitor-ds").await;
    let query = create_published_query(client, &app.address, ds).await;
    let mine = create_monitor(client, &app.address, query, "mine").await;
    let other = create_monitor(client, &app.address, query, "other").await;
    let mine_key = format!("monitors/{mine}/observations/2026-01.jsonl");
    let other_key = format!("monitors/{other}/observations/2026-01.jsonl");
    put(mem, &mine_key).await;
    put(mem, &other_key).await;
    MonitorFixture {
        mine,
        other,
        mine_key,
        other_key,
    }
}

#[tokio::test]
async fn deleting_monitor_removes_its_observation_files() {
    let mem = Arc::new(MemoryBlobStore::default());
    let app = skip_if_none!(app_with(mem.clone(), StorageMode::ArcoDash).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let f = seed_monitors(&app, &client, &mem).await;

    let status = delete(&client, format!("{}/api/monitors/{}", app.address, f.mine)).await;
    assert_eq!(status, 204);

    let keys = mem.keys();
    assert!(!keys.contains(&f.mine_key));
    assert!(keys.contains(&f.other_key));
}

#[tokio::test]
async fn deleting_query_removes_its_monitors_observation_files() {
    let mem = Arc::new(MemoryBlobStore::default());
    let app = skip_if_none!(app_with(mem.clone(), StorageMode::ArcoDash).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let f = seed_monitors(&app, &client, &mem).await;
    let ds = create_ds(&client, &app.address, "cleanup-query-other-ds").await;
    let other_query = create_published_query(&client, &app.address, ds).await;
    let survivor = create_monitor(&client, &app.address, other_query, "survivor").await;
    let survivor_key = format!("monitors/{survivor}/observations/2026-01.jsonl");
    put(&mem, &survivor_key).await;
    let query_id = monitor::Entity::find_by_id(f.mine)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .query_id;

    let status = delete(&client, format!("{}/api/queries/{query_id}", app.address)).await;
    assert_eq!(status, 204);

    let keys = mem.keys();
    assert!(!keys.contains(&f.mine_key), "{keys:?}");
    assert!(!keys.contains(&f.other_key), "{keys:?}");
    assert!(keys.contains(&survivor_key));
}

#[tokio::test]
async fn deleting_data_source_removes_results_and_profile() {
    let mem = Arc::new(MemoryBlobStore::default());
    let app = skip_if_none!(app_with(mem.clone(), StorageMode::ArcoDash).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let mine = create_ds(&client, &app.address, "cleanup-ds-a").await;
    let other = create_ds(&client, &app.address, "cleanup-ds-b").await;
    let mine_upload = seed_upload(&app, &mem, mine).await;
    let other_upload = seed_upload(&app, &mem, other).await;
    let mine_keys = [
        format!("results/{mine}/h.json.gz"),
        format!("ai-profiles/{mine}.json"),
        format!("uploads/{mine_upload}"),
    ];
    let other_keys = [
        format!("results/{other}/h.json.gz"),
        format!("ai-profiles/{other}.json"),
        format!("uploads/{other_upload}"),
    ];
    for k in mine_keys.iter().chain(&other_keys) {
        put(&mem, k).await;
    }

    let status = delete(&client, format!("{}/api/data_sources/{mine}", app.address)).await;
    assert_eq!(status, 204);

    let keys = mem.keys();
    assert!(mine_keys.iter().all(|k| !keys.contains(k)), "{keys:?}");
    assert!(other_keys.iter().all(|k| keys.contains(k)));
}

#[tokio::test]
async fn customer_storage_objects_are_never_deleted_on_delete() {
    let mem = Arc::new(MemoryBlobStore::default());
    let app = skip_if_none!(app_with(mem.clone(), StorageMode::Customer).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let f = seed_monitors(&app, &client, &mem).await;
    let ds = create_ds(&client, &app.address, "cleanup-customer-ds").await;
    let upload = format!("uploads/{}", seed_upload(&app, &mem, ds).await);
    put(&mem, &format!("results/{ds}/h.json.gz")).await;
    put(&mem, &format!("ai-profiles/{ds}.json")).await;
    let before = mem.keys();

    assert_eq!(
        delete(&client, format!("{}/api/monitors/{}", app.address, f.mine)).await,
        204
    );
    assert_eq!(
        delete(&client, format!("{}/api/data_sources/{ds}", app.address)).await,
        204
    );

    let query_id = monitor::Entity::find_by_id(f.other)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .query_id;
    assert_eq!(
        delete(&client, format!("{}/api/queries/{query_id}", app.address)).await,
        204
    );

    assert_eq!(mem.deletes(), 0);
    assert_eq!(mem.keys(), before);
    assert!(mem.keys().contains(&upload));
}

#[tokio::test]
async fn storage_cleanup_failure_does_not_block_delete() {
    let failing = Arc::new(FailingBlobStore::default());
    let app = skip_if_none!(app_with(failing.clone(), StorageMode::ArcoDash).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let mem = MemoryBlobStore::default();
    let f = seed_monitors(&app, &client, &mem).await;
    let ds = create_ds(&client, &app.address, "cleanup-failing-ds").await;

    assert_eq!(
        delete(&client, format!("{}/api/monitors/{}", app.address, f.other)).await,
        204
    );
    assert_eq!(
        delete(&client, format!("{}/api/data_sources/{ds}", app.address)).await,
        204
    );

    assert!(monitor::Entity::find_by_id(f.other)
        .one(&app.db)
        .await
        .unwrap()
        .is_none());
    assert!(data_source::Entity::find_by_id(ds)
        .one(&app.db)
        .await
        .unwrap()
        .is_none());
}
