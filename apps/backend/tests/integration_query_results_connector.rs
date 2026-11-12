//! Integration tests for the "Query Results" meta-connector's tenancy and authorization.
//! `load_result_data` must filter on `org_id` (the DB role bypasses RLS) and fail closed without
//! `org_id`/`user_id`; within one org, rows are visible only to callers who could view the
//! underlying query (`can_view_query_like`). Skipped when `TEST_DATABASE_URL` is unset.

mod common;
use common::spawn_app;

use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::EntityTrait;
use serde_json::json;

use arcodash_backend::services::query_service;
use arcodash_core::connectors::query_results::QueryResultsConnector;
use arcodash_core::connectors::QueryRunner;
use arcodash_core::entities::{query_result, user};
use arcodash_core::hash::gen_query_hash;
use arcodash_core::test_support;

/// A label plus a random suffix, so re-runs against a persistent Postgres don't collide on unique
/// indexes (these writes aren't covered by `TestApp::cleanup`).
fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::new_v4().simple())
}

/// Seed a fresh org with one user, one `pg` data source and one query; returns `(org_id,
/// data_source_id, query_id, query_hash, owner_user_id)`.
async fn seed_org_and_query(
    db: &sea_orm::DatabaseConnection,
    label: &str,
    query_text: &str,
) -> (i64, i64, i64, String, i64) {
    let mut org = test_support::new_organization();
    org.slug = Set(unique(label));
    let org = org.insert(db).await.expect("insert org");

    let mut user = test_support::new_user(org.id);
    user.api_key_hash = Set(arcodash_core::hash::hash_api_key(&unique(&format!(
        "{label}-user-key"
    ))));
    let user = user.insert(db).await.expect("insert user");

    let ds = test_support::new_data_source(org.id)
        .insert(db)
        .await
        .expect("insert data source");

    let mut query = test_support::new_query(org.id, ds.id, user.id);
    query.query = Set(query_text.to_string());
    let query_hash = gen_query_hash(query_text, &json!({}));
    query.query_hash = Set(query_hash.clone());
    query.api_key = Set(unique(&format!("{label}-query-key")));
    let query = query.insert(db).await.expect("insert query");

    (org.id, ds.id, query.id, query_hash, user.id)
}

/// Like the above plus one cached `query_results` row; returns `(org_id, query_id, owner_user_id)`.
/// The connector exposes it as the SQLite table `query_<query_id>`.
async fn seed_org_with_result(
    db: &sea_orm::DatabaseConnection,
    label: &str,
    query_text: &str,
    column_name: &str,
    value: &str,
) -> (i64, i64, i64) {
    let (org_id, ds_id, query_id, query_hash, owner_id) =
        seed_org_and_query(db, label, query_text).await;
    seed_result(
        db,
        org_id,
        ds_id,
        &query_hash,
        query_text,
        column_name,
        value,
    )
    .await;
    (org_id, query_id, owner_id)
}

/// Insert one cached `query_results` row for `(org_id, data_source_id, query_hash)`.
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query_text: &str,
    column_name: &str,
    value: &str,
) {
    let now = chrono::Utc::now().into();
    let result = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query_text.to_string()),
        data: Set(Some(json!({
            "columns": [{"name": column_name, "friendly_type": "string"}],
            "rows": [{column_name: value}],
        }))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        last_used_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    };
    result.insert(db).await.expect("seed query_result");
}

