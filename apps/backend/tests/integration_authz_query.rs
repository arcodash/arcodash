//! Capability-ceiling authz integration tests for the query family: `query_service` call sites
//! routed through `authorize`, plus the GET draft-vs-published boundary. Needs `TEST_DATABASE_URL`.

mod common;
use common::default_org_id;
use common::{make_org, set_role_permissions, test_api_key, user_with_role};

use arcodash_backend::authz::authorize::{authorize, evaluate, Action, QueryVerb};
use arcodash_backend::services::{permission_service, query_service};
use arcodash_core::entities::{data_source, query, user};
use sea_orm::{ActiveModelTrait, EntityTrait};

/// Insert a data source + query owned by `owner`, both in `org_id`. Returns the query row.
async fn make_query(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
) -> query::Model {
    let ds_id = data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id;

    let mut active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    // `new_query`'s fixed api_key collides across queries in one shared DB; give each a unique
    // value.
    active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    active.insert(db).await.expect("query insert failed")
}

/// Grant `access_type` ("viewer"/"editor") on `q` to `grantee`, sharing as the query's owner.
async fn grant_on_query(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
    q: &query::Model,
    grantee: &user::Model,
    access_type: &str,
) {
    permission_service::grant(
        db,
        org_id,
        owner,
        "query",
        q.id,
        Some(grantee.id),
        None,
        access_type,
    )
    .await
    .expect("grant failed");
}

