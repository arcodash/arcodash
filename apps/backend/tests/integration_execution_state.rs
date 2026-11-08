//! Integration tests for execution-state binding of ad-hoc/editor executions.
//! `query_service::execute` promotes an ad-hoc run to a bound run when its content matches the
//! query's draft, and keeps the query's execution-state columns and result pointer in sync on a
//! cache hit. Drives the real `POST /api/query_results` and `POST /api/queries/<id>/results`
//! endpoints. Skipped when `TEST_DATABASE_URL` is unset.

mod common;
use common::default_org_id;
use common::{spawn_app, test_api_key, user_with_role, ADMIN_EMAIL};

use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use arcodash_backend::services::permission_service;
use arcodash_core::entities::{query, query_result, user};
use arcodash_core::hash::gen_query_hash;

/// Create a query, returning `(query_id, query_hash, working_version)`. Created queries start as
/// unpublished drafts.
async fn create_query(
    client: &reqwest::Client,
    address: &str,
    ds_id: i64,
    name: &str,
    sql: &str,
) -> (i64, String, i32) {
    let resp = client
        .post(format!("{address}/api/queries"))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": name,
            "query": sql,
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create query: {body}");
    let q: serde_json::Value = serde_json::from_str(&body).unwrap();
    (
        q["id"].as_i64().unwrap(),
        q["query_hash"].as_str().unwrap().to_string(),
        q["working_version"].as_i64().unwrap() as i32,
    )
}

async fn create_ds(client: &reqwest::Client, address: &str, name: &str) -> i64 {
    let resp = client
        .post(format!("{address}/api/data_sources"))
        .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create data source: {body}");
    let ds: serde_json::Value = serde_json::from_str(&body).unwrap();
    ds["id"].as_i64().unwrap()
}

/// Seed a fresh `query_results` row directly (no worker involved).
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query: &str,
    data: serde_json::Value,
) -> query_result::Model {
    seed_result_at(
        db,
        org_id,
        data_source_id,
        query_hash,
        query,
        data,
        chrono::Utc::now().into(),
    )
    .await
}

/// Like [`seed_result`] with an explicit `retrieved_at`.
async fn seed_result_at(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query: &str,
    data: serde_json::Value,
    retrieved_at: sea_orm::prelude::DateTimeWithTimeZone,
) -> query_result::Model {
    let now = chrono::Utc::now().into();
    let model = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query.to_string()),
        data: Set(Some(data)),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(retrieved_at),
        created_at: Set(now),
        updated_at: Set(now),
    };
    model.insert(db).await.expect("seed query_result failed")
}

fn sample_data() -> serde_json::Value {
    serde_json::json!({
        "columns": [{ "name": "id" }, { "name": "label" }],
        "rows": [
            { "id": 1, "label": "alpha" },
            { "id": 2, "label": "beta" },
            { "id": 3, "label": "gamma" }
        ]
    })
}

async fn fetch_query(db: &sea_orm::DatabaseConnection, id: i64) -> query::Model {
    query::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

/// An ad-hoc execute whose `query`/`data_source_id` match the query's draft and hit a fresh cache
/// must promote to a bound execution and write the pointer and state columns.
#[tokio::test]
async fn bound_ad_hoc_cache_hit_writes_state_and_is_idempotent() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash, _version) =
        create_query(&client, &app.address, ds_id, "Editor Query", sql).await;
    let cached = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let before = fetch_query(&app.db, query_id).await;
    assert_eq!(before.last_execution_status, "New");
    assert!(before.latest_query_data_id.is_none());

    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "ad-hoc execute: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        parsed.get("query_result").is_some(),
        "matching content over a fresh cache must still return a cache hit; got: {body}"
    );
    assert_eq!(
        parsed["query_result"]["id"].as_i64(),
        Some(cached.id),
        "cache hit must return the seeded result id; got: {body}"
    );

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(
        after.latest_query_data_id,
        Some(cached.id),
        "promoted cache hit must repoint the draft pointer"
    );
    assert_eq!(after.last_execution_status, "Success");
    assert_eq!(
        after.last_execution_at.map(|t| t.timestamp()),
        Some(cached.retrieved_at.timestamp()),
        "last_execution_at must be the cache row's retrieved_at, not the current time"
    );
    assert_eq!(
        after.row_count,
        Some(3),
        "row_count must reflect the cached rows"
    );
    assert!(after.last_execution_error.is_none());
    assert!(
        after.published_latest_query_data_id.is_none(),
        "an unpublished query's draft-only pointer must not touch published_latest_query_data_id"
    );

    // Idempotent repeat: the pointer already matches, so no write happens and the row stays
    // byte-identical.
    let resp2 = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 200);
    let after2 = fetch_query(&app.db, query_id).await;
    assert_eq!(
        after, after2,
        "a repeat cache hit against an already-synced pointer must not write anything"
    );
}