/// Flip a group to restricted (allowlist-only) data-source access; groups default unrestricted.
async fn restrict_group(db: &sea_orm::DatabaseConnection, group_id: i64) {
    use arcodash_core::entities::group;
    let g = group::Entity::find_by_id(group_id)
        .one(db)
        .await
        .unwrap()
        .expect("group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(true);
    active.update(db).await.expect("group restrict failed");
}

/// Put `data_source_id` on `group_id`'s allowlist.
async fn assign_group(db: &sea_orm::DatabaseConnection, data_source_id: i64, group_id: i64) {
    use arcodash_core::entities::data_source_group;
    data_source_group::ActiveModel {
        id: NotSet,
        data_source_id: Set(data_source_id),
        group_id: Set(group_id),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(db)
    .await
    .expect("data_source_group insert failed");
}

/// Grant `grantee_id` `access_type` on `('query', query_id)` via `access_permissions`.
async fn grant_access(
    db: &sea_orm::DatabaseConnection,
    query_id: i64,
    grantee_id: i64,
    access_type: &str,
) {
    use arcodash_core::entities::access_permission;
    access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query_id),
        access_type: Set(access_type.to_string()),
        grantor_id: Set(grantee_id),
        grantee_id: Set(Some(grantee_id)),
        grantee_group_id: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(db)
    .await
    .expect("access_permission insert failed");
}

/// Insert a query owned by `owner` and, if `visibility` is `Some`, publish it via the real
/// `query_service::publish`. `owner` must hold `edit_query`. Returns the query row and its
/// `query_hash`.
async fn seed_query(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    owner: &user::Model,
    query_text: &str,
    visibility: Option<&str>,
) -> (arcodash_core::entities::query::Model, String) {
    let mut q = test_support::new_query(org_id, data_source_id, owner.id);
    q.query = Set(query_text.to_string());
    let query_hash = gen_query_hash(query_text, &json!({}));
    q.query_hash = Set(query_hash.clone());
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    if let Some(v) = visibility {
        q.visibility = Set(v.to_string());
    }
    let q = q.insert(db).await.expect("query insert failed");
    let q = if visibility.is_some() {
        query_service::publish(db, owner, q, 0)
            .await
            .expect("publish failed")
    } else {
        q
    };
    (q, query_hash)
}

#[tokio::test]
async fn get_schema_only_returns_calling_orgs_tables() {
    let app = skip_if_none!(spawn_app().await);

    let (org_a, query_a, owner_a) =
        seed_org_with_result(&app.db, "qrds-schema-a", "SELECT 1 AS a", "a", "1").await;
    let (org_b, query_b, owner_b) =
        seed_org_with_result(&app.db, "qrds-schema-b", "SELECT 2 AS b", "b", "2").await;

    let connector = QueryResultsConnector::new(app.db.clone());

    let tables_a = connector
        .get_schema(&json!({ "org_id": org_a, "user_id": owner_a }))
        .await
        .expect("get_schema for org A");
    let names_a: Vec<String> = tables_a.into_iter().map(|t| t.name).collect();
    assert!(
        names_a.contains(&format!("query_{query_a}")),
        "org A must see its own table; got {names_a:?}"
    );
    assert!(
        !names_a.contains(&format!("query_{query_b}")),
        "org A must NOT see org B's table; got {names_a:?}"
    );

    let tables_b = connector
        .get_schema(&json!({ "org_id": org_b, "user_id": owner_b }))
        .await
        .expect("get_schema for org B");
    let names_b: Vec<String> = tables_b.into_iter().map(|t| t.name).collect();
    assert!(
        names_b.contains(&format!("query_{query_b}")),
        "org B must see its own table; got {names_b:?}"
    );
    assert!(
        !names_b.contains(&format!("query_{query_a}")),
        "org B must NOT see org A's table; got {names_b:?}"
    );
}

#[tokio::test]
async fn run_query_cannot_read_another_orgs_cached_result() {
    let app = skip_if_none!(spawn_app().await);

    let (org_a, query_a, owner_a) =
        seed_org_with_result(&app.db, "qrds-run-a", "SELECT 1 AS a", "a", "1").await;
    let (_org_b, query_b, _owner_b) =
        seed_org_with_result(&app.db, "qrds-run-b", "SELECT 2 AS b", "b", "2").await;

    let connector = QueryResultsConnector::new(app.db.clone());

    let own = connector
        .run_query(
            &json!({ "org_id": org_a, "user_id": owner_a }),
            &format!("SELECT * FROM query_{query_a}"),
        )
        .await
        .expect("run_query against own table must succeed");
    assert_eq!(own.rows.len(), 1);

    // Org B's table was never loaded into org A's SQLite, so querying it is a SQL error.
    let other = connector
        .run_query(
            &json!({ "org_id": org_a, "user_id": owner_a }),
            &format!("SELECT * FROM query_{query_b}"),
        )
        .await;
    assert!(
        other.is_err(),
        "org A must not be able to read org B's cached query_{query_b} table"
    );
}

/// `query_hash` is derived from query text and params only, so two orgs can share a hash. A join
/// filtering only `queries.org_id` would pair org A's query with org B's cached result;
/// `load_result_data` must also require `qr.org_id = $1` and `q.org_id = qr.org_id`.
#[tokio::test]
async fn hash_collision_across_orgs_does_not_leak_data() {
    let app = skip_if_none!(spawn_app().await);

    let shared_query_text = "SELECT 'shared-text' AS x";

    let (org_a, _ds_a, query_a, _hash_a, owner_a) =
        seed_org_and_query(&app.db, "qrds-collide-a", shared_query_text).await;

    let (_org_b, _query_b, _owner_b) = seed_org_with_result(
        &app.db,
        "qrds-collide-b",
        shared_query_text,
        "secret",
        "org-b-secret-value",
    )
    .await;

    let connector = QueryResultsConnector::new(app.db.clone());

    // Org A has no cached results: get_schema must return no tables, not org B's under A's query
    // id.
    let tables_a = connector
        .get_schema(&json!({ "org_id": org_a, "user_id": owner_a }))
        .await
        .expect("get_schema for org A");
    assert!(
        tables_a.is_empty(),
        "org A has no cached query_results of its own; a hash collision with org B must not \
         manufacture a table — got {tables_a:?}"
    );

    let leaked = connector
        .run_query(
            &json!({ "org_id": org_a, "user_id": owner_a }),
            &format!("SELECT * FROM query_{query_a}"),
        )
        .await;
    assert!(
        leaked.is_err(),
        "org A must not be able to read org B's cached data via a query_hash collision \
         (query_{query_a} must not exist for org A)"
    );
}

#[tokio::test]
async fn missing_org_id_in_opts_fails_closed() {
    let app = skip_if_none!(spawn_app().await);
    let connector = QueryResultsConnector::new(app.db.clone());

    let err = connector.get_schema(&json!({})).await.unwrap_err();
    assert!(
        err.to_string().contains("org_id"),
        "get_schema error should mention org_id: {err}"
    );

    let err = connector
        .run_query(&json!({}), "SELECT 1")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("org_id"),
        "run_query error should mention org_id: {err}"
    );
}

