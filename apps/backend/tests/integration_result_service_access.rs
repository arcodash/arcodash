//! Integration tests for `result_service::get` (`GET /api/query_results/<id>` and
//! `.../<id>.<format>`) — the data-source gate requires execute-tier reach AND a query-object
//! reach check applies for any result whose parent query is identifiable. Otherwise any
//! org member merely in a data-source viewer group could walk sequential `query_results.id`
//! values and read the full row data of every result ever produced against that source,
//! including results belonging to `restricted`-visibility queries they could never otherwise see
//! or execute.

mod common;
use common::{api_key_client, default_org_id};
use common::{spawn_app, test_api_key, user_with_role};

use arcodash_backend::services::{permission_service, query_service};
use arcodash_core::entities::{data_source, query, query_result, user};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

async fn make_ds(db: &sea_orm::DatabaseConnection, org_id: i64) -> i64 {
    data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id
}

/// Insert a query owned by `owner` on `ds_id`, in `org_id`.
async fn make_query(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    ds_id: i64,
    owner: &user::Model,
) -> query::Model {
    let mut active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    active.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    active.insert(db).await.expect("query insert failed")
}

/// Insert a `query_results` row directly (no worker) and return its id.
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
) -> i64 {
    let now = chrono::Utc::now();
    query_result::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(serde_json::json!({
            "columns": [{"name": "x"}],
            "rows": [{"x": 1}]
        }))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(db)
    .await
    .expect("query_result insert failed")
    .id
}

/// Publish `q` — the org-open "workspace" default only grants View reach to a *published*
/// query; an unpublished draft stays owner/grant-only regardless of its `visibility` column
/// (see `query_service::list_for_user`'s docs). Returns the updated row.
async fn publish_query(
    db: &sea_orm::DatabaseConnection,
    owner: &user::Model,
    q: query::Model,
) -> query::Model {
    let expected = q.working_version;
    query_service::publish(db, owner, q, expected)
        .await
        .expect("publish failed")
}