/// A cache hit older than the recorded execution moves the result pointer but must not overwrite
/// the newer failure's status/timestamp/error.
#[tokio::test]
async fn cache_hit_does_not_overwrite_a_newer_failure_with_an_older_success() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-race-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash, _version) =
        create_query(&client, &app.address, ds_id, "Race Query", sql).await;

    let failed_at: sea_orm::prelude::DateTimeWithTimeZone = chrono::Utc::now().into();
    let mut active: query::ActiveModel = fetch_query(&app.db, query_id).await.into();
    active.last_execution_status = Set("Failed".to_string());
    active.last_execution_at = Set(Some(failed_at));
    active.last_execution_error = Set(Some("connection refused".to_string()));
    active.update(&app.db).await.unwrap();

    let stale_at = failed_at - chrono::Duration::seconds(60);
    let cached = seed_result_at(&app.db, org_id, ds_id, &hash, sql, sample_data(), stale_at).await;

    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "ad-hoc execute: {body}");

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(
        after.latest_query_data_id,
        Some(cached.id),
        "the cache hit is real and must still move the result pointer"
    );
    assert_eq!(
        after.last_execution_status, "Failed",
        "an older cached success must not overwrite a newer failure's status"
    );
    assert_eq!(
        after.last_execution_at.map(|t| t.timestamp()),
        Some(failed_at.timestamp()),
        "last_execution_at must stay at the newer failure's timestamp"
    );
    assert_eq!(
        after.last_execution_error.as_deref(),
        Some("connection refused"),
        "the failure's error must survive the older cache hit"
    );
}

/// A cache hit more recent than the last recorded execution flips the status forward, even from a
/// failure.
#[tokio::test]
async fn cache_hit_applies_when_genuinely_more_recent_than_a_prior_failure() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-recover-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash, _version) =
        create_query(&client, &app.address, ds_id, "Recovering Query", sql).await;

    let failed_at: sea_orm::prelude::DateTimeWithTimeZone =
        (chrono::Utc::now() - chrono::Duration::seconds(60)).into();
    let mut active: query::ActiveModel = fetch_query(&app.db, query_id).await.into();
    active.last_execution_status = Set("Failed".to_string());
    active.last_execution_at = Set(Some(failed_at));
    active.last_execution_error = Set(Some("timeout".to_string()));
    active.update(&app.db).await.unwrap();

    let cached = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(after.latest_query_data_id, Some(cached.id));
    assert_eq!(
        after.last_execution_status, "Success",
        "a genuinely newer cache hit must flip status forward, even from a prior failure"
    );
    assert_eq!(
        after.last_execution_at.map(|t| t.timestamp()),
        Some(cached.retrieved_at.timestamp())
    );
    assert!(after.last_execution_error.is_none());
}

/// An ad-hoc execute whose `query` diverges from the saved draft is never promoted; no
/// execution-state column changes on a cache hit.
#[tokio::test]
async fn ad_hoc_cache_hit_with_mismatched_text_stays_unbound() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-mismatch-ds").await;
    let saved_sql = "SELECT 1";
    let (query_id, _hash, _version) =
        create_query(&client, &app.address, ds_id, "Has Unsaved Edit", saved_sql).await;

    // Seed a cached result for the edited (unsaved) SQL.
    let edited_sql = "SELECT 2";
    let edited_hash = gen_query_hash(edited_sql, &serde_json::Value::Null);
    seed_result(
        &app.db,
        org_id,
        ds_id,
        &edited_hash,
        edited_sql,
        sample_data(),
    )
    .await;

    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": edited_sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(
        after.last_execution_status, "New",
        "a divergent ad-hoc run must never flip the saved query's execution state"
    );
    assert!(after.latest_query_data_id.is_none());
}