/// The `user_id` sibling: a caller that forgets `user_id` must be refused, not treated as
/// anonymous.
#[tokio::test]
async fn missing_user_id_in_opts_fails_closed() {
    let app = skip_if_none!(spawn_app().await);
    let connector = QueryResultsConnector::new(app.db.clone());

    let err = connector
        .get_schema(&json!({ "org_id": 1 }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("user_id"),
        "get_schema error should mention user_id: {err}"
    );

    let err = connector
        .run_query(&json!({ "org_id": 1 }), "SELECT 1")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("user_id"),
        "run_query error should mention user_id: {err}"
    );
}

/// Fixture: one org, an "author" who publishes queries, and two data sources each with a published
/// workspace query and cached result.
#[allow(dead_code)] // author/ds_denied document the fixture even where a given test doesn't read them
struct AuthzFixture {
    org_id: i64,
    author: user::Model,
    ds_allowed: i64,
    ds_denied: i64,
    query_allowed: i64,
    query_denied: i64,
}

async fn build_authz_fixture(db: &sea_orm::DatabaseConnection, label: &str) -> AuthzFixture {
    let org_id = common::make_org(db).await;
    let (author, _role_id) = common::user_with_role(
        db,
        org_id,
        &["create_query", "view_query", "edit_query", "execute_query"],
    )
    .await;

    let ds_allowed = test_support::new_data_source(org_id)
        .insert(db)
        .await
        .expect("ds_allowed insert failed")
        .id;
    let ds_denied = test_support::new_data_source(org_id)
        .insert(db)
        .await
        .expect("ds_denied insert failed")
        .id;

    let (q_allowed, hash_allowed) = seed_query(
        db,
        org_id,
        ds_allowed,
        &author,
        &format!("SELECT 1 AS a -- {label}-allowed"),
        Some("workspace"),
    )
    .await;
    seed_result(
        db,
        org_id,
        ds_allowed,
        &hash_allowed,
        &q_allowed.query,
        "a",
        "1",
    )
    .await;

    let (q_denied, hash_denied) = seed_query(
        db,
        org_id,
        ds_denied,
        &author,
        &format!("SELECT 2 AS b -- {label}-denied"),
        Some("workspace"),
    )
    .await;
    seed_result(
        db,
        org_id,
        ds_denied,
        &hash_denied,
        &q_denied.query,
        "b",
        "2",
    )
    .await;

    AuthzFixture {
        org_id,
        author,
        ds_allowed,
        ds_denied,
        query_allowed: q_allowed.id,
        query_denied: q_denied.id,
    }
}