#[tokio::test]
async fn update_requires_capability_and_editor_reach() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _owner_role) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    // Owner (Owner reach) WITHOUT edit_query capability -> denied (owner-ceiling).
    let (owner_no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let q2 = make_query(&app.db, org_id, &owner_no_cap).await;
    let denied = query_service::update(
        &app.db,
        &owner_no_cap,
        q2.clone(),
        Some("x".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(denied.is_err(), "owner without edit_query must be denied");

    // Capability holder with NO reach on someone else's query -> denied.
    let (stranger, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let denied2 = query_service::update(
        &app.db,
        &stranger,
        q.clone(),
        Some("y".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        denied2.is_err(),
        "edit_query holder with no reach on this specific query must be denied"
    );

    // Editor GRANT without edit_query capability -> denied (canonical grant-without-capability).
    let (grantee_no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &grantee_no_cap, "editor").await;
    let denied3 = query_service::update(
        &app.db,
        &grantee_no_cap,
        q.clone(),
        Some("z".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        denied3.is_err(),
        "Editor grant without edit_query capability must be denied"
    );

    // Capability + Editor reach (owner who kept the capability) -> allowed.
    let allowed = query_service::update(
        &app.db,
        &owner,
        q.clone(),
        Some("Renamed".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("owner with edit_query must be allowed to update");
    assert_eq!(allowed.name, "Renamed");

    // Capability + Editor GRANT (not owner) -> allowed.
    let (editor_grantee, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &editor_grantee, "editor").await;
    let allowed2 = query_service::update(
        &app.db,
        &editor_grantee,
        allowed,
        Some("Renamed Again".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("edit_query + Editor grant must be allowed");
    assert_eq!(allowed2.name, "Renamed Again");
}

/// Admin bypasses both the capability and reach halves regardless of role/ownership.
#[tokio::test]
async fn admin_bypasses_capability_and_reach() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let allowed = authorize(
        &app.db,
        &admin,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await;
    assert!(
        allowed.is_ok(),
        "admin must bypass capability+reach entirely on a query they neither own nor were granted"
    );
}

/// Delete needs Owner-tier reach on top of `edit_query`; revert only Editor tier, so an Editor-tier
/// grantee can revert but not delete.
#[tokio::test]
async fn delete_requires_owner_tier_reach_revert_stays_editor_tier() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (no_cap_stranger, _) = user_with_role(&app.db, org_id, &[]).await;
    let denied = query_service::delete(&app.db, &no_cap_stranger, q.clone()).await;
    assert!(denied.is_err(), "no capability, no reach -> delete denied");

    let denied_revert = query_service::revert(
        &app.db,
        &no_cap_stranger,
        q.clone(),
        query_service::RevertTarget::Version(1),
        None,
    )
    .await;
    assert!(
        denied_revert.is_err(),
        "no capability, no reach -> revert denied"
    );

    let (editor_grantee, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &editor_grantee, "editor").await;
    let revert_allowed = authorize(
        &app.db,
        &editor_grantee,
        Action::Query {
            verb: QueryVerb::Revert,
            id: q.id,
        },
    )
    .await;
    assert!(
        revert_allowed.is_ok(),
        "Editor-tier grant + edit_query capability must clear the revert gate"
    );

    let denied_delete = query_service::delete(&app.db, &editor_grantee, q.clone()).await;
    assert!(
        denied_delete.is_err(),
        "Editor-tier grant must not be enough to delete (delete requires Owner-tier reach)"
    );

    query_service::delete(&app.db, &owner, q)
        .await
        .expect("owner with edit_query must be allowed to delete");
}

/// Admin bypasses the Owner-tier reach requirement on delete, same as every other action.
#[tokio::test]
async fn delete_admin_bypasses_owner_tier_requirement() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    query_service::delete(&app.db, &admin, q)
        .await
        .expect("admin must be able to delete a query it neither owns nor was granted");
}

#[tokio::test]
async fn publish_denied_for_editor_grant_recipient_without_edit_query_capability() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &grantee, "editor").await;

    let result = query_service::publish(&app.db, &grantee, q.clone(), q.working_version).await;
    assert!(
        result.is_err(),
        "REQUIRED: an Editor-grant recipient lacking edit_query must be denied publish"
    );

    query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("owner with edit_query + Owner reach must be allowed to publish");
}

#[tokio::test]
async fn restore_and_discard_require_capability_and_editor_reach() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    let published = query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish failed");

    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;
    grant_on_query(&app.db, org_id, &owner, &published, &grantee, "editor").await;

    let change_id = published.published_change_id.unwrap();
    let denied = query_service::restore(
        &app.db,
        &grantee,
        published.clone(),
        change_id,
        published.working_version,
    )
    .await;
    assert!(
        denied.is_err(),
        "restore must be denied without edit_query capability"
    );

    let denied_discard = query_service::discard_changes(
        &app.db,
        &grantee,
        published.clone(),
        published.working_version,
    )
    .await;
    assert!(
        denied_discard.is_err(),
        "discard must be denied without edit_query capability"
    );
}

#[tokio::test]
async fn execute_saved_query_requires_capability_and_viewer_reach() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    // `execute` also needs data-source access, independent of query ownership.
    let dsg = arcodash_core::entities::data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: sea_orm::ActiveValue::Set(q.data_source_id),
        group_id: sea_orm::ActiveValue::Set(owner.groups[0]),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    };
    dsg.insert(&app.db)
        .await
        .expect("data_source_group insert failed");

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());
    let redis = app.redis.clone();

    // execute_query capability but NO reach on this specific restricted query -> denied.
    let (stranger, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    let denied = query_service::execute(
        &app.db,
        &queue,
        None,
        &redis,
        org_id,
        &stranger,
        q.data_source_id,
        q.query.clone(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        Some(q.id),
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        denied.is_err(),
        "execute_query capability without reach on a restricted query must be denied"
    );

    let allowed = query_service::execute(
        &app.db,
        &queue,
        None,
        &redis,
        org_id,
        &owner,
        q.data_source_id,
        q.query.clone(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        Some(q.id),
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        allowed.is_ok(),
        "owner with execute_query must be allowed to execute their own query: {allowed:?}"
    );
}

/// `TEST_REDIS_URL`, same default as `common::build_test_state`.
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string())
}

/// Flip a group (unrestricted by default) to restricted so its reach depends on its
/// `data_source_group` allowlist.
async fn restrict_group(db: &sea_orm::DatabaseConnection, group_id: i64) {
    use arcodash_core::entities::group;
    let g = group::Entity::find_by_id(group_id)
        .one(db)
        .await
        .unwrap()
        .expect("group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = sea_orm::ActiveValue::Set(true);
    active.update(db).await.expect("group restrict failed");
}

/// Drain (read + delete) every job on the `queries` queue, parsed as JSON. Destructive by design.
async fn drain_queries_queue() -> Vec<serde_json::Value> {
    use redis::AsyncCommands;
    let mut conn = arcodash_core::valkey_client::connect(&redis_url())
        .await
        .expect("connect to test redis");
    let key = "arcodash:queue:queries";
    let items: Vec<String> = conn.lrange(key, 0, -1).await.unwrap();
    let _: () = conn.del(key).await.unwrap();
    items
        .iter()
        .map(|s| serde_json::from_str(s).unwrap_or_else(|e| panic!("job not JSON ({e}): {s}")))
        .collect()
}

/// An ad-hoc run tagged with `acl_query_id` for a query the caller has an ACL grant on but whose
/// data source they can't reach must surface the friendly `data_source_access_required` error.
#[tokio::test]
async fn ad_hoc_execute_with_acl_grant_but_no_ds_access_gets_friendly_error() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    // Restrict the viewer's group with no allowlist row (groups default unrestricted) to get a real
    // denial.
    let (viewer, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    restrict_group(&app.db, viewer.groups[0]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &viewer, "viewer").await;

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());
    let result = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &viewer,
        q.data_source_id,
        q.query.clone(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        None,
        Some(q.id),
        false,
        10_000,
    )
    .await;

    let err = result.expect_err("must be denied: viewer has no reach on the data source");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        err.error_code.as_deref(),
        Some("data_source_access_required"),
        "explicit grant on the query must surface the friendly error, not the opaque 404: {err:?}"
    );
}

/// With NO ACL grant at all, the opaque error (no `error_code`) must be preserved to avoid an info
/// leak.
#[tokio::test]
async fn ad_hoc_execute_with_no_acl_grant_gets_opaque_error() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    // A stranger with execute_query but no grant, in a restricted group, so the data-source check
    // denies.
    let (stranger, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    restrict_group(&app.db, stranger.groups[0]).await;

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());
    let result = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &stranger,
        q.data_source_id,
        q.query.clone(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        None,
        Some(q.id),
        false,
        10_000,
    )
    .await;

    let err = result.expect_err("must be denied: stranger has no reach on the data source");
    assert_eq!(
        err.error_code, None,
        "no explicit grant on the query must keep the opaque error, not leak its existence: {err:?}"
    );
}

