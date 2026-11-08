//! Integration tests: the publish/restore/discard/unpublish lifecycle, the `working_version`
//! optimistic-concurrency token, and the Editor-or-above gate. Unpublish sets `unpublished_at`
//! only, so publish history and `restore` survive a republish.

mod common;
use common::ok_json;
use common::spawn_app;

use redis::AsyncCommands;
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait,
    PaginatorTrait, QueryFilter,
};

use arcodash_core::entities::query::Entity as QueryEntity;
use arcodash_core::entities::{access_permission, change, organization, query_result};
use arcodash_core::hash::gen_query_hash;
use arcodash_core::queue::{JobMessage, QueueName};
use arcodash_core::test_support::new_user;

/// A fresh group in `org_id` linked to a role carrying exactly `view_query` (reads need the
/// capability as well as reach).
async fn view_query_group_id(db: &sea_orm::DatabaseConnection, org_id: i64) -> i64 {
    use arcodash_core::entities::{group, group_role, role};
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("view-query-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["view_query".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("role insert failed")
    .last_insert_id;
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "view-query-group-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        data_source_access_restricted: Set(false),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("group insert failed")
    .last_insert_id;
    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("group_role insert failed");
    group_id
}

async fn seed_query(
    client: &reqwest::Client,
    address: &str,
    ds_name: &str,
    query_name: &str,
    query_text: &str,
) -> (i64, i64) {
    let ds = ok_json(
        client
            .post(format!("{address}/api/data_sources"))
            .json(&serde_json::json!({ "name": ds_name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{address}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": query_name,
                "query": query_text,
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    (ds_id, query_id)
}

async fn get_query(client: &reqwest::Client, address: &str, query_id: i64) -> serde_json::Value {
    ok_json(
        client
            .get(format!("{address}/api/queries/{query_id}"))
            .send()
            .await
            .unwrap(),
        "get query",
    )
    .await
}

/// One `queries` row fetched directly, to assert on `published_*` columns `QueryResponse` doesn't
/// expose.
async fn get_row(
    db: &sea_orm::DatabaseConnection,
    query_id: i64,
) -> arcodash_core::entities::query::Model {
    QueryEntity::find_by_id(query_id)
        .one(db)
        .await
        .unwrap()
        .expect("query row must exist")
}

/// Count of `changes` rows for `query_id`.
async fn changes_count_for_query(db: &sea_orm::DatabaseConnection, query_id: i64) -> u64 {
    change::Entity::find()
        .filter(change::Column::ObjectType.eq("query"))
        .filter(change::Column::ObjectId.eq(query_id))
        .count(db)
        .await
        .unwrap()
}

/// First publish fills every `published_*` column from the draft, seeds
/// `published_latest_query_data_id`, appends one `changes` row and bumps `working_version`.
#[tokio::test]
async fn first_publish_fills_published_columns_and_seeds_result_pointer() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "publish-ds", "My Query", "SELECT 1").await;

    // Give the draft a result pointer so the first-publish seeding is observable.
    let row = get_row(&app.db, query_id).await;
    let result = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(row.org_id),
        data_source_id: Set(row.data_source_id),
        query_hash: Set(row.query_hash.clone()),
        query: Set(row.query.clone()),
        data: Set(Some(serde_json::json!({"columns": [], "rows": []}))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(None),
        retrieved_at: Set(chrono::Utc::now().into()),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let mut active: arcodash_core::entities::query::ActiveModel = row.into();
    active.latest_query_data_id = Set(Some(result.id));
    active.update(&app.db).await.unwrap();
    let result_id = result.id;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    let body = ok_json(resp, "first publish").await;

    assert_eq!(body["published"], true);
    assert_eq!(body["has_unpublished_changes"], false);
    assert_eq!(body["working_version"], 1);

    let row = get_row(&app.db, query_id).await;
    assert_eq!(row.published_name.as_deref(), Some("My Query"));
    assert_eq!(row.published_query.as_deref(), Some("SELECT 1"));
    assert_eq!(row.published_data_source_id, Some(row.data_source_id));
    assert!(row.published_change_id.is_some());
    assert!(row.published_at.is_some());
    assert_eq!(
        row.published_latest_query_data_id,
        Some(result_id),
        "first publish seeds the published result pointer from the draft's own"
    );

    let history = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "history after first publish",
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 1);
}

/// Editing the draft after a publish sets `has_unpublished_changes` while `published_*` stay as
/// they were.
#[tokio::test]
async fn editing_draft_after_publish_leaves_published_columns_untouched() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(&client, &app.address, "edit-ds", "Q", "SELECT 1").await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    let before_edit = get_row(&app.db, query_id).await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit draft",
    )
    .await;

    let after = get_query(&client, &app.address, query_id).await;
    assert_eq!(after["has_unpublished_changes"], true);
    assert_eq!(after["query"], "SELECT 2", "draft reflects the edit");

    let after_row = get_row(&app.db, query_id).await;
    assert_eq!(after_row.published_query, before_edit.published_query);
    assert_eq!(after_row.published_query.as_deref(), Some("SELECT 1"));
}

/// Publishing again overwrites `published_*`, appends a `changes` row, and leaves
/// `published_latest_query_data_id` untouched.
#[tokio::test]
async fn publish_changes_overwrites_published_columns_leaves_result_pointer() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "republish-ds", "Q", "SELECT 1").await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "first publish",
    )
    .await;

    let after_first = get_row(&app.db, query_id).await;
    let result_pointer_after_first = after_first.published_latest_query_data_id;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "name": "Renamed", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit draft",
    )
    .await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 2 }))
        .send()
        .await
        .unwrap();
    let body = ok_json(resp, "publish changes").await;
    assert_eq!(body["has_unpublished_changes"], false);
    assert_eq!(body["working_version"], 3);

    let after_second = get_row(&app.db, query_id).await;
    assert_eq!(after_second.published_query.as_deref(), Some("SELECT 2"));
    assert_eq!(after_second.published_name.as_deref(), Some("Renamed"));
    assert_eq!(
        after_second.published_latest_query_data_id, result_pointer_after_first,
        "republish must not touch the published result pointer"
    );

    let history = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "history after publish changes",
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 2);
}