/// A restricted-group user sees and reads only the query over the allowlisted data source, though
/// both are published workspace queries.
#[tokio::test]
async fn restricted_group_cannot_read_query_over_denied_data_source() {
    let app = skip_if_none!(spawn_app().await);
    let fx = build_authz_fixture(&app.db, "restricted").await;

    let (caller, _role_id) = common::user_with_role(&app.db, fx.org_id, &[]).await;
    let group_id = caller.groups[0];
    restrict_group(&app.db, group_id).await;
    assign_group(&app.db, fx.ds_allowed, group_id).await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": fx.org_id, "user_id": caller.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        names.contains(&format!("query_{}", fx.query_allowed)),
        "must see the query over the in-scope data source; got {names:?}"
    );
    assert!(
        !names.contains(&format!("query_{}", fx.query_denied)),
        "must NOT see the query over the out-of-scope data source; got {names:?}"
    );

    let denied = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", fx.query_denied))
        .await;
    assert!(
        denied.is_err(),
        "reading the denied data source's cached results must fail, not return rows"
    );

    let allowed = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", fx.query_allowed))
        .await
        .expect("reading the in-scope data source's cached results must succeed");
    assert_eq!(allowed.rows.len(), 1);
}

/// Org admin bypasses the data-source scope gate and reads every published workspace query.
#[tokio::test]
async fn admin_sees_and_reads_every_query() {
    let app = skip_if_none!(spawn_app().await);
    let fx = build_authz_fixture(&app.db, "admin").await;

    let (admin, _role_id) = common::user_with_role(&app.db, fx.org_id, &["admin"]).await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": fx.org_id, "user_id": admin.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(names.contains(&format!("query_{}", fx.query_allowed)));
    assert!(names.contains(&format!("query_{}", fx.query_denied)));

    for query_id in [fx.query_allowed, fx.query_denied] {
        connector
            .run_query(&opts, &format!("SELECT * FROM query_{query_id}"))
            .await
            .unwrap_or_else(|e| panic!("admin must be able to read query_{query_id}: {e}"));
    }
}

/// An explicit `access_permissions` grant is unconditional reach and overrides the data-source
/// denial; scope only gates the default.
#[tokio::test]
async fn explicit_grant_overrides_data_source_denial() {
    let app = skip_if_none!(spawn_app().await);
    let fx = build_authz_fixture(&app.db, "grant").await;

    let (caller, _role_id) = common::user_with_role(&app.db, fx.org_id, &[]).await;
    let group_id = caller.groups[0];
    restrict_group(&app.db, group_id).await;
    assign_group(&app.db, fx.ds_allowed, group_id).await;

    grant_access(&app.db, fx.query_denied, caller.id, "view").await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": fx.org_id, "user_id": caller.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        names.contains(&format!("query_{}", fx.query_denied)),
        "an explicit grant must make the table visible despite the data-source denial; got {names:?}"
    );

    connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", fx.query_denied))
        .await
        .expect("an explicit grant must make the table readable despite the data-source denial");
}