/// A published query's cache hit via `execute_saved` repoints `published_latest_query_data_id`,
/// never the draft's `latest_query_data_id`.
#[tokio::test]
async fn published_query_cache_hit_moves_published_pointer_not_draft() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-pub-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash, version) =
        create_query(&client, &app.address, ds_id, "Published Query", sql).await;

    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": version }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        publish_resp.status().as_u16(),
        200,
        "publish failed: {}",
        publish_resp.text().await.unwrap_or_default()
    );

    let cached = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "execute_saved: {body}");

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(
        after.published_latest_query_data_id,
        Some(cached.id),
        "a published query's cache hit must repoint the published pointer"
    );
    assert!(
        after.latest_query_data_id.is_none(),
        "the draft pointer must be untouched by a published-content execution"
    );
    assert_eq!(after.last_execution_status, "Success");
}

/// `QueryResponse::from` must fall back to the resolved (published-or-draft) pointer when the draft
/// pointer is null, or `GET /api/queries` reports Success with a null pointer the frontend reads as
/// "no exportable result". Drives `execute_saved` and asserts the list response carries a matching
/// pointer.
#[tokio::test]
async fn list_reports_resolved_pointer_for_published_query_after_served_content_execution() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-list-pointer-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash, version) = create_query(
        &client,
        &app.address,
        ds_id,
        "Published Query List Pointer",
        sql,
    )
    .await;

    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": version }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        publish_resp.status().as_u16(),
        200,
        "publish failed: {}",
        publish_resp.text().await.unwrap_or_default()
    );

    let cached = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let exec_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
        .send()
        .await
        .unwrap();
    assert_eq!(exec_resp.status().as_u16(), 200);

    // Sanity: the row has a null draft pointer and only the published one set.
    let after = fetch_query(&app.db, query_id).await;
    assert!(after.latest_query_data_id.is_none());
    assert_eq!(after.published_latest_query_data_id, Some(cached.id));

    let list_resp = client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let row = list_body
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("published query must appear in the list response");

    assert_eq!(
        row["last_execution_status"].as_str(),
        Some("Success"),
        "row: {row}"
    );
    assert_eq!(row["row_count"].as_i64(), Some(3), "row: {row}");
    assert_eq!(
        row["latest_query_data_id"].as_i64(),
        Some(cached.id),
        "list() must report the resolved (published) pointer, not the raw null draft column; row: {row}"
    );
}