/// Discard resets the draft to the published content and bumps `working_version`.
#[tokio::test]
async fn discard_resets_draft_to_published_content() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "discard-ds", "Original", "SELECT 1").await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "name": "Changed", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit draft",
    )
    .await;

    let mid = get_query(&client, &app.address, query_id).await;
    assert_eq!(mid["has_unpublished_changes"], true);

    let resp = client
        .post(format!("{}/api/queries/{query_id}/discard", app.address))
        .json(&serde_json::json!({ "expected_version": 2 }))
        .send()
        .await
        .unwrap();
    let body = ok_json(resp, "discard").await;

    assert_eq!(
        body["query"], "SELECT 1",
        "draft reset to published query text"
    );
    assert_eq!(body["name"], "Original", "draft reset to published name");
    assert_eq!(body["has_unpublished_changes"], false);
    assert_eq!(body["working_version"], 3);

    let after = get_query(&client, &app.address, query_id).await;
    assert_eq!(after["query"], "SELECT 1");
    assert_eq!(after["name"], "Original");
}

#[tokio::test]
async fn discard_on_never_published_query_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "discard-never-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/discard", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "discard on a never-published query must be 400: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// Restore makes a past publish point current and resets the draft to match; history only grows.
#[tokio::test]
async fn restore_promotes_past_publish_point_and_resets_draft() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "restore-ds", "V1", "SELECT 1").await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish v1",
    )
    .await;

    let history_after_v1 = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "history after v1",
    )
    .await;
    let v1_change_id = history_after_v1[0]["change_id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "name": "V2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit to v2",
    )
    .await;
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 2 }))
            .send()
            .await
            .unwrap(),
        "publish v2",
    )
    .await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/restore", app.address))
        .json(&serde_json::json!({ "change_id": v1_change_id, "expected_version": 3 }))
        .send()
        .await
        .unwrap();
    let body = ok_json(resp, "restore v1").await;

    assert_eq!(body["query"], "SELECT 1", "draft reset to v1's content");
    assert_eq!(body["name"], "V1");
    assert_eq!(body["has_unpublished_changes"], false);
    assert_eq!(body["working_version"], 4);

    let row = get_row(&app.db, query_id).await;
    assert_eq!(row.query, "SELECT 1", "draft column equals v1's content");
    assert_eq!(row.name, "V1");
    assert_eq!(
        row.published_query.as_deref(),
        Some("SELECT 1"),
        "published column equals v1's content"
    );
    assert_eq!(row.published_name.as_deref(), Some("V1"));
    assert!(row.published_change_id.is_some());

    let history_after_restore = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "history after restore",
    )
    .await;
    assert_eq!(history_after_restore.as_array().unwrap().len(), 3);

    let served = get_query(&client, &app.address, query_id).await;
    assert_eq!(served["query"], "SELECT 1");
}