/// An `access_permissions` row with an unrecognized `access_type` grants nothing (fail-closed).
#[tokio::test]
async fn unrecognized_access_type_grants_nothing() {
    let app = skip_if_none!(spawn_app().await);
    let fx = build_authz_fixture(&app.db, "bogus-grant").await;

    let (caller, _role_id) = common::user_with_role(&app.db, fx.org_id, &[]).await;
    let group_id = caller.groups[0];
    restrict_group(&app.db, group_id).await;
    assign_group(&app.db, fx.ds_allowed, group_id).await;

    grant_access(&app.db, fx.query_denied, caller.id, "bogus").await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": fx.org_id, "user_id": caller.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&format!("query_{}", fx.query_denied)),
        "an unrecognized access_type must not grant visibility; got {names:?}"
    );

    let denied = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", fx.query_denied))
        .await;
    assert!(denied.is_err());
}

/// Another user's draft query is not visible via the org-open default even with full data-source
/// access: `is_draft` gates independently.
#[tokio::test]
async fn drafters_own_query_not_readable_by_another_user_despite_data_source_access() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = common::make_org(&app.db).await;

    let (drafter, _role_id) = common::user_with_role(
        &app.db,
        org_id,
        &["create_query", "view_query", "edit_query"],
    )
    .await;
    let ds = test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");

    let (q, hash) = seed_query(&app.db, org_id, ds.id, &drafter, "SELECT 3 AS c", None).await;
    seed_result(&app.db, org_id, ds.id, &hash, &q.query, "c", "3").await;

    // Unrestricted group (full data-source reach), but no ownership or grant on this query.
    let (caller, _role_id) = common::user_with_role(&app.db, org_id, &[]).await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": org_id, "user_id": caller.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&format!("query_{}", q.id)),
        "a draft query must not be visible to a non-owner with no grant; got {names:?}"
    );

    let denied = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", q.id))
        .await;
    assert!(denied.is_err());
}

/// Ownership is unconditional reach: the owner of a private draft can read their own cached result.
#[tokio::test]
async fn owner_always_sees_own_query_regardless_of_visibility() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = common::make_org(&app.db).await;

    let (owner, _role_id) = common::user_with_role(
        &app.db,
        org_id,
        &["create_query", "view_query", "edit_query"],
    )
    .await;
    let ds = test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");

    let (q, hash) = seed_query(&app.db, org_id, ds.id, &owner, "SELECT 4 AS d", None).await;
    seed_result(&app.db, org_id, ds.id, &hash, &q.query, "d", "4").await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": org_id, "user_id": owner.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        names.contains(&format!("query_{}", q.id)),
        "the owner must always see their own query; got {names:?}"
    );

    let own = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", q.id))
        .await
        .expect("the owner must always be able to read their own cached result");
    assert_eq!(own.rows.len(), 1);
}

/// A `query_results` row whose `data_source_id` matches neither the query's draft nor published
/// data source must not be served under that query's table, even to its owner.
#[tokio::test]
async fn join_excludes_result_row_from_a_different_data_source() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = common::make_org(&app.db).await;

    let (owner, _role_id) = common::user_with_role(
        &app.db,
        org_id,
        &["create_query", "view_query", "edit_query"],
    )
    .await;
    let ds_a = test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds_a insert failed");
    let ds_b = test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds_b insert failed");

    let (q, hash) = seed_query(&app.db, org_id, ds_a.id, &owner, "SELECT 5 AS e", None).await;
    // The cached row's data_source_id is ds_b, which is neither q.data_source_id (ds_a) nor a
    // published one.
    seed_result(&app.db, org_id, ds_b.id, &hash, &q.query, "e", "5").await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": org_id, "user_id": owner.id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must succeed");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&format!("query_{}", q.id)),
        "a result row from an unrelated data source must not surface under the query's table \
         name, even to its owner; got {names:?}"
    );

    let err = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", q.id))
        .await;
    assert!(err.is_err(), "the table must not exist at all");
}

/// Like [`seed_result`] but takes the full `data` JSON, for the NULL/numeric typing tests.
async fn seed_result_with_data(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query_text: &str,
    data: serde_json::Value,
) {
    let now = chrono::Utc::now().into();
    let result = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query_text.to_string()),
        data: Set(Some(data)),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        last_used_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    };
    result.insert(db).await.expect("seed query_result");
}

