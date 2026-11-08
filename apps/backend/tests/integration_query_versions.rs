//! Integration tests: query version history, revert, and fork. `changes` rows are appended only by
//! `POST /publish`; revert restores a publish-point onto the draft columns without growing history
//! or touching `published_*`.

mod common;
use common::ok_json;
use common::{make_org, spawn_app, user_with_role};

use sea_orm::EntityTrait;

use arcodash_backend::db::Db;
use arcodash_backend::services::display_names;
use arcodash_core::entities::query::Entity as QueryEntity;

/// Create a data source and a never-published draft query; returns `(data_source_id, query_id)`.
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

async fn get_history(client: &reqwest::Client, address: &str, query_id: i64) -> serde_json::Value {
    ok_json(
        client
            .get(format!("{address}/api/queries/{query_id}/version"))
            .send()
            .await
            .unwrap(),
        "get version history",
    )
    .await
}

async fn publish(
    client: &reqwest::Client,
    address: &str,
    query_id: i64,
    expected_version: i64,
) -> serde_json::Value {
    ok_json(
        client
            .post(format!("{address}/api/queries/{query_id}/publish"))
            .json(&serde_json::json!({ "expected_version": expected_version }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await
}

/// Create then two draft updates → history stays empty.
#[tokio::test]
async fn create_and_updates_record_no_history() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "novers-ds", "Versioned", "SELECT 1").await;

    let created = get_query(&client, &app.address, query_id).await;
    assert_eq!(created["working_version"], 0);
    assert_eq!(created["published"], false);
    assert_eq!(created["has_unpublished_changes"], false);

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2" }))
            .send()
            .await
            .unwrap(),
        "first update",
    )
    .await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "name": "Versioned v2" }))
            .send()
            .await
            .unwrap(),
        "second update",
    )
    .await;

    let history = get_history(&client, &app.address, query_id).await;
    assert_eq!(
        history.as_array().unwrap().len(),
        0,
        "create + updates must record zero history entries: {history}"
    );

    // working_version bumps on every draft write even though history stays empty.
    let after = get_query(&client, &app.address, query_id).await;
    assert_eq!(after["working_version"], 2);
    assert_eq!(after["published"], false);
}

/// History gets one entry per publish, not per draft edit.
#[tokio::test]
async fn version_history_grows_only_on_publish() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "publish-ver-ds",
        "Versioned",
        "SELECT 1",
    )
    .await;

    let published = publish(&client, &app.address, query_id, 0).await;
    assert_eq!(published["published"], true);
    assert_eq!(published["has_unpublished_changes"], false);
    assert_eq!(published["working_version"], 1);

    let history_after_first = get_history(&client, &app.address, query_id).await;
    let entries = history_after_first.as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "first publish must record exactly one history entry: {history_after_first}"
    );
    assert_eq!(entries[0]["version"], 1);
    assert_eq!(entries[0]["change"]["query"], "SELECT 1");
    assert_eq!(entries[0]["change"]["name"], "Versioned");
    // The dialog must show a name, not a raw "User #<id>" — resolved at read time from `user_id`.
    assert_eq!(
        entries[0]["user_name"].as_str(),
        Some("Test Admin"),
        "expected the author's display name alongside user_id: {entries:#?}"
    );

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "edit after publish",
    )
    .await;

    let mid = get_query(&client, &app.address, query_id).await;
    assert_eq!(mid["has_unpublished_changes"], true);
    assert_eq!(mid["working_version"], 2);

    let history_unchanged = get_history(&client, &app.address, query_id).await;
    assert_eq!(
        history_unchanged.as_array().unwrap().len(),
        1,
        "a draft edit alone must not grow history"
    );

    let republished = publish(&client, &app.address, query_id, 2).await;
    assert_eq!(republished["has_unpublished_changes"], false);
    assert_eq!(republished["working_version"], 3);

    let history_after_second = get_history(&client, &app.address, query_id).await;
    let entries = history_after_second.as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "publish changes must append a second history entry: {history_after_second}"
    );
    assert_eq!(entries[1]["version"], 2);
    assert_eq!(entries[1]["change"]["query"], "SELECT 2");
}