#[tokio::test]
async fn restore_on_never_published_query_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "restore-never-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/restore", app.address))
        .json(&serde_json::json!({ "change_id": 1, "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "restore on a never-published query must be 400: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// publish → publish-changes → discard → restore on one query, checking lifecycle fields after each
/// step.
#[tokio::test]
async fn publish_then_publish_changes_then_discard_then_restore_round_trip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "roundtrip-ds", "Q1", "SELECT 1").await;

    let p1 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;
    assert_eq!(p1["published"], true);
    assert_eq!(p1["working_version"], 1);

    let history_after_p1 = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "history after p1",
    )
    .await;
    let p1_change_id = history_after_p1[0]["change_id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit",
    )
    .await;

    let p2 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 2 }))
            .send()
            .await
            .unwrap(),
        "publish changes",
    )
    .await;
    assert_eq!(p2["working_version"], 3);

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 3", "expected_version": 3 }))
            .send()
            .await
            .unwrap(),
        "edit again",
    )
    .await;

    let d = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/discard", app.address))
            .json(&serde_json::json!({ "expected_version": 4 }))
            .send()
            .await
            .unwrap(),
        "discard",
    )
    .await;
    assert_eq!(
        d["query"], "SELECT 2",
        "discard restores the latest publish"
    );
    assert_eq!(d["working_version"], 5);

    let r = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/restore", app.address))
            .json(&serde_json::json!({ "change_id": p1_change_id, "expected_version": 5 }))
            .send()
            .await
            .unwrap(),
        "restore v1",
    )
    .await;
    assert_eq!(
        r["published"], true,
        "restore never clears published_change_id"
    );
    assert_eq!(r["query"], "SELECT 1", "restore promotes v1's content");
    assert_eq!(r["working_version"], 6);
}