/// Write-back guard: an ad-hoc run tagged with `acl_query_id` for a reachable query must still
/// enqueue with `query_id: null`; `acl_query_id` only steers the friendly-error remap, never the
/// worker's result-pointer write-back.
#[tokio::test]
async fn ad_hoc_execute_with_acl_query_id_never_leaks_into_the_enqueued_query_id() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    // Grant the owner's group data-source reach so the run reaches the enqueue step.
    let dsg = arcodash_core::entities::data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: sea_orm::ActiveValue::Set(q.data_source_id),
        group_id: sea_orm::ActiveValue::Set(owner.groups[0]),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    };
    dsg.insert(&app.db)
        .await
        .expect("data_source_group insert failed");

    drain_queries_queue().await;

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());
    let outcome = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &owner,
        q.data_source_id,
        "SELECT 42 -- ad-hoc, deliberately different from the saved query's SQL".to_string(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        None,
        Some(q.id),
        false,
        20,
    )
    .await
    .expect("owner with real data-source reach must be allowed to run");
    assert!(
        matches!(outcome, query_service::ExecuteOutcome::Enqueued { .. }),
        "max_age=0 must always enqueue, never return a cache hit: {outcome:?}"
    );

    let jobs = drain_queries_queue().await;
    assert_eq!(
        jobs.len(),
        1,
        "exactly one job must be enqueued; got: {jobs:?}"
    );
    let job_query_id = &jobs[0]["query_id"];
    assert!(
        job_query_id.is_null(),
        "acl_query_id must never leak into the enqueued job's query_id \
         (worker write-back must stay gated off for ad-hoc runs): {jobs:?}"
    );
    assert_eq!(
        jobs[0]["max_rows"], 20,
        "execute()'s max_query_rows must ride the job as max_rows: {jobs:?}"
    );
}