/// POST .../revert {version:1} → draft columns match the snapshot and working_version bumps;
/// history length and `published_*` columns (checked in the DB row) are untouched.
#[tokio::test]
async fn revert_by_version_restores_draft_without_appending_history_or_touching_published() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "revert-ver-ds",
        "Original Name",
        "SELECT 1",
    )
    .await;

    publish(&client, &app.address, query_id, 0).await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "name": "Changed Name", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "update 1",
    )
    .await;
    publish(&client, &app.address, query_id, 2).await;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 3", "expected_version": 3 }))
            .send()
            .await
            .unwrap(),
        "update 2",
    )
    .await;

    let before = get_history(&client, &app.address, query_id).await;
    assert_eq!(
        before.as_array().unwrap().len(),
        2,
        "expected 2 entries before revert (one per publish)"
    );

    let reverted = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/revert", app.address))
            .json(&serde_json::json!({ "version": 1 }))
            .send()
            .await
            .unwrap(),
        "revert to version 1",
    )
    .await;

    assert_eq!(reverted["query"], "SELECT 1", "query text must match v1");
    assert_eq!(reverted["name"], "Original Name", "name must match v1");
    assert_eq!(
        reverted["working_version"], 5,
        "revert bumps working_version like any draft write"
    );

    let live = get_query(&client, &app.address, query_id).await;
    assert_eq!(live["query"], "SELECT 1");
    assert_eq!(live["name"], "Original Name");
    assert_eq!(
        live["has_unpublished_changes"], true,
        "draft now differs from the still-latest publish (v2)"
    );

    let after = get_history(&client, &app.address, query_id).await;
    let after_entries = after.as_array().unwrap();
    assert_eq!(
        after_entries.len(),
        2,
        "revert must not append a history entry: {after}"
    );
    let before_entries = before.as_array().unwrap();
    for i in 0..2 {
        assert_eq!(
            after_entries[i]["change_id"], before_entries[i]["change_id"],
            "history entry {i} must be unchanged after revert"
        );
    }

    let row = QueryEntity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("query row must exist");
    assert_eq!(row.published_query.as_deref(), Some("SELECT 2"));
    assert_eq!(row.published_name.as_deref(), Some("Changed Name"));
}

/// Concurrent-edit race: client A reverts while still believing it's at v after B already saved.
/// Revert is CAS-guarded, so A gets 409 with the actual `working_version` and B's content survives.
#[tokio::test]
async fn revert_conflicts_when_expected_version_is_stale() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "revert-cas-ds",
        "Original Name",
        "SELECT 1",
    )
    .await;
    publish(&client, &app.address, query_id, 0).await;
    // Draft at working_version=1; a second publish gives a version 1 snapshot to revert to.
    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 2", "expected_version": 1 }))
            .send()
            .await
            .unwrap(),
        "seed edit",
    )
    .await;
    let published_v2 = publish(&client, &app.address, query_id, 2).await;
    let v = published_v2["working_version"].as_i64().unwrap();

    let b_saved = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 'from B'", "expected_version": v }))
            .send()
            .await
            .unwrap(),
        "client B save",
    )
    .await;
    assert_eq!(b_saved["working_version"], v + 1);

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 1, "expected_version": v }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "a stale expected_version on revert must conflict, not silently overwrite B's save"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["working_version"],
        v + 1,
        "409 body must report the row's actual current version"
    );

    let live = get_query(&client, &app.address, query_id).await;
    assert_eq!(live["query"], "SELECT 'from B'");
    assert_eq!(live["working_version"], v + 1);
}

/// A revert with the current `expected_version` succeeds and bumps the version by one.
#[tokio::test]
async fn revert_with_correct_expected_version_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "revert-cas-ok-ds",
        "Original Name",
        "SELECT 1",
    )
    .await;
    let published = publish(&client, &app.address, query_id, 0).await;
    let v = published["working_version"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 1, "expected_version": v }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "correct expected_version must succeed"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["working_version"], v + 1);
}

/// A revert without `expected_version` still succeeds (backward compatible).
#[tokio::test]
async fn revert_without_expected_version_still_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "revert-cas-omit-ds",
        "Original Name",
        "SELECT 1",
    )
    .await;
    publish(&client, &app.address, query_id, 0).await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "omitted expected_version must not block the revert"
    );
}

