//! `GET /api/queries/favorites` returns the same `caller` block as `GET /api/queries/{id}` for
//! the same query and same user.

mod common;
use common::{spawn_app, test_api_key, user_with_role};

#[tokio::test]
async fn favorites_caller_matches_direct_get() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({"name": "test-pg", "type": "pg", "options": {}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Fav Test Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let q_id = q["id"].as_i64().unwrap();

    let star = client
        .post(format!("{}/api/queries/{}/favorite", app.address, q_id))
        .send()
        .await
        .unwrap();
    assert_eq!(star.status().as_u16(), 204, "star query");

    let direct = client
        .get(format!("{}/api/queries/{}", app.address, q_id))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    println!("direct GET caller: {}", direct["caller"]);

    let favs = client
        .get(format!("{}/api/queries/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let fav_entry = favs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_i64() == Some(q_id))
        .expect("favorited query present in favorites list");
    println!("favorites GET caller: {}", fav_entry["caller"]);

    assert_eq!(
        direct["caller"], fav_entry["caller"],
        "caller block must match between GET /api/queries/{{id}} and GET /api/queries/favorites"
    );
}

/// A query the caller has favorited but subsequently loses visibility to (its visibility flips
/// from workspace-open to restricted, with no explicit grant) must simply drop out of
/// `GET /api/queries/favorites`, not error the whole response — each favorited id's visibility
/// is resolved directly (bounded by favorite count) rather than by filtering an
/// already-loaded org-wide list, and an invisible favorite must keep dropping out.
#[tokio::test]
async fn favorites_list_still_excludes_a_query_the_caller_cannot_see() {
    use arcodash_core::entities::{organization, query};
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .id;

    let admin = app.admin_client().await;
    let ds = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({"name": "fav-visibility-ds", "type": "pg", "options": {}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Fav Visibility Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let q_id = q["id"].as_i64().unwrap();
    admin
        .post(format!("{}/api/queries/{q_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();

    let (user, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let key = test_api_key(&user.email);
    let http = reqwest::Client::new();

    let star = http
        .post(format!(
            "{}/api/queries/{q_id}/favorite?api_key={key}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(star.status().as_u16(), 204, "star query");

    let before = http
        .get(format!(
            "{}/api/queries/favorites?api_key={key}",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        before
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_i64() == Some(q_id)),
        "the favorited query must be visible before its visibility is revoked: {before}"
    );

    // Flip the query to restricted with no explicit grant — the caller loses the workspace-open
    // default that was the only thing making it visible.
    let row = query::Entity::find_by_id(q_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("query must exist");
    let mut active: query::ActiveModel = row.into();
    active.visibility = sea_orm::ActiveValue::Set("restricted".to_string());
    active
        .update(&app.db)
        .await
        .expect("visibility update failed");

    let after_resp = http
        .get(format!(
            "{}/api/queries/favorites?api_key={key}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after_resp.status().as_u16(),
        200,
        "the favorites list must still succeed once one favorite becomes invisible"
    );
    let after = after_resp.json::<serde_json::Value>().await.unwrap();
    assert!(
        !after
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_i64() == Some(q_id)),
        "a favorited query the caller can no longer see must be dropped from the list: {after}"
    );
}

/// A non-editor who favorites a published query must be served the published SQL/name in
/// `GET /api/queries/favorites`, never a draft edit sitting on top of it — the same overlay
/// `GET /api/queries/{id}` applies via `can_edit`.
#[tokio::test]
async fn favorites_list_serves_published_content_to_a_non_editor() {
    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .id;

    let admin = app.admin_client().await;
    let ds = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({"name": "fav-overlay-ds", "type": "pg", "options": {}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Published Name",
            "query": "SELECT 1 AS published_column",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let q_id = q["id"].as_i64().unwrap();

    admin
        .post(format!("{}/api/queries/{q_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();

    // Draft edit on top of the published content — never republished, so `published_*` still
    // carries the original name/SQL.
    let draft_update = admin
        .post(format!("{}/api/queries/{q_id}", app.address))
        .json(&serde_json::json!({
            "name": "Draft Name (unpublished)",
            "query": "SELECT 2 AS draft_column"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(draft_update.status().as_u16(), 200, "draft update");

    let (user, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let key = test_api_key(&user.email);
    let http = reqwest::Client::new();

    let star = http
        .post(format!(
            "{}/api/queries/{q_id}/favorite?api_key={key}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(star.status().as_u16(), 204, "star query");

    let favs = http
        .get(format!(
            "{}/api/queries/favorites?api_key={key}",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let entry = favs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_i64() == Some(q_id))
        .expect("favorited query present in favorites list");

    assert_eq!(
        entry["caller"]["can_edit"],
        serde_json::json!(false),
        "the non-editor caller must not have edit access: {entry}"
    );
    assert_eq!(
        entry["name"], "Published Name",
        "a non-editor must see the published name, not the unpublished draft: {entry}"
    );
    assert_eq!(
        entry["query"], "SELECT 1 AS published_column",
        "a non-editor must see the published SQL, not the unpublished draft: {entry}"
    );
}

/// A favorited-but-archived query must not reappear in `GET /api/queries/favorites`, matching
/// `GET /api/queries` (`?include_archived` defaults to excluding archived rows).
#[tokio::test]
async fn favorites_list_excludes_an_archived_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({"name": "fav-archived-ds", "type": "pg", "options": {}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Fav Archived Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let q_id = q["id"].as_i64().unwrap();

    let star = client
        .post(format!("{}/api/queries/{}/favorite", app.address, q_id))
        .send()
        .await
        .unwrap();
    assert_eq!(star.status().as_u16(), 204, "star query");

    let before = client
        .get(format!("{}/api/queries/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        before
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_i64() == Some(q_id)),
        "the favorited query must be visible before it is archived: {before}"
    );

    let archive = client
        .post(format!("{}/api/queries/{q_id}", app.address))
        .json(&serde_json::json!({ "is_archived": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(archive.status().as_u16(), 200, "archive query");

    let after = client
        .get(format!("{}/api/queries/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        !after
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_i64() == Some(q_id)),
        "a favorited-but-archived query must not appear in the favorites list: {after}"
    );
}