/// `POST /api/queries/:id/run` goes through `run_service::run_now`, so it needs its own data-source
/// gate (`ensure_run_data_source_access`): a caller with an ACL grant but no data-source reach gets
/// the friendly `data_source_access_required` error.
#[tokio::test]
async fn run_with_acl_grant_but_no_ds_access_gets_friendly_error() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    let (viewer, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    restrict_group(&app.db, viewer.groups[0]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &viewer, "viewer").await;

    let result =
        query_service::ensure_run_data_source_access(&app.db, &viewer, q.data_source_id, q.id)
            .await;

    let err = result.expect_err("must be denied: viewer has no reach on the data source");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        err.error_code.as_deref(),
        Some("data_source_access_required"),
        "explicit grant on the query must surface the friendly error, not the opaque 404: {err:?}"
    );
}

/// With NO ACL grant, the opaque error (no `error_code`) must be preserved.
#[tokio::test]
async fn run_with_no_acl_grant_gets_opaque_error() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    let (stranger, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    restrict_group(&app.db, stranger.groups[0]).await;

    let result =
        query_service::ensure_run_data_source_access(&app.db, &stranger, q.data_source_id, q.id)
            .await;

    let err = result.expect_err("must be denied: stranger has no reach on the data source");
    assert_eq!(
        err.error_code, None,
        "no explicit grant on the query must keep the opaque error, not leak its existence: {err:?}"
    );
}

/// A caller whose group reaches the data source must still be allowed (no over-denial).
#[tokio::test]
async fn run_with_ds_access_is_allowed() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query", "execute_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    // Groups default unrestricted, so this viewer already reaches the data source.
    let (viewer, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &viewer, "viewer").await;

    let result =
        query_service::ensure_run_data_source_access(&app.db, &viewer, q.data_source_id, q.id)
            .await;
    assert!(
        result.is_ok(),
        "viewer with real data-source reach must be allowed: {result:?}"
    );
}

/// Insert a data source + query owned by `owner` on a caller-supplied `ds_id`.
async fn make_query_on_ds(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
    ds_id: i64,
) -> query::Model {
    let mut active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    active.insert(db).await.expect("query insert failed")
}

/// Add a fresh unrestricted group (grants nothing) to `user`'s memberships, giving one powerless
/// unrestricted group plus one capable restricted group.
async fn add_unrestricted_powerless_group(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    user: user::Model,
) -> user::Model {
    use arcodash_core::entities::group;
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: sea_orm::ActiveValue::Set(org_id),
        name: sea_orm::ActiveValue::Set(format!("unrestricted-{}", uuid::Uuid::new_v4().simple())),
        permissions: sea_orm::ActiveValue::Set(vec![]),
        r#type: sea_orm::ActiveValue::Set("regular".to_string()),
        data_source_access_restricted: sea_orm::ActiveValue::Set(false),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    let mut groups = user.groups.clone();
    groups.push(group_id);
    let mut active: user::ActiveModel = user.into();
    active.groups = sea_orm::ActiveValue::Set(groups);
    active.update(db).await.expect("user update failed")
}