/// A published query with its own more recent draft test-run result must report the same pointer
/// from `GET /api/queries` and `GET /api/queries/<id>/results` (both use
/// `resolve_latest_query_data_id`, which prefers the draft).
#[tokio::test]
async fn list_and_editor_stored_result_agree_on_pointer_when_draft_and_published_diverge() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-diverge-ds").await;
    let published_sql = "SELECT 1";
    let (query_id, published_hash, version) = create_query(
        &client,
        &app.address,
        ds_id,
        "Diverging Draft And Published",
        published_sql,
    )
    .await;

    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": version }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        publish_resp.status().as_u16(),
        200,
        "publish failed: {}",
        publish_resp.text().await.unwrap_or_default()
    );

    let published_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &published_hash,
        published_sql,
        sample_data(),
    )
    .await;
    let exec_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
        .send()
        .await
        .unwrap();
    assert_eq!(exec_resp.status().as_u16(), 200);

    let draft_sql = "SELECT 2";
    let update_resp = client
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "query": draft_sql, "expected_version": version + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        update_resp.status().as_u16(),
        200,
        "draft edit failed: {}",
        update_resp.text().await.unwrap_or_default()
    );
    let draft_hash = gen_query_hash(draft_sql, &serde_json::Value::Null);
    let draft_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &draft_hash,
        draft_sql,
        sample_data(),
    )
    .await;
    let adhoc_resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": draft_sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(adhoc_resp.status().as_u16(), 200);

    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(after.latest_query_data_id, Some(draft_result.id));
    assert_eq!(
        after.published_latest_query_data_id,
        Some(published_result.id)
    );
    assert_ne!(draft_result.id, published_result.id);

    let list_resp = client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let row = list_body
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("query must appear in the list response");
    assert_eq!(
        row["latest_query_data_id"].as_i64(),
        Some(draft_result.id),
        "list() must report the draft pointer, not the published one, when both exist; row: {row}"
    );

    let editor_resp = client
        .get(format!("{}/api/queries/{query_id}/results", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(editor_resp.status().as_u16(), 200);
    let editor_body: serde_json::Value = editor_resp.json().await.unwrap();
    assert_eq!(
        editor_body["query_result"]["id"].as_i64(),
        Some(draft_result.id),
        "the editor's stored-result endpoint must agree with list() on the resolved pointer; body: {editor_body}"
    );
}

/// A non-editor Viewer of a published-but-being-edited query must see the PUBLISHED pointer from
/// both endpoints, never the draft's: draft-first resolution is safe only for callers who see the
/// draft SQL, else it leaks output of a query they never saw.
#[tokio::test]
async fn viewer_sees_published_pointer_from_list_and_results_when_draft_and_published_diverge() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "exec-state-viewer-ds").await;
    let published_sql = "SELECT 1";
    let (query_id, published_hash, version) = create_query(
        &client,
        &app.address,
        ds_id,
        "Viewer Sees Published Only",
        published_sql,
    )
    .await;

    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": version }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        publish_resp.status().as_u16(),
        200,
        "publish failed: {}",
        publish_resp.text().await.unwrap_or_default()
    );

    let published_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &published_hash,
        published_sql,
        sample_data(),
    )
    .await;
    let exec_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
        .send()
        .await
        .unwrap();
    assert_eq!(exec_resp.status().as_u16(), 200);

    // Draft test-run result: the editor's own, never meant for a viewer.
    let draft_sql = "SELECT 2";
    let update_resp = client
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "query": draft_sql, "expected_version": version + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        update_resp.status().as_u16(),
        200,
        "draft edit failed: {}",
        update_resp.text().await.unwrap_or_default()
    );
    let draft_hash = gen_query_hash(draft_sql, &serde_json::Value::Null);
    let draft_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &draft_hash,
        draft_sql,
        sample_data(),
    )
    .await;
    let adhoc_resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": draft_sql,
            "parameters": {},
            "options": {},
            "max_age": 86400,
            "query_id": query_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(adhoc_resp.status().as_u16(), 200);

    // Sanity: the two pointers diverge on the row.
    let after = fetch_query(&app.db, query_id).await;
    assert_eq!(after.latest_query_data_id, Some(draft_result.id));
    assert_eq!(
        after.published_latest_query_data_id,
        Some(published_result.id)
    );
    assert_ne!(draft_result.id, published_result.id);

    // A non-admin, non-owner Viewer with an explicit "viewer" grant: the user
    // `overlay_serving_content` protects.
    let (viewer, _role_id) =
        user_with_role(&app.db, org_id, &["view_query", "execute_query"]).await;
    let admin = user::Entity::find()
        .filter(user::Column::Email.eq(ADMIN_EMAIL))
        .filter(user::Column::OrgId.eq(org_id))
        .one(&app.db)
        .await
        .expect("admin user query failed")
        .expect("admin user must exist");
    permission_service::grant(
        &app.db,
        org_id,
        &admin,
        "query",
        query_id,
        Some(viewer.id),
        None,
        "viewer",
    )
    .await
    .expect("viewer grant failed");

    let viewer_client = reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!(
                    "Key {}",
                    test_api_key(&viewer.email)
                ))
                .unwrap(),
            );
            headers
        })
        .build()
        .unwrap();

    let list_resp = viewer_client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let row = list_body
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("query must appear in the viewer's list response");
    assert_eq!(
        row["latest_query_data_id"].as_i64(),
        Some(published_result.id),
        "a non-editor viewer's list() must report the published pointer, never the draft's own \
         test-run pointer; row: {row}"
    );

    let results_resp = viewer_client
        .get(format!("{}/api/queries/{query_id}/results", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        results_resp.status().as_u16(),
        200,
        "viewer GET .../results: {}",
        results_resp.text().await.unwrap_or_default()
    );
    let results_body: serde_json::Value = results_resp.json().await.unwrap();
    assert_eq!(
        results_body["query_result"]["id"].as_i64(),
        Some(published_result.id),
        "a non-editor viewer's GET .../results must return the published result, never the \
         draft's own separately-executed test-run result; body: {results_body}"
    );
}