/// Point `q.latest_query_data_id` at `result_id` — stands in for a completed execution since this
/// harness runs no in-process worker.
async fn point_at_result(db: &sea_orm::DatabaseConnection, query_id: i64, result_id: i64) {
    let existing = query::Entity::find_by_id(query_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut active: query::ActiveModel = existing.into();
    active.latest_query_data_id = Set(Some(result_id));
    active.update(db).await.unwrap();
}

async fn get_result(client: &reqwest::Client, address: &str, id: i64) -> u16 {
    client
        .get(format!("{address}/api/query_results/{id}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn get_result_csv(client: &reqwest::Client, address: &str, id: i64) -> u16 {
    client
        .get(format!("{address}/api/query_results/{id}.csv"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A `restricted`-visibility query's result must 404 for a same-org, same-data-source-
/// reach user with no ACL grant on the query — even though they hold `execute_query` and could
/// see the data source itself. The owner must still see it.
#[tokio::test]
async fn restricted_query_result_is_hidden_from_a_user_with_no_grant_but_visible_to_the_owner() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["execute_query", "view_query", "edit_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;
    point_at_result(&app.db, q.id, result_id).await;
    permission_service::set_visibility(&app.db, org_id, &owner, "query", q.id, "restricted")
        .await
        .expect("set_visibility failed");

    let (stranger, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;

    let owner_client = api_key_client(&test_api_key(&owner.email));
    let stranger_client = api_key_client(&test_api_key(&stranger.email));

    assert_eq!(
        get_result(&stranger_client, &app.address, result_id).await,
        404,
        "a same-org user with data-source reach but no grant on a restricted query's result must 404"
    );
    assert_eq!(
        get_result_csv(&stranger_client, &app.address, result_id).await,
        404,
        "the .csv download variant must be gated identically"
    );
    assert_eq!(
        get_result(&owner_client, &app.address, result_id).await,
        200,
        "the query's owner must still see their own result"
    );
}

/// Same setup, but the query's visibility is left at the default `workspace` — the
/// stranger must now see it. Proves the gate doesn't break the legitimate org-open case.
#[tokio::test]
async fn workspace_query_result_is_visible_to_any_org_member_with_execute_reach() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["execute_query", "view_query", "edit_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    let q = publish_query(&app.db, &owner, q).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;
    point_at_result(&app.db, q.id, result_id).await;
    // visibility left at its default ("workspace")

    let (other, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;
    let other_client = api_key_client(&test_api_key(&other.email));

    assert_eq!(
        get_result(&other_client, &app.address, result_id).await,
        200,
        "a workspace-visibility query's result must remain visible to any org member with \
         execute-tier data-source reach"
    );
}

/// A result with no identifiable parent query (an ad-hoc execute — no query anywhere
/// shares its `(data_source_id, query_hash)`, and no `latest_query_data_id`/`runs` row points at
/// it) must be visible to any caller who already holds execute-tier reach on its data source —
/// they could reproduce these exact rows by submitting the same SQL themselves.
#[tokio::test]
async fn ad_hoc_result_with_no_parent_query_is_visible_given_execute_reach() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let unique_hash = format!("no-parent-{}", uuid::Uuid::new_v4().simple());
    let result_id = seed_result(&app.db, org_id, ds_id, &unique_hash).await;

    let (user, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;
    let client = api_key_client(&test_api_key(&user.email));

    assert_eq!(
        get_result(&client, &app.address, result_id).await,
        200,
        "an orphaned/ad-hoc result must be visible to a caller with execute reach on its data source"
    );
}

/// A viewer-tier user (data-source reach but NO `execute_query` capability) must be
/// denied even for a `workspace`-visibility query's result — proves the execute-tier data-source
/// requirement, not just the query-reach check, is actually enforced.
#[tokio::test]
async fn viewer_tier_user_without_execute_capability_is_denied_even_for_a_workspace_query() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;
    point_at_result(&app.db, q.id, result_id).await;
    // visibility left at its default ("workspace")

    // view_query only — no execute_query capability at all.
    let (viewer, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let viewer_client = api_key_client(&test_api_key(&viewer.email));

    assert_eq!(
        get_result(&viewer_client, &app.address, result_id).await,
        404,
        "a caller with data-source reach but no execute_query capability must be denied"
    );
}

/// An admin sees a `restricted`-visibility query's result regardless of grants.
#[tokio::test]
async fn admin_sees_a_restricted_query_result_regardless_of_grants() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["execute_query", "view_query", "edit_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;
    point_at_result(&app.db, q.id, result_id).await;
    permission_service::set_visibility(&app.db, org_id, &owner, "query", q.id, "restricted")
        .await
        .expect("set_visibility failed");

    // An admin *within this same org* — `result_service::get` is org-scoped even for admins
    // (it filters `result.org_id == user.org_id` before any admin bypass), so the global
    // `admin_client()` (a different, seeded org) would not be a valid probe here.
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let admin_client = api_key_client(&test_api_key(&admin.email));
    assert_eq!(
        get_result(&admin_client, &app.address, result_id).await,
        200,
        "an admin must see any result in their own org regardless of grants/visibility"
    );
}

/// The `query_hash` candidate arm — a result whose `(data_source_id, query_hash)` matches
/// a restricted query's, but which is reachable through NEITHER `latest_query_data_id`/
/// `published_latest_query_data_id` NOR a `runs` row. This is the "two users ran byte-identical
/// SQL, one saved it as a restricted query" case: the interactive-execute result and the saved
/// query never got explicitly linked, so only the hash match ties them together.
#[tokio::test]
async fn query_hash_arm_gates_a_result_reachable_only_by_hash_match_to_a_restricted_query() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["execute_query", "view_query", "edit_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    permission_service::set_visibility(&app.db, org_id, &owner, "query", q.id, "restricted")
        .await
        .expect("set_visibility failed");

    // Same `(data_source_id, query_hash)` as `q` (both default to "SELECT 1" / `{}`), but never
    // pointed at by `q.latest_query_data_id`/`published_latest_query_data_id`, and no `runs` row
    // links them — the hash match is the only thing connecting this result to `q`.
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;

    let (stranger, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;
    let stranger_client = api_key_client(&test_api_key(&stranger.email));
    let owner_client = api_key_client(&test_api_key(&owner.email));

    assert_eq!(
        get_result(&stranger_client, &app.address, result_id).await,
        404,
        "the query_hash arm must find `q` as a candidate and deny a caller with no grant on it, \
         even though no pointer/runs row links the result to `q`"
    );
    assert_eq!(
        get_result(&owner_client, &app.address, result_id).await,
        200,
        "the query_hash arm must let the query's owner see a result only reachable through it"
    );
}

/// The dashboard-widget flow — `GET /api/queries/<id>/results` for a
/// workspace-visibility query, as a non-owner org member — must still return 200. This is the
/// legitimate case the execute-tier data-source requirement must not break.
#[tokio::test]
async fn dashboard_widget_flow_get_latest_result_for_non_owner_workspace_query_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = default_org_id(&app.db).await;
    let ds_id = make_ds(&app.db, org_id).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["execute_query", "view_query", "edit_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, ds_id, &owner).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &q.query_hash).await;
    point_at_result(&app.db, q.id, result_id).await;
    // `latest_result` serves a non-editor the *published* pointer, not the draft's — publish
    // AFTER pointing the draft so first-publish seeds `published_latest_query_data_id` from it.
    let q = query_service::get_for_user(&app.db, org_id, &owner, q.id)
        .await
        .expect("reload before publish");
    let q = publish_query(&app.db, &owner, q).await;

    let (non_owner, _) = user_with_role(&app.db, org_id, &["execute_query", "view_query"]).await;
    let non_owner_client = api_key_client(&test_api_key(&non_owner.email));

    let resp = non_owner_client
        .get(format!("{}/api/queries/{}/results", app.address, q.id))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "dashboard-widget flow: a non-owner with execute reach on a workspace query must still \
         see its latest result"
    );
}