/// `execute`/`run`'s data-source gate must bind the `execute_query` capability and reach to the *same* group:
/// a restricted group's allowlist must not be bypassed by reach borrowed from an unrestricted group.
#[tokio::test]
async fn execute_and_run_denied_when_capability_and_reach_on_different_groups() {
    use arcodash_core::entities::data_source_group;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["edit_query", "execute_query", "create_query"],
    )
    .await;

    let ds_allowed_id =
        data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
            .exec(&app.db)
            .await
            .expect("data source insert failed")
            .last_insert_id;
    let ds_x_id = data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
        .exec(&app.db)
        .await
        .expect("data source insert failed")
        .last_insert_id;

    // A restricted group granting execute_query, allowlisting only ds_allowed (not ds_x).
    let (user, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    let restricted_group_id = user.groups[0];
    restrict_group(&app.db, restricted_group_id).await;
    data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: sea_orm::ActiveValue::Set(ds_allowed_id),
        group_id: sea_orm::ActiveValue::Set(restricted_group_id),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .expect("data_source_group insert failed");

    let user = add_unrestricted_powerless_group(&app.db, org_id, user).await;

    let q_x = make_query_on_ds(&app.db, org_id, &owner, ds_x_id).await;
    let q_allowed = make_query_on_ds(&app.db, org_id, &owner, ds_allowed_id).await;
    grant_on_query(&app.db, org_id, &owner, &q_x, &user, "viewer").await;
    grant_on_query(&app.db, org_id, &owner, &q_allowed, &user, "viewer").await;

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());

    // execute() against ds_x: denied — no single group both grants execute_query and reaches it.
    let denied = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &user,
        ds_x_id,
        q_x.query.clone(),
        q_x.options.clone(),
        serde_json::json!({}),
        Some(0),
        Some(q_x.id),
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        denied.is_err(),
        "the unrestricted group's reach must not lend itself to the restricted group's \
         execute_query grant for a DS outside that group's own allowlist"
    );
    // Query-level reach but no DS-level reach: `friendly_ds_error_for_run` upgrades the bare 404 to
    // a 403.
    assert_eq!(
        denied.unwrap_err().status,
        axum::http::StatusCode::FORBIDDEN
    );

    let allowed = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &user,
        ds_allowed_id,
        q_allowed.query.clone(),
        q_allowed.options.clone(),
        serde_json::json!({}),
        Some(0),
        Some(q_allowed.id),
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        allowed.is_ok(),
        "a DS on the capability-granting group's own allowlist must be allowed: {allowed:?}"
    );

    let run_denied =
        query_service::ensure_run_data_source_access(&app.db, &user, ds_x_id, q_x.id).await;
    assert!(
        run_denied.is_err(),
        "run must deny ds_x for the same reason execute does"
    );

    query_service::ensure_run_data_source_access(&app.db, &user, ds_allowed_id, q_allowed.id)
        .await
        .expect("run must allow the allowlisted data source");

    // Isolation control: removing the unrestricted group changes nothing.
    let mut active: user::ActiveModel = user.into();
    active.groups = sea_orm::ActiveValue::Set(vec![restricted_group_id]);
    let user_isolated = active.update(&app.db).await.expect("user update failed");

    let still_denied =
        query_service::ensure_run_data_source_access(&app.db, &user_isolated, ds_x_id, q_x.id)
            .await;
    assert!(
        still_denied.is_err(),
        "removing the unrestricted group must not change the ds_x denial"
    );
    query_service::ensure_run_data_source_access(
        &app.db,
        &user_isolated,
        ds_allowed_id,
        q_allowed.id,
    )
    .await
    .expect("removing the unrestricted group must not change the allowlisted-DS allow");
}

/// A session with exactly the builtin Viewer permission set is denied everywhere it could write or
/// execute (create, both execute paths, delete, update).
#[tokio::test]
async fn strict_viewer_permission_set_denied_create_execute_delete_update() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_query", "edit_query", "execute_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (viewer, _) = user_with_role(
        &app.db,
        org_id,
        arcodash_core::org_seed::VIEWER_ROLE_PERMISSIONS,
    )
    .await;

    let create_result = query_service::create(
        &app.db,
        org_id,
        &viewer,
        q.data_source_id,
        "viewer-attempt".to_string(),
        None,
        "SELECT 1".to_string(),
        None,
        None,
    )
    .await;
    assert!(
        create_result.is_err(),
        "strict Viewer permission set must be denied create_query"
    );

    let queue = arcodash_core::queue::ValkeyQueue::new(app.redis.clone());

    // ad-hoc execute (POST /api/query_results path: query_id=None)
    let ad_hoc_result = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &viewer,
        q.data_source_id,
        "SELECT 1".to_string(),
        serde_json::json!({}),
        serde_json::json!({}),
        Some(0),
        None,
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        ad_hoc_result.is_err(),
        "strict Viewer permission set must be denied ad-hoc execute"
    );

    // saved execute (POST /api/queries/:id/results path: query_id=Some)
    let saved_result = query_service::execute(
        &app.db,
        &queue,
        None,
        &app.redis,
        org_id,
        &viewer,
        q.data_source_id,
        q.query.clone(),
        q.options.clone(),
        serde_json::json!({}),
        Some(0),
        Some(q.id),
        None,
        false,
        10_000,
    )
    .await;
    assert!(
        saved_result.is_err(),
        "strict Viewer permission set must be denied saved execute"
    );

    // run (POST /api/queries/:id/run path: authorize() Execute verb, same gate as execute_query)
    let run_result = authorize(
        &app.db,
        &viewer,
        Action::Query {
            verb: QueryVerb::Execute,
            id: q.id,
        },
    )
    .await;
    assert!(
        run_result.is_err(),
        "strict Viewer permission set must be denied run"
    );

    let delete_result = query_service::delete(&app.db, &viewer, q.clone()).await;
    assert!(
        delete_result.is_err(),
        "strict Viewer permission set must be denied delete"
    );

    let update_result = query_service::update(
        &app.db,
        &viewer,
        q.clone(),
        Some("renamed-by-viewer".to_string()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        update_result.is_err(),
        "strict Viewer permission set must be denied update/patch"
    );
}