/// A cached `NULL` must bind as SQLite `NULL`, not the string `"null"`, and numbers as numbers so
/// `WHERE n > 5` works.
#[tokio::test]
async fn null_and_numeric_values_bind_with_their_real_sqlite_types() {
    let app = skip_if_none!(spawn_app().await);
    let (org_id, ds_id, query_id, query_hash, owner_id) =
        seed_org_and_query(&app.db, "qrds-null-types", "SELECT * FROM t").await;

    seed_result_with_data(
        &app.db,
        org_id,
        ds_id,
        &query_hash,
        "SELECT * FROM t",
        json!({
            "columns": [
                { "name": "c", "friendly_type": "string" },
                { "name": "n", "friendly_type": "integer" }
            ],
            "rows": [
                { "c": "null", "n": 3 },
                { "c": null, "n": 10 }
            ]
        }),
    )
    .await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": org_id, "user_id": owner_id });
    let table = format!("query_{query_id}");

    let null_count = connector
        .run_query(
            &opts,
            &format!("SELECT COUNT(*) AS n FROM {table} WHERE c IS NULL"),
        )
        .await
        .expect("IS NULL query failed");
    assert_eq!(
        null_count.rows[0]["n"],
        json!(1),
        "IS NULL must match the real null, not the string \"null\""
    );

    let string_null_count = connector
        .run_query(
            &opts,
            &format!("SELECT COUNT(*) AS n FROM {table} WHERE c = 'null'"),
        )
        .await
        .expect("string 'null' query failed");
    assert_eq!(
        string_null_count.rows[0]["n"],
        json!(1),
        "a genuine \"null\" string must stay distinguishable from a real null"
    );

    let count_col = connector
        .run_query(&opts, &format!("SELECT COUNT(c) AS n FROM {table}"))
        .await
        .expect("COUNT(c) query failed");
    assert_eq!(
        count_col.rows[0]["n"],
        json!(1),
        "COUNT(col) must not count the NULL row"
    );

    let numeric_cmp = connector
        .run_query(
            &opts,
            &format!("SELECT COUNT(*) AS n FROM {table} WHERE n > 5"),
        )
        .await
        .expect("numeric comparison query failed");
    assert_eq!(
        numeric_cmp.rows[0]["n"],
        json!(1),
        "numeric comparison must work on a bound integer"
    );
}

/// A cached result with empty `columns` can't become a SQLite table and is skipped without taking
/// down other queries in the same call.
#[tokio::test]
async fn a_result_with_no_columns_is_skipped_without_breaking_other_tables() {
    let app = skip_if_none!(spawn_app().await);
    let (org_id, ds_id, empty_query_id, empty_hash, owner_id) =
        seed_org_and_query(&app.db, "qrds-empty-cols", "SELECT * FROM empty").await;
    seed_result_with_data(
        &app.db,
        org_id,
        ds_id,
        &empty_hash,
        "SELECT * FROM empty",
        json!({ "columns": [], "rows": [] }),
    )
    .await;

    let mut ok_query = test_support::new_query(org_id, ds_id, owner_id);
    ok_query.query = Set("SELECT 1 AS a".to_string());
    let ok_hash = gen_query_hash("SELECT 1 AS a", &json!({}));
    ok_query.query_hash = Set(ok_hash.clone());
    ok_query.api_key = Set(unique("qrds-empty-cols-ok-query-key"));
    let ok_query = ok_query.insert(&app.db).await.expect("insert ok query");
    seed_result(&app.db, org_id, ds_id, &ok_hash, "SELECT 1 AS a", "a", "1").await;

    let connector = QueryResultsConnector::new(app.db.clone());
    let opts = json!({ "org_id": org_id, "user_id": owner_id });

    let tables = connector
        .get_schema(&opts)
        .await
        .expect("get_schema must not fail because of the empty-column result");
    let names: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&format!("query_{empty_query_id}")),
        "the empty-column query must not appear in the schema: {names:?}"
    );
    assert!(
        names.contains(&format!("query_{}", ok_query.id)),
        "the other, well-formed query must still appear: {names:?}"
    );

    let result = connector
        .run_query(&opts, &format!("SELECT * FROM query_{}", ok_query.id))
        .await
        .expect("run_query against the well-formed table must still succeed");
    assert_eq!(result.rows.len(), 1);
}