/// A stale `expected_version` on publish/discard/restore returns 409 (`error_code: "conflict"` +
/// current `working_version`), applies no write, and leaves no orphaned `changes` row.
#[tokio::test]
async fn stale_expected_version_returns_409_on_every_lifecycle_endpoint() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "conflict-ds", "Q", "SELECT 1").await;

    assert_eq!(
        changes_count_for_query(&app.db, query_id).await,
        0,
        "no changes row before any publish"
    );

    let resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error_code"], "conflict");
    assert_eq!(body["working_version"], 0);

    assert_eq!(
        changes_count_for_query(&app.db, query_id).await,
        0,
        "a rejected (stale-version) publish must not leave an orphaned changes row"
    );

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;
    let changes_after_real_publish = changes_count_for_query(&app.db, query_id).await;
    assert_eq!(changes_after_real_publish, 1);

    let resp = client
        .post(format!("{}/api/queries/{query_id}/discard", app.address))
        .json(&serde_json::json!({ "expected_version": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error_code"], "conflict");
    assert_eq!(body["working_version"], 1);

    let resp = client
        .post(format!("{}/api/queries/{query_id}/restore", app.address))
        .json(&serde_json::json!({ "change_id": 1, "expected_version": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);

    assert_eq!(
        changes_count_for_query(&app.db, query_id).await,
        changes_after_real_publish,
        "a rejected (stale-version) restore must not leave an orphaned changes row"
    );

    let after = get_query(&client, &app.address, query_id).await;
    assert_eq!(after["published"], true);
    assert_eq!(after["working_version"], 1);

    let resp = client
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "query": "SELECT 9", "expected_version": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
}

/// A Viewer-grant user (no edit authority) gets 404 from publish/restore/discard, the usual
/// convention for denied object-bound actions, though they can GET the query.
#[tokio::test]
async fn viewer_without_edit_authority_gets_404_on_lifecycle_endpoints() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&admin, &app.address, "viewer-403-ds", "Q", "SELECT 1").await;
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish as admin",
    )
    .await;

    // A plain org member with a viewer grant and `view_query`, authenticated via `?api_key=` (which
    // bypasses CSRF/session).
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(arcodash_backend::auth::DEFAULT_ORG_SLUG))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");

    let view_query_group = view_query_group_id(&app.db, org.id).await;
    let mut viewer = new_user(org.id);
    viewer.email = Set("edit-commit-viewer@test.local".to_string());
    viewer.api_key_hash = Set(arcodash_core::hash::hash_api_key("edit-commit-viewer-key"));
    viewer.groups = Set(vec![view_query_group]);
    let viewer = viewer.insert(&app.db).await.unwrap();

    let grant = access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query_id),
        access_type: Set("viewer".to_string()),
        grantor_id: Set(viewer.id),
        grantee_id: Set(Some(viewer.id)),
        grantee_group_id: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
    };
    grant.insert(&app.db).await.unwrap();

    let viewer_client = reqwest::Client::new();

    // Confirm the viewer can see the query, so the denial is an authority gate, not visibility.
    let get_resp = viewer_client
        .get(format!(
            "{}/api/queries/{query_id}?api_key={}",
            app.address, "edit-commit-viewer-key"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status().as_u16(), 200);

    let requests: Vec<(&str, serde_json::Value)> = vec![
        ("publish", serde_json::json!({ "expected_version": 1 })),
        (
            "restore",
            serde_json::json!({ "change_id": 1, "expected_version": 1 }),
        ),
        ("discard", serde_json::json!({ "expected_version": 1 })),
    ];
    for (path, body) in requests {
        let resp = viewer_client
            .post(format!(
                "{}/api/queries/{query_id}/{path}?api_key={}",
                app.address, "edit-commit-viewer-key"
            ))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "viewer-only grant must not authorize {path}: {}",
            resp.text().await.unwrap_or_default()
        );
    }
}

/// With an edited, unpublished draft, `latest_result` and `execute_saved`'s cache hit must both
/// still resolve against the PUBLISHED content.
#[tokio::test]
async fn latest_result_and_execute_saved_serve_published_after_unpublished_draft_edit() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (ds_id, query_id) = seed_query(&client, &app.address, "served-ds", "Q", "SELECT 1").await;

    // Seed a cached result for the soon-to-be-published text and point the draft at it.
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(arcodash_backend::auth::DEFAULT_ORG_SLUG))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let hash = gen_query_hash("SELECT 1", &serde_json::Value::Null);
    let result = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        data_source_id: Set(ds_id),
        query_hash: Set(hash),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(serde_json::json!({"columns": [], "rows": []}))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(None),
        retrieved_at: Set(chrono::Utc::now().into()),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let row = get_row(&app.db, query_id).await;
    let mut active: arcodash_core::entities::query::ActiveModel = row.into();
    active.latest_query_data_id = Set(Some(result.id));
    active.update(&app.db).await.unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    // Edit the draft to a different query without republishing; resolving against it would miss the
    // cache.
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit draft (unpublished)",
    )
    .await;

    let latest = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/results", app.address))
            .send()
            .await
            .unwrap(),
        "latest_result",
    )
    .await;
    assert_eq!(
        latest["query_result"]["id"].as_i64(),
        Some(result.id),
        "latest_result must return the published result, not 404 or a mismatch caused by \
         resolving against the edited draft"
    );

    // `execute_saved` cache-hits against the published text/hash; `canonicalize` treats empty
    // `parameters` like `Value::Null`.
    let executed = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap(),
        "execute_saved",
    )
    .await;
    assert_eq!(
        executed["query_result"]["id"].as_i64(),
        Some(result.id),
        "execute_saved must cache-hit against the published query's hash, not enqueue a run of \
         the edited (unpublished) draft text: {executed}"
    );
}