#[tokio::test]
async fn fork_requires_create_query_capability_and_viewer_reach_on_source() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    // create_query capability but no reach on this restricted source -> denied.
    let (no_reach, _) = user_with_role(&app.db, org_id, &["create_query"]).await;
    let denied = query_service::fork(&app.db, org_id, &no_reach, &q).await;
    assert!(
        denied.is_err(),
        "create_query capability without reach on the source must be denied"
    );

    // Viewer reach (a viewer grant) but no create_query capability -> denied.
    let (viewer_no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &viewer_no_cap, "viewer").await;
    let denied2 = query_service::fork(&app.db, org_id, &viewer_no_cap, &q).await;
    assert!(
        denied2.is_err(),
        "Viewer reach without create_query capability must be denied"
    );

    // Both -> allowed.
    let (forker, _) = user_with_role(&app.db, org_id, &["create_query"]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &forker, "viewer").await;
    let forked = query_service::fork(&app.db, org_id, &forker, &q)
        .await
        .expect("create_query + Viewer reach must be allowed to fork");
    assert_eq!(forked.user_id, forker.id);
}

#[tokio::test]
async fn capability_less_editor_grant_recipient_sees_published_not_draft() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;
    let published = query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish failed");

    let edited = query_service::update(
        &app.db,
        &owner,
        published,
        None,
        None,
        Some("SELECT 'draft-only-content'".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("draft edit failed");
    assert_ne!(
        edited.query,
        edited.published_query.clone().unwrap(),
        "test fixture must actually have draft != published content"
    );

    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;
    grant_on_query(&app.db, org_id, &owner, &edited, &grantee, "editor").await;

    let is_editor = authorize(
        &app.db,
        &grantee,
        Action::Query {
            verb: QueryVerb::Edit,
            id: edited.id,
        },
    )
    .await
    .is_ok();
    assert!(
        !is_editor,
        "REQUIRED: capability-less Editor-grant recipient must not be treated as editor"
    );

    let mut served = edited.clone();
    if !is_editor {
        query_service::overlay_serving_content(&mut served);
    }
    assert_eq!(
        served.query,
        edited.published_query.unwrap(),
        "REQUIRED: capability-less Editor-grant recipient must receive published SQL, not draft"
    );

    let owner_is_editor = authorize(
        &app.db,
        &owner,
        Action::Query {
            verb: QueryVerb::Edit,
            id: edited.id,
        },
    )
    .await
    .is_ok();
    assert!(
        owner_is_editor,
        "owner with edit_query must be treated as editor"
    );
}

#[tokio::test]
async fn evaluate_reports_object_role_and_can_edit_consistently_with_authorize() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, role_id) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query(&app.db, org_id, &owner).await;

    let result = evaluate(
        &app.db,
        &owner,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert!(result.can_edit);
    assert_eq!(
        result.object_role,
        Some(arcodash_backend::authz::access::ObjectRole::Owner)
    );

    set_role_permissions(&app.db, role_id, &["view_query"]).await;
    let result2 = evaluate(
        &app.db,
        &owner,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert!(
        !result2.can_edit,
        "owner-ceiling: losing edit_query must flip can_edit false even though still owner"
    );
    assert_eq!(
        result2.object_role,
        Some(arcodash_backend::authz::access::ObjectRole::Owner),
        "object_role reflects ownership regardless of capability"
    );
}

/// Fills in the `caller`-block matrix: Editor grant with/without capability, admin and no-access,
/// each round-tripped through `Caller::from` to pin the JSON.
#[tokio::test]
async fn caller_block_covers_grant_admin_and_no_access_cases() {
    use arcodash_backend::authz::access::ObjectRole;
    use arcodash_backend::authz::authorize::Caller;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_query", "edit_query", "view_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, &owner).await;

    let (editor_grantee, _) = user_with_role(&app.db, org_id, &["edit_query", "view_query"]).await;
    grant_on_query(&app.db, org_id, &owner, &q, &editor_grantee, "editor").await;
    let eval = evaluate(
        &app.db,
        &editor_grantee,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(
        eval.can_edit,
        "Editor grant + edit_query capability must allow edit"
    );
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "editor");
    assert!(caller.can_edit && !caller.can_delete);
    assert_eq!(caller.can_share, None, "queries have no can_share field");

    // Editor grant WITHOUT edit_query capability -> Editor role, can_edit false.
    let (editor_grantee_no_cap, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    grant_on_query(
        &app.db,
        org_id,
        &owner,
        &q,
        &editor_grantee_no_cap,
        "editor",
    )
    .await;
    let eval = evaluate(
        &app.db,
        &editor_grantee_no_cap,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(
        !eval.can_edit,
        "Editor grant without edit_query must deny edit"
    );
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "editor");
    assert!(!caller.can_edit && !caller.can_delete);

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let eval = evaluate(
        &app.db,
        &admin,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);
    assert_eq!(Caller::from(eval).object_role, "owner");

    // True no-access: capability held, but no ownership/grant/workspace-visibility reach at all.
    let (stranger, _) = user_with_role(&app.db, org_id, &["edit_query", "view_query"]).await;
    let eval = evaluate(
        &app.db,
        &stranger,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(
        eval.object_role, None,
        "no ownership/grant/visibility reach -> None"
    );
    assert!(!eval.can_edit);
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "none");
    assert!(!caller.can_edit && !caller.can_delete);
}

/// `GET /api/queries/:id/consumers` must apply the same gate as `GET /api/queries/:id`: a user who
/// 404s on the query must 404 identically on `consumers`.
#[tokio::test]
async fn consumers_denies_the_same_user_that_the_query_itself_denies() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;
    // A real HTTP request resolves its org via the default-org fallback, so the user must live
    // there or API-key auth 401s first.
    let org_id = default_org_id(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["view_query", "edit_query", "create_query"],
    )
    .await;
    let q = make_query(&app.db, org_id, &owner).await;

    // A stranger with capability but whose only group is restricted with no allowlist entry, and no
    // ACL grant.
    let (stranger, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    restrict_group(&app.db, stranger.groups[0]).await;

    let key = test_api_key(&stranger.email);
    let http = reqwest::Client::new();

    let get_resp = http
        .get(format!(
            "{}/api/queries/{}?api_key={key}",
            app.address, q.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        404,
        "no reach must 404 the query itself"
    );
    let get_body: serde_json::Value = get_resp.json().await.unwrap_or_default();

    let consumers_resp = http
        .get(format!(
            "{}/api/queries/{}/consumers?api_key={key}",
            app.address, q.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        consumers_resp.status().as_u16(),
        404,
        "consumers must apply the same gate as the query read, not a bare View check"
    );
    let consumers_body: serde_json::Value = consumers_resp.json().await.unwrap_or_default();
    assert_eq!(
        get_body, consumers_body,
        "the two 404s must be indistinguishable to the caller"
    );

    let owner_key = test_api_key(&owner.email);
    let owner_consumers = http
        .get(format!(
            "{}/api/queries/{}/consumers?api_key={owner_key}",
            app.address, q.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        owner_consumers.status().as_u16(),
        200,
        "the owner must still list consumers"
    );
}