/// POST .../revert with an out-of-range version (999) → 404. Works even with zero history.
#[tokio::test]
async fn revert_out_of_range_version_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "revert-oor-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "out-of-range version must be 404: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// POST .../revert with neither change_id nor version → 400.
#[tokio::test]
async fn revert_with_neither_field_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "revert-neither-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "revert with neither field must be 400: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// POST .../revert with both change_id and version → 400 (ambiguous), before any snapshot lookup.
#[tokio::test]
async fn revert_with_both_fields_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "revert-both-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "change_id": 1, "version": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "revert with both fields must be 400: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// POST .../revert with version < 1 (0) → 400.
#[tokio::test]
async fn revert_version_less_than_one_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_query(&client, &app.address, "revert-zero-ds", "Q", "SELECT 1").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "version < 1 must be 400: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// POST .../fork → 200, new draft "Copy of <name>" owned by caller, with its own empty history.
#[tokio::test]
async fn fork_creates_draft_copy_with_correct_fields() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (ds_id, query_id) = seed_query(
        &client,
        &app.address,
        "fork-ds",
        "Source Query",
        "SELECT 42",
    )
    .await;

    let source = publish(&client, &app.address, query_id, 0).await;
    assert_eq!(
        source["published"], true,
        "source should be published before fork"
    );
    let source_user_id = source["user_id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "type": "CHART",
                "name": "Source Chart",
                "options": {"globalSeriesType": "line"}
            }))
            .send()
            .await
            .unwrap(),
        "create source visualization",
    )
    .await;

    let fork = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/fork", app.address))
            .send()
            .await
            .unwrap(),
        "fork query",
    )
    .await;

    assert_eq!(
        fork["name"], "Copy of Source Query",
        "fork name must be 'Copy of <name>'"
    );
    assert_eq!(fork["published"], false, "fork must start unpublished");
    assert_eq!(fork["query"], "SELECT 42", "fork must copy the query text");
    assert_eq!(
        fork["data_source_id"], ds_id,
        "fork must copy the data source"
    );
    assert_eq!(
        fork["user_id"].as_i64().unwrap(),
        source_user_id,
        "fork owned by caller"
    );

    assert_ne!(
        fork["id"].as_i64().unwrap(),
        query_id,
        "fork must have a distinct id"
    );

    let source_after = get_query(&client, &app.address, query_id).await;
    assert_eq!(
        source_after["name"], "Source Query",
        "source name must be unchanged"
    );
    assert_eq!(
        source_after["published"], true,
        "source publish state must be unchanged"
    );

    // Fork has no history of its own.
    let fork_id = fork["id"].as_i64().unwrap();
    let fork_history = get_history(&client, &app.address, fork_id).await;
    assert_eq!(
        fork_history.as_array().unwrap().len(),
        0,
        "fork starts with zero history entries: {fork_history}"
    );

    // Fork copies every source visualization, retargeted at the new query.
    let source_visualizations = ok_json(
        client
            .get(format!(
                "{}/api/queries/{query_id}/visualizations",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list source visualizations",
    )
    .await;
    let fork_visualizations = ok_json(
        client
            .get(format!(
                "{}/api/queries/{fork_id}/visualizations",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list fork visualizations",
    )
    .await;
    assert_eq!(
        fork_visualizations.as_array().unwrap().len(),
        source_visualizations.as_array().unwrap().len(),
        "fork must copy the source's visualization count"
    );
    assert_eq!(
        fork_visualizations[0]["type"], "CHART",
        "fork's copied visualization must preserve type"
    );
    assert_eq!(
        fork_visualizations[0]["name"], "Source Chart",
        "fork's copied visualization must preserve name"
    );
    assert_ne!(
        fork_visualizations[0]["id"], source_visualizations[0]["id"],
        "fork's visualization must be a distinct row, not shared with the source"
    );
}

/// GET .../version on an inaccessible (nonexistent) query → 404.
#[tokio::test]
async fn version_history_on_inaccessible_query_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/queries/999999/version", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "version history on nonexistent query must be 404: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// `display_names::resolve_user_names` is org-scoped: a user id from another org never resolves to
/// a name.
#[tokio::test]
async fn resolve_user_names_does_not_leak_across_orgs() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let session = ok_json(
        client
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let home_org_id = session["org"]["id"].as_i64().expect("org id in session");

    let other_org_id = make_org(&app.db).await;
    let (other_org_user, _role_id) = user_with_role(&app.db, other_org_id, &[]).await;
    let db = Db::Pool(app.db.clone());

    let names = display_names::resolve_user_names(&db, home_org_id, [other_org_user.id])
        .await
        .expect("resolve_user_names failed");
    assert!(
        names.is_empty(),
        "a user id from another org must not resolve to a name: {names:?}"
    );

    // Same id scoped to its own org resolves, so the empty result above is the org filter at work.
    let names_same_org = display_names::resolve_user_names(&db, other_org_id, [other_org_user.id])
        .await
        .expect("resolve_user_names failed");
    assert_eq!(
        names_same_org.get(&other_org_user.id),
        Some(&other_org_user.name)
    );
}