/// `refresh` enqueues the PUBLISHED text with `repoint_published: true`, never the pending draft.
#[tokio::test]
async fn refresh_enqueues_published_text_with_repoint_published_true() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(&client, &app.address, "refresh-ds", "Q", "SELECT 1").await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit draft (unpublished)",
    )
    .await;

    let key = QueueName::Queries.queue_key();
    let mut conn = app.redis.clone();
    while conn
        .lpop::<_, Option<String>>(&key, None)
        .await
        .unwrap()
        .is_some()
    {}

    let resp = client
        .post(format!("{}/api/queries/{query_id}/refresh", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    let payload: String = conn
        .lpop::<_, Option<String>>(&key, None)
        .await
        .unwrap()
        .expect("refresh must enqueue a job");
    let job: JobMessage = serde_json::from_str(&payload).unwrap();
    match job {
        JobMessage::ExecuteQuery {
            query_text,
            repoint_published,
            ..
        } => {
            assert_eq!(
                query_text, "SELECT 1",
                "refresh must run the published text, not the pending draft edit"
            );
            assert!(
                repoint_published,
                "refreshing a published query must repoint the published pointer"
            );
        }
        other => panic!("expected ExecuteQuery, got {other:?}"),
    }
}

/// A viewer of a published-pending query is served the PUBLISHED SQL/name/options; the editor still
/// sees the draft.
#[tokio::test]
async fn viewer_gets_published_editor_gets_draft_after_unpublished_edit() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &admin,
        &app.address,
        "viewer-ds",
        "Original Name",
        "SELECT 1",
    )
    .await;

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT 'pending draft edit'",
                "name": "Pending Name",
                "expected_version": 1
            }))
            .send()
            .await
            .unwrap(),
        "edit draft (unpublished)",
    )
    .await;

    let editor_view = get_query(&admin, &app.address, query_id).await;
    assert_eq!(editor_view["query"], "SELECT 'pending draft edit'");
    assert_eq!(editor_view["name"], "Pending Name");

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(arcodash_backend::auth::DEFAULT_ORG_SLUG))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let view_query_group = view_query_group_id(&app.db, org.id).await;
    let mut viewer = new_user(org.id);
    viewer.email = Set("edit-commit-viewer-role@test.local".to_string());
    viewer.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "edit-commit-viewer-role-key",
    ));
    viewer.groups = Set(vec![view_query_group]);
    let viewer = viewer.insert(&app.db).await.unwrap();

    let grant = access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query_id),
        access_type: Set("viewer".to_string()),
        grantor_id: Set(viewer.id),
        grantee_id: Set(Some(viewer.id)),
        grantee_group_id: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
    };
    grant.insert(&app.db).await.unwrap();

    let viewer_client = reqwest::Client::new();
    let resp = viewer_client
        .get(format!(
            "{}/api/queries/{query_id}?api_key={}",
            app.address, "edit-commit-viewer-role-key"
        ))
        .send()
        .await
        .unwrap();
    let viewer_view = ok_json(resp, "viewer GET").await;
    assert_eq!(
        viewer_view["query"], "SELECT 1",
        "a viewer of a published-pending query must never receive the pending draft SQL"
    );
    assert_eq!(viewer_view["name"], "Original Name");
}

/// Unpublish flips `published` to `false` and bumps `working_version`, leaving `published_*` and
/// `changes` history intact.
#[tokio::test]
async fn unpublish_returns_to_draft_and_keeps_publish_history() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "unpublish-ds", "Q", "SELECT 1").await;
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;
    let published_row = get_row(&app.db, query_id).await;
    assert_eq!(published_row.working_version, 1);
    assert!(published_row.published_change_id.is_some());

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
            .json(&serde_json::json!({ "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "unpublish",
    )
    .await;
    assert_eq!(body["published"], false);
    assert_eq!(body["working_version"], 2);

    let row = get_row(&app.db, query_id).await;
    assert!(row.unpublished_at.is_some());
    assert_eq!(
        row.published_change_id, published_row.published_change_id,
        "unpublish must not clear publish history"
    );
    assert_eq!(row.published_query.as_deref(), Some("SELECT 1"));
    assert_eq!(
        changes_count_for_query(&app.db, query_id).await,
        1,
        "unpublish is not a content edit — no new changes row"
    );

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 2 }))
            .send()
            .await
            .unwrap(),
        "republish",
    )
    .await;
    assert_eq!(body["published"], true);
    let row = get_row(&app.db, query_id).await;
    assert!(row.unpublished_at.is_none());
}

#[tokio::test]
async fn unpublish_rejects_a_draft_or_an_already_unpublished_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "unpublish-draft-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
            .json(&serde_json::json!({ "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "unpublish",
    )
    .await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
        .json(&serde_json::json!({ "expected_version": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn unpublish_stale_expected_version_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "unpublish-conflict-ds",
        "Q",
        "SELECT 1",
    )
    .await;
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
        .json(&serde_json::json!({ "expected_version": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
    let row = get_row(&app.db, query_id).await;
    assert!(
        row.unpublished_at.is_none(),
        "a rejected unpublish must not apply"
    );
}

/// Same 404 convention as publish/restore/discard: a viewer-tier grant is not enough.
#[tokio::test]
async fn unpublish_requires_edit_authority_viewer_grant_gets_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&admin, &app.address, "unpublish-403-ds", "Q", "SELECT 1").await;
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish as admin",
    )
    .await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(arcodash_backend::auth::DEFAULT_ORG_SLUG))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let view_query_group = view_query_group_id(&app.db, org.id).await;
    let mut viewer = new_user(org.id);
    viewer.email = Set("unpublish-viewer@test.local".to_string());
    viewer.api_key_hash = Set(arcodash_core::hash::hash_api_key("unpublish-viewer-key"));
    viewer.groups = Set(vec![view_query_group]);
    let viewer = viewer.insert(&app.db).await.unwrap();

    let grant = access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query_id),
        access_type: Set("viewer".to_string()),
        grantor_id: Set(viewer.id),
        grantee_id: Set(Some(viewer.id)),
        grantee_group_id: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
    };
    grant.insert(&app.db).await.unwrap();

    let viewer_client = reqwest::Client::new();
    let resp = viewer_client
        .post(format!(
            "{}/api/queries/{query_id}/unpublish?api_key={}",
            app.address, "unpublish-viewer-key"
        ))
        .json(&serde_json::json!({ "expected_version": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// A workspace member relying on the `visibility: "workspace"` default can read a published query
/// and loses access when it's unpublished (end-to-end over HTTP).
#[tokio::test]
async fn unpublish_revokes_workspace_default_viewer_access() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &admin,
        &app.address,
        "unpublish-visibility-ds",
        "Q",
        "SELECT 1",
    )
    .await;
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish as admin",
    )
    .await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(arcodash_backend::auth::DEFAULT_ORG_SLUG))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let view_query_group = view_query_group_id(&app.db, org.id).await;
    let mut stranger = new_user(org.id);
    stranger.email = Set("unpublish-stranger@test.local".to_string());
    stranger.api_key_hash = Set(arcodash_core::hash::hash_api_key("unpublish-stranger-key"));
    stranger.groups = Set(vec![view_query_group]);
    stranger.insert(&app.db).await.unwrap();

    let stranger_client = reqwest::Client::new();
    let get = |client: reqwest::Client| {
        let address = app.address.clone();
        async move {
            client
                .get(format!(
                    "{address}/api/queries/{query_id}?api_key=unpublish-stranger-key"
                ))
                .send()
                .await
                .unwrap()
        }
    };

    let resp = get(stranger_client.clone()).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a workspace member with no grant must see a published workspace-visibility query"
    );

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
            .json(&serde_json::json!({ "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "unpublish as admin",
    )
    .await;

    let resp = get(stranger_client).await;
    assert_eq!(
        resp.status().as_u16(),
        404,
        "unpublishing must revoke the org-open workspace default the same way a never-published query denies it"
    );
}
