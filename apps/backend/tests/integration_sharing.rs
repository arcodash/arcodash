//! Integration tests for sharing and collaboration, run at the service layer. Invariants: existing
//! read access survives rollout; restricted dashboards need an explicit grant; draft dashboards are
//! private like draft queries.

mod common;
use common::{make_org, spawn_app, user_with_role};

use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
};

use arcodash_backend::authz::access;
use arcodash_backend::services::{
    dashboard_service, embed_service, permission_service, query_service, widget_service,
};
use arcodash_core::entities::{
    access_permission, dashboard, data_source_group, embed, group, group_role, monitor,
    organization, query, role, user,
};
use arcodash_core::test_support::{
    new_dashboard, new_data_source, new_group, new_organization, new_query, new_user,
};

/// Shared fixture: default org, one data source, one group with Full Access to it, and two members
/// (`a`, `b`) in that group.
struct Fixture {
    org_id: i64,
    data_source_id: i64,
    a: user::Model,
    b: user::Model,
}

async fn build_fixture(db: &sea_orm::DatabaseConnection, tag: &str) -> Fixture {
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(db)
        .await
        .unwrap()
        .expect("default org must exist after migrations");

    let ds = new_data_source(org.id).insert(db).await.unwrap();

    let group = new_group(org.id, &format!("shared-group-{tag}"), vec![])
        .insert(db)
        .await
        .unwrap();

    let dsg = data_source_group::ActiveModel {
        id: NotSet,
        data_source_id: Set(ds.id),
        group_id: Set(group.id),
        created_at: NotSet,
        updated_at: NotSet,
    };
    dsg.insert(db).await.unwrap();

    // Exercises object-access (grants/visibility), so the shared group gets a role carrying the
    // needed capabilities.
    let cap_role = role::ActiveModel {
        org_id: Set(org.id),
        name: Set(format!("sharing-test-role-{tag}")),
        permissions: Set(vec![
            "view_query".to_string(),
            "edit_query".to_string(),
            "create_query".to_string(),
            "execute_query".to_string(),
            "view_dashboard".to_string(),
            "edit_dashboard".to_string(),
            "create_dashboard".to_string(),
            "view_monitor".to_string(),
            "edit_monitor".to_string(),
        ]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    group_role::ActiveModel {
        group_id: Set(group.id),
        role_id: Set(cap_role.id),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    let mut a = new_user(org.id);
    a.email = Set(format!("owner-a-{tag}@test.local"));
    a.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!(
        "sharing-test-a-{tag}"
    )));
    a.groups = Set(vec![group.id]);
    let a = a.insert(db).await.unwrap();

    let mut b = new_user(org.id);
    b.email = Set(format!("member-b-{tag}@test.local"));
    b.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!(
        "sharing-test-b-{tag}"
    )));
    b.groups = Set(vec![group.id]);
    let b = b.insert(db).await.unwrap();

    Fixture {
        org_id: org.id,
        data_source_id: ds.id,
        a,
        b,
    }
}

async fn insert_published_query(
    db: &sea_orm::DatabaseConnection,
    fx: &Fixture,
    tag: &str,
) -> query::Model {
    let mut q = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    q.api_key = Set(format!("sharing-test-query-{tag}"));
    let q = q.insert(db).await.unwrap();
    query_service::publish(db, &fx.a, q, 0).await.unwrap()
}

/// A dashboard with a populated `published_snapshot` (empty widget set), without going through
/// `dashboard_service::publish`.
async fn insert_published_dashboard(
    db: &sea_orm::DatabaseConnection,
    fx: &Fixture,
    tag: &str,
) -> dashboard::Model {
    let mut d = new_dashboard(fx.org_id, fx.a.id);
    d.slug = Set(format!("sharing-test-dashboard-{tag}"));
    let inserted = d.insert(db).await.unwrap();
    let snapshot = serde_json::json!({
        "name": inserted.name,
        "layout": inserted.layout,
        "options": inserted.options,
        "tags": inserted.tags.clone().unwrap_or_default(),
        "dashboard_filters_enabled": inserted.dashboard_filters_enabled,
        "widgets": [],
    });
    let mut active: dashboard::ActiveModel = inserted.into();
    active.published_snapshot = Set(Some(snapshot));
    active.update(db).await.unwrap()
}

/// A brand-new dashboard: never published (`published_snapshot` `None`).
async fn insert_default_dashboard(
    db: &sea_orm::DatabaseConnection,
    fx: &Fixture,
    tag: &str,
) -> dashboard::Model {
    let mut d = new_dashboard(fx.org_id, fx.a.id);
    d.slug = Set(format!("sharing-test-dashboard-{tag}"));
    d.insert(db).await.unwrap()
}

/// Rollout invariant: a published query and an already-published dashboard at default `workspace`
/// visibility stay readable by B, a plain member with no explicit grant.
#[tokio::test]
async fn migration_preserves_read_access() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "invariant").await;
    let q = insert_published_query(&app.db, &fx, "invariant").await;
    let d = insert_published_dashboard(&app.db, &fx, "invariant").await;
    assert!(
        dashboard_service::is_published(&d),
        "sanity: the backfill leaves every pre-existing dashboard published"
    );

    let queries = query_service::list_for_user(&app.db, fx.org_id, &fx.b, false, None, None, None)
        .await
        .expect("list queries for B");
    assert!(
        queries.iter().any(|x| x.id == q.id),
        "B must still see A's published workspace query in the list after the migration"
    );

    let dashboards = dashboard_service::list(&app.db, fx.org_id, &fx.b, false, None, None, None)
        .await
        .expect("list dashboards for B");
    assert!(
        dashboards.iter().any(|x| x.id == d.id),
        "B must still see A's published workspace dashboard in the list after the migration"
    );

    query_service::get_for_user(&app.db, fx.org_id, &fx.b, q.id)
        .await
        .expect("B must still be able to GET A's published workspace query");
    dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
        .await
        .expect("B must still be able to GET A's published workspace dashboard");
}

/// Restricting a published dashboard 404s B; a `viewer` grant restores access; revoking blocks
/// again.
#[tokio::test]
async fn restricted_dashboard_enforcement() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "restricted").await;
    let d = insert_published_dashboard(&app.db, &fx, "restricted").await;

    dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
        .await
        .expect("B must see the dashboard before it's restricted");

    let mut active: dashboard::ActiveModel = d.clone().into();
    active.visibility = Set("restricted".to_string());
    active.update(&app.db).await.unwrap();

    let denied = dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id).await;
    assert!(
        denied.is_err(),
        "B must be denied once the dashboard is restricted"
    );
    assert_eq!(
        denied.unwrap_err().status,
        axum::http::StatusCode::NOT_FOUND,
        "restricted-dashboard denial must be a 404, not a distinguishable 403"
    );

    // Grant B a viewer role directly, seeding the row as-is.
    let now = chrono::Utc::now();
    let grant = access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("dashboard".to_string()),
        object_id: Set(d.id),
        access_type: Set("viewer".to_string()),
        grantor_id: Set(fx.a.id),
        grantee_id: Set(Some(fx.b.id)),
        grantee_group_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
        .await
        .expect("B must be able to read after being granted viewer");

    access_permission::Entity::delete_by_id(grant.id)
        .exec(&app.db)
        .await
        .unwrap();

    let denied_again = dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id).await;
    assert!(
        denied_again.is_err(),
        "B must be blocked again once the viewer grant is revoked"
    );
    assert_eq!(
        denied_again.unwrap_err().status,
        axum::http::StatusCode::NOT_FOUND
    );
}

/// A draft query is invisible to B until published; a dashboard behaves the same, and `restricted`
/// hides it again.
#[tokio::test]
async fn draft_privacy_query_and_dashboard() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "draft").await;

    let mut qa = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    qa.api_key = Set("sharing-test-query-draft".to_string());
    let q = qa.insert(&app.db).await.unwrap();
    assert!(
        q.published_change_id.is_none(),
        "query must start unpublished"
    );

    let before_list =
        query_service::list_for_user(&app.db, fx.org_id, &fx.b, false, None, None, None)
            .await
            .unwrap();
    assert!(
        !before_list.iter().any(|x| x.id == q.id),
        "B must not see A's draft query in the list"
    );
    assert!(
        query_service::get_for_user(&app.db, fx.org_id, &fx.b, q.id)
            .await
            .is_err(),
        "B must not be able to GET A's draft query"
    );

    query_service::publish(&app.db, &fx.a, q.clone(), 0)
        .await
        .unwrap();

    let after_list =
        query_service::list_for_user(&app.db, fx.org_id, &fx.b, false, None, None, None)
            .await
            .unwrap();
    assert!(
        after_list.iter().any(|x| x.id == q.id),
        "B must see A's query in the list once published"
    );
    query_service::get_for_user(&app.db, fx.org_id, &fx.b, q.id)
        .await
        .expect("B must be able to GET A's query once published");

    let d = insert_default_dashboard(&app.db, &fx, "draft-exempt").await;
    assert!(
        !dashboard_service::is_published(&d),
        "dashboard must start never-published"
    );

    let dash_list = dashboard_service::list(&app.db, fx.org_id, &fx.b, false, None, None, None)
        .await
        .unwrap();
    assert!(
        !dash_list.iter().any(|x| x.id == d.id),
        "B must not see A's never-published dashboard in the list"
    );
    assert!(
        dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
            .await
            .is_err(),
        "B must not be able to GET A's never-published dashboard"
    );

    widget_service::create(
        &app.db,
        fx.org_id,
        &fx.a,
        d.id,
        None,
        Some("A tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("A must be able to add a widget to their own dashboard");
    let d = dashboard_service::publish(&app.db, &fx.a, d)
        .await
        .expect("A must be able to publish their own dashboard");

    let dash_list_after_publish =
        dashboard_service::list(&app.db, fx.org_id, &fx.b, false, None, None, None)
            .await
            .unwrap();
    assert!(
        dash_list_after_publish.iter().any(|x| x.id == d.id),
        "B must see A's dashboard in the list once published"
    );
    dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
        .await
        .expect("B must be able to GET A's dashboard once published");

    let mut active: dashboard::ActiveModel = d.clone().into();
    active.visibility = Set("restricted".to_string());
    active.update(&app.db).await.unwrap();

    let restricted_list =
        dashboard_service::list(&app.db, fx.org_id, &fx.b, false, None, None, None)
            .await
            .unwrap();
    assert!(
        !restricted_list.iter().any(|x| x.id == d.id),
        "B must not see the dashboard in the list once restricted"
    );
    assert!(
        dashboard_service::get_by_id(&app.db, fx.org_id, &fx.b, d.id)
            .await
            .is_err(),
        "B must not be able to GET the dashboard once restricted"
    );
}

/// A group grant (not just a user grant) flows through `object_role`.
#[tokio::test]
async fn grant_editor_to_group_gives_member_editor_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "group-grant").await;

    let mut qa = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    qa.api_key = Set("sharing-test-query-group-grant".to_string());
    qa.visibility = Set("restricted".to_string());
    let q = qa.insert(&app.db).await.unwrap();
    let q = query_service::publish(&app.db, &fx.a, q, 0).await.unwrap();

    let editors_group = new_group(fx.org_id, "editors-group-grant", vec![])
        .insert(&app.db)
        .await
        .unwrap();
    let mut b_active: user::ActiveModel = fx.b.clone().into();
    b_active.groups = Set(vec![editors_group.id]);
    let b = b_active.update(&app.db).await.unwrap();

    assert_eq!(
        access::object_role_for(&app.db, &b, "query", q.id)
            .await
            .unwrap(),
        None,
        "B must have no role on a restricted query before any grant"
    );

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        None,
        Some(editors_group.id),
        "editor",
    )
    .await
    .expect("A, the owner, must be able to grant Editor to the group");

    assert_eq!(
        access::object_role_for(&app.db, &b, "query", q.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Editor),
        "B, a member of the granted group, must have Editor role after the group grant"
    );
}

/// Re-granting a different role to the same grantee updates `access_type` in place.
#[tokio::test]
async fn regrant_updates_existing_role_for_user_and_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "regrant").await;

    let mut qa = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    qa.api_key = Set("sharing-test-query-regrant-user".to_string());
    qa.visibility = Set("restricted".to_string());
    let q = qa.insert(&app.db).await.unwrap();
    let q = query_service::publish(&app.db, &fx.a, q, 0).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(fx.b.id),
        None,
        "viewer",
    )
    .await
    .expect("initial viewer grant to B must succeed");
    assert_eq!(
        access::object_role_for(&app.db, &fx.b, "query", q.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Viewer),
        "B must be Viewer after the initial grant"
    );

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(fx.b.id),
        None,
        "editor",
    )
    .await
    .expect("re-granting editor to the same user must succeed");
    assert_eq!(
        access::object_role_for(&app.db, &fx.b, "query", q.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Editor),
        "re-granting must update B's role to Editor, not leave the stale Viewer grant"
    );

    let rows = permission_service::list(&app.db, fx.org_id, "query", q.id)
        .await
        .unwrap();
    let user_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.grantee_id == Some(fx.b.id))
        .collect();
    assert_eq!(
        user_rows.len(),
        1,
        "re-granting must update the existing row, not insert a second one"
    );
    assert_eq!(user_rows[0].access_type, "editor");

    let mut qb = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    qb.api_key = Set("sharing-test-query-regrant-group".to_string());
    qb.visibility = Set("restricted".to_string());
    let q2 = qb.insert(&app.db).await.unwrap();
    let q2 = query_service::publish(&app.db, &fx.a, q2, 0).await.unwrap();

    let group = new_group(fx.org_id, "regrant-group", vec![])
        .insert(&app.db)
        .await
        .unwrap();
    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-regrant@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key("sharing-test-c-regrant"));
    c.groups = Set(vec![group.id]);
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q2.id,
        None,
        Some(group.id),
        "viewer",
    )
    .await
    .expect("initial viewer grant to the group must succeed");
    assert_eq!(
        access::object_role_for(&app.db, &c, "query", q2.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Viewer),
        "group member C must be Viewer after the initial group grant"
    );

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q2.id,
        None,
        Some(group.id),
        "editor",
    )
    .await
    .expect("re-granting editor to the same group must succeed");
    assert_eq!(
        access::object_role_for(&app.db, &c, "query", q2.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Editor),
        "re-granting must update the group's role to Editor"
    );

    let rows2 = permission_service::list(&app.db, fx.org_id, "query", q2.id)
        .await
        .unwrap();
    let group_rows: Vec<_> = rows2
        .iter()
        .filter(|r| r.grantee_group_id == Some(group.id))
        .collect();
    assert_eq!(
        group_rows.len(),
        1,
        "re-granting to a group must update the existing row, not insert a second one"
    );
    assert_eq!(group_rows[0].access_type, "editor");
}

/// Granting to a group in a different org is rejected.
#[tokio::test]
async fn grant_to_cross_org_group_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "cross-org").await;
    let q = insert_published_query(&app.db, &fx, "cross-org").await;

    let mut other_org = new_organization();
    other_org.slug = Set(format!(
        "sharing-test-cross-org-{}",
        uuid::Uuid::new_v4().simple()
    ));
    other_org.name = Set("Sharing Cross-Org Test".to_string());
    let other_org = other_org.insert(&app.db).await.unwrap();
    let other_org_group = new_group(other_org.id, "other-org-group", vec![])
        .insert(&app.db)
        .await
        .unwrap();

    let err = permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        None,
        Some(other_org_group.id),
        "editor",
    )
    .await
    .expect_err("granting to a cross-org group must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
}

/// `set_visibility`: the owner may restrict; an unrelated member gets 403.
#[tokio::test]
async fn set_visibility_owner_ok_non_owner_forbidden() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "set-visibility").await;

    let owned_by_a = insert_published_dashboard(&app.db, &fx, "set-visibility-a").await;
    permission_service::set_visibility(
        &app.db,
        fx.org_id,
        &fx.a,
        "dashboard",
        owned_by_a.id,
        "restricted",
    )
    .await
    .expect("the owner must be able to set visibility");

    let reloaded = dashboard::Entity::find_by_id(owned_by_a.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.visibility, "restricted");

    let owned_by_a_2 = insert_published_dashboard(&app.db, &fx, "set-visibility-a2").await;
    let err = permission_service::set_visibility(
        &app.db,
        fx.org_id,
        &fx.b,
        "dashboard",
        owned_by_a_2.id,
        "restricted",
    )
    .await
    .expect_err("a non-owner, non-admin member must be refused");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

/// `?filter=shared-with-me` returns exactly the queries B has an explicit grant on and doesn't own.
#[tokio::test]
async fn list_scope_shared_with_me_returns_granted_not_owned() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "scope-filter").await;

    let owned_by_a = insert_published_query(&app.db, &fx, "scope-filter-owned").await;

    let mut qa = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    qa.api_key = Set("sharing-test-query-scope-filter-shared".to_string());
    qa.visibility = Set("restricted".to_string());
    let shared_with_b = qa.insert(&app.db).await.unwrap();
    let shared_with_b = query_service::publish(&app.db, &fx.a, shared_with_b, 0)
        .await
        .unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        shared_with_b.id,
        Some(fx.b.id),
        None,
        "viewer",
    )
    .await
    .expect("A must be able to grant B viewer");

    let all = query_service::list_for_user(&app.db, fx.org_id, &fx.b, false, None, None, None)
        .await
        .unwrap();
    assert!(all.iter().any(|q| q.id == owned_by_a.id));
    assert!(all.iter().any(|q| q.id == shared_with_b.id));

    let scoped = query_service::list_for_user(
        &app.db,
        fx.org_id,
        &fx.b,
        false,
        None,
        None,
        Some("shared-with-me"),
    )
    .await
    .unwrap();
    let scoped_ids: Vec<i64> = scoped.iter().map(|q| q.id).collect();
    assert_eq!(
        scoped_ids,
        vec![shared_with_b.id],
        "shared-with-me must return exactly the granted-not-owned query"
    );
}

/// A published `restricted` query, so only an explicit grant unlocks access.
async fn insert_restricted_query(
    db: &sea_orm::DatabaseConnection,
    fx: &Fixture,
    tag: &str,
) -> query::Model {
    let mut q = new_query(fx.org_id, fx.data_source_id, fx.a.id);
    q.api_key = Set(format!("sharing-test-query-{tag}"));
    q.visibility = Set("restricted".to_string());
    let q = q.insert(db).await.unwrap();
    query_service::publish(db, &fx.a, q, 0).await.unwrap()
}

/// An owner-tier ACL grant is enough to share further: B, granted `owner`, can grant C `viewer`.
#[tokio::test]
async fn acl_owner_grant_can_grant_to_others() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "acl-owner-grant").await;
    let q = insert_restricted_query(&app.db, &fx, "acl-owner-grant").await;

    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-acl-owner-grant@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-c-acl-owner-grant",
    ));
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(fx.b.id),
        None,
        "owner",
    )
    .await
    .expect("A, the creator, must be able to grant B owner");

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.b,
        "query",
        q.id,
        Some(c.id),
        None,
        "viewer",
    )
    .await
    .expect("B, an ACL-owner grantee (not the creator), must be able to grant C viewer");

    assert_eq!(
        access::object_role_for(&app.db, &c, "query", q.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Viewer),
        "C must be able to read the query after B's grant"
    );
}

/// An ACL-owner grantee can revoke a grant they didn't create.
#[tokio::test]
async fn acl_owner_grant_can_revoke() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "acl-owner-revoke").await;
    let q = insert_restricted_query(&app.db, &fx, "acl-owner-revoke").await;

    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-acl-owner-revoke@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-c-acl-owner-revoke",
    ));
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(fx.b.id),
        None,
        "owner",
    )
    .await
    .expect("A must be able to grant B owner");

    // A, not B, granted C, so revoking as B works only via the owner-tier check.
    let c_grant = permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(c.id),
        None,
        "viewer",
    )
    .await
    .expect("A must be able to grant C viewer");

    permission_service::revoke(&app.db, fx.org_id, &fx.b, "query", q.id, c_grant.id)
        .await
        .expect("B, an ACL-owner grantee, must be able to revoke a grant A made");

    assert_eq!(
        access::object_role_for(&app.db, &c, "query", q.id)
            .await
            .unwrap(),
        None,
        "C must lose access once the grant is revoked"
    );
}

/// An editor-tier grant can't share; only owner-tier can.
#[tokio::test]
async fn acl_editor_grant_cannot_grant() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "acl-editor-cannot-grant").await;
    let q = insert_published_query(&app.db, &fx, "acl-editor-cannot-grant").await;

    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-acl-editor-cannot-grant@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-c-acl-editor-cannot-grant",
    ));
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(fx.b.id),
        None,
        "editor",
    )
    .await
    .expect("A must be able to grant B editor");

    let err = permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.b,
        "query",
        q.id,
        Some(c.id),
        None,
        "viewer",
    )
    .await
    .expect_err("B, an editor-tier grantee, must not be able to grant C");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

/// The same owner-tier rule applies to monitors.
#[tokio::test]
async fn acl_owner_grant_can_grant_to_others_on_monitor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "acl-owner-grant-monitor").await;
    let q = insert_published_query(&app.db, &fx, "acl-owner-grant-monitor").await;
    let m = monitor::ActiveModel {
        org_id: Set(fx.org_id),
        name: Set("ACL Test Monitor".to_string()),
        query_id: Set(q.id),
        user_id: Set(fx.a.id),
        options: Set(serde_json::json!({ "op": ">", "value": 1 })),
        state: Set("unknown".to_string()),
        visibility: Set("restricted".to_string()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-acl-owner-grant-monitor@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-c-acl-owner-grant-monitor",
    ));
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "monitor",
        m.id,
        Some(fx.b.id),
        None,
        "owner",
    )
    .await
    .expect("A, the creator, must be able to grant B owner on the monitor");

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.b,
        "monitor",
        m.id,
        Some(c.id),
        None,
        "viewer",
    )
    .await
    .expect("B, an ACL-owner grantee, must be able to grant C viewer on the monitor");

    assert_eq!(
        access::object_role_for(&app.db, &c, "monitor", m.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Viewer),
        "C must be able to read the monitor after B's grant"
    );
}

/// The owner-tier rule extends through a group grant.
#[tokio::test]
async fn acl_group_owner_grant_can_grant_to_others() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "acl-group-owner-grant").await;
    let q = insert_restricted_query(&app.db, &fx, "acl-group-owner-grant").await;

    let owners_group = new_group(fx.org_id, "owners-group-acl-grant", vec![])
        .insert(&app.db)
        .await
        .unwrap();
    // Keep B's fixture group membership (carries edit_query) alongside the new owner-tier group.
    let mut groups = fx.b.groups.clone();
    groups.push(owners_group.id);
    let mut b_active: user::ActiveModel = fx.b.clone().into();
    b_active.groups = Set(groups);
    let b = b_active.update(&app.db).await.unwrap();

    let mut c = new_user(fx.org_id);
    c.email = Set("member-c-acl-group-owner-grant@test.local".to_string());
    c.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-c-acl-group-owner-grant",
    ));
    let c = c.insert(&app.db).await.unwrap();

    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        None,
        Some(owners_group.id),
        "owner",
    )
    .await
    .expect("A must be able to grant the group owner");

    permission_service::grant(
        &app.db,
        fx.org_id,
        &b,
        "query",
        q.id,
        Some(c.id),
        None,
        "viewer",
    )
    .await
    .expect("B, a member of the owner-granted group, must be able to grant C viewer");

    assert_eq!(
        access::object_role_for(&app.db, &c, "query", q.id)
            .await
            .unwrap(),
        Some(access::ObjectRole::Viewer),
        "C must be able to read the query after B's grant"
    );
}

async fn is_owner(db: &sea_orm::DatabaseConnection, user: &user::Model, query_id: i64) -> bool {
    access::object_role_for(db, user, "query", query_id)
        .await
        .unwrap()
        == Some(access::ObjectRole::Owner)
}

/// Owner reach across every tier: creator, admin, `owner` grant, `editor`/`viewer` grant (false),
/// unrelated user, nonexistent id.
#[tokio::test]
async fn owner_reach_matrix() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let fx = build_fixture(&app.db, "is-object-owner").await;
    let q = insert_restricted_query(&app.db, &fx, "is-object-owner").await;

    assert!(
        is_owner(&app.db, &fx.a, q.id).await,
        "the creator must be an owner"
    );

    // `is_admin` derives from role-linked permissions, so use the builtin per-org "admin" group.
    let admin_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(fx.org_id))
        .filter(group::Column::Name.eq("admin"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin admin group must exist");
    let mut admin = new_user(fx.org_id);
    admin.email = Set("admin-is-object-owner@test.local".to_string());
    admin.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-admin-is-object-owner",
    ));
    admin.groups = Set(vec![admin_group.id]);
    let admin = admin.insert(&app.db).await.unwrap();
    assert!(
        is_owner(&app.db, &admin, q.id).await,
        "an org admin must be an owner regardless of grants"
    );

    let mut owner_grantee = new_user(fx.org_id);
    owner_grantee.email = Set("owner-grantee-is-object-owner@test.local".to_string());
    owner_grantee.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-owner-grantee-is-object-owner",
    ));
    let owner_grantee = owner_grantee.insert(&app.db).await.unwrap();
    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(owner_grantee.id),
        None,
        "owner",
    )
    .await
    .unwrap();
    assert!(
        is_owner(&app.db, &owner_grantee, q.id).await,
        "a user with an owner ACL grant must be an owner"
    );

    let mut editor_grantee = new_user(fx.org_id);
    editor_grantee.email = Set("editor-grantee-is-object-owner@test.local".to_string());
    editor_grantee.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-editor-grantee-is-object-owner",
    ));
    let editor_grantee = editor_grantee.insert(&app.db).await.unwrap();
    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(editor_grantee.id),
        None,
        "editor",
    )
    .await
    .unwrap();
    assert!(
        !is_owner(&app.db, &editor_grantee, q.id).await,
        "a user with only an editor ACL grant must not be an owner"
    );

    let mut viewer_grantee = new_user(fx.org_id);
    viewer_grantee.email = Set("viewer-grantee-is-object-owner@test.local".to_string());
    viewer_grantee.api_key_hash = Set(arcodash_core::hash::hash_api_key(
        "sharing-test-viewer-grantee-is-object-owner",
    ));
    let viewer_grantee = viewer_grantee.insert(&app.db).await.unwrap();
    permission_service::grant(
        &app.db,
        fx.org_id,
        &fx.a,
        "query",
        q.id,
        Some(viewer_grantee.id),
        None,
        "viewer",
    )
    .await
    .unwrap();
    assert!(
        !is_owner(&app.db, &viewer_grantee, q.id).await,
        "a user with only a viewer ACL grant must not be an owner"
    );

    assert!(
        !is_owner(&app.db, &fx.b, q.id).await,
        "a user with no relation to the object must not be an owner"
    );

    assert!(
        !is_owner(&app.db, &fx.a, q.id + 999_000).await,
        "a nonexistent object id must not be an owner (fail-closed, not an error)"
    );
}

/// End to end: an unrelated `options` update keeps `secret_token`; revoking revokes the live
/// `embeds` row; re-sharing mints a fresh link.
#[tokio::test]
async fn dashboard_share_survives_unrelated_updates_and_revoke_actually_revokes() {
    let app = skip_if_none!(spawn_app().await);
    // Must be the seeded "default" org: OSS `auth::resolve_org` resolves every request to it, so
    // the public-URL checks need it.
    let org_id = {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        arcodash_core::entities::organization::Entity::find()
            .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
            .one(&app.db)
            .await
            .unwrap()
            .expect("default org must exist")
            .id
    };
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Share Desync".to_string())
        .await
        .expect("create failed");
    let slug = d.slug.clone();
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("widget create failed");
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    // An unrelated options update must not drop the token.
    let token1 = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .expect("share failed");
    dashboard_service::update(
        &app.db,
        org_id,
        &owner,
        &slug,
        None,
        None,
        Some(serde_json::json!({ "theme": "dark" })),
        None,
        None,
        None,
    )
    .await
    .expect("unrelated options update failed");
    let reloaded = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    assert_eq!(
        reloaded
            .options
            .get("secret_token")
            .and_then(|v| v.as_str()),
        Some(token1.as_str()),
        "secret_token must survive an unrelated options update"
    );
    assert_eq!(
        reloaded.options.get("theme").and_then(|v| v.as_str()),
        Some("dark"),
        "the unrelated key must still be written"
    );

    let public_url = |token: &str| format!("{}/api/dashboards/public/{token}", app.address);
    let anon = reqwest::Client::builder().build().unwrap();
    assert_eq!(
        anon.get(public_url(&token1)).send().await.unwrap().status(),
        200,
        "token1 must serve before revoke"
    );

    // Revoking via the legacy endpoint must revoke the embeds row; the public URL must 404.
    dashboard_service::revoke_share(&app.db, org_id, &owner, &slug)
        .await
        .expect("revoke failed");
    let revoked_row = embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::Token.eq(token1.clone()))
        .one(&app.db)
        .await
        .unwrap()
        .expect("embed row must still exist (revoked, not deleted)");
    assert!(
        revoked_row.revoked_at.is_some(),
        "the embeds row itself must be revoked"
    );
    assert_eq!(
        anon.get(public_url(&token1)).send().await.unwrap().status(),
        404,
        "token1 must 404 after revoke"
    );

    let token2 = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .expect("re-share failed");
    assert_ne!(
        token1, token2,
        "re-share after revoke must not return the dead token"
    );
    assert_eq!(
        anon.get(public_url(&token2)).send().await.unwrap().status(),
        200,
        "the fresh token must serve"
    );

    // Same guarantee via `DELETE /api/embeds/{id}`: share() must notice the dead token and mint a
    // fresh one.
    let groups = arcodash_backend::authz::permissions::load_effective_groups(&app.db, &owner)
        .await
        .unwrap();
    let embed_row2 = embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::Token.eq(token2.clone()))
        .one(&app.db)
        .await
        .unwrap()
        .expect("embed row for token2 must exist");
    embed_service::revoke_embed(&app.db, org_id, &owner, &groups, embed_row2.id)
        .await
        .expect("revoke via embeds route failed");
    assert_eq!(
        anon.get(public_url(&token2)).send().await.unwrap().status(),
        404,
        "token2 must 404 after being revoked via DELETE /api/embeds/{{id}}"
    );

    let token3 = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .expect("re-share after embeds-route revoke failed");
    assert_ne!(
        token2, token3,
        "share() must notice the embeds-route revoke and mint a fresh token"
    );
    assert_eq!(
        anon.get(public_url(&token3)).send().await.unwrap().status(),
        200,
        "the newest token must serve"
    );
}

/// The legacy `DELETE .../share` revokes only the embed backing `options.secret_token`; an
/// independently minted embed survives.
#[tokio::test]
async fn revoke_share_only_revokes_the_legacy_token_not_other_live_embeds() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        arcodash_core::entities::organization::Entity::find()
            .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
            .one(&app.db)
            .await
            .unwrap()
            .expect("default org must exist")
            .id
    };
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Two Live Embeds".to_string())
        .await
        .expect("create failed");
    let slug = d.slug.clone();
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("widget create failed");
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    let legacy_token = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .expect("share failed");

    // A second, independently minted embed on the same dashboard, inserted directly.
    let now = chrono::Utc::now();
    let other_token = uuid::Uuid::new_v4().to_string();
    embed::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        object_type: Set("dashboard".to_string()),
        object_id: Set(d.id),
        mode: Set("public".to_string()),
        token: Set(Some(other_token.clone())),
        locked_params: Set(serde_json::json!({})),
        frame_ancestors: Set(None),
        expires_at: Set(None),
        created_by_id: Set(Some(owner.id)),
        anon_authorized: Set(true),
        revoked_at: Set(None),
        created_at: Set(now.into()),
    }
    .insert(&app.db)
    .await
    .expect("second embed insert failed");

    dashboard_service::revoke_share(&app.db, org_id, &owner, &slug)
        .await
        .expect("revoke_share failed");

    let public_url = |token: &str| format!("{}/api/dashboards/public/{token}", app.address);
    let anon = reqwest::Client::builder().build().unwrap();
    assert_eq!(
        anon.get(public_url(&legacy_token))
            .send()
            .await
            .unwrap()
            .status(),
        404,
        "the legacy token must 404 after revoke_share"
    );
    assert_eq!(
        anon.get(public_url(&other_token))
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "the unrelated embed minted via POST /api/embeds must still serve"
    );

    let groups = arcodash_backend::authz::permissions::load_effective_groups(&app.db, &owner)
        .await
        .unwrap();
    let live = embed_service::list_embeds_for_object(
        &app.db,
        org_id,
        &owner,
        &groups,
        embed_service::EmbedObjectType::Dashboard,
        d.id,
    )
    .await
    .expect("list_embeds_for_object failed")
    .into_iter()
    .filter(|e| e.revoked_at.is_none())
    .collect::<Vec<_>>();
    assert_eq!(
        live.len(),
        1,
        "exactly one live embed must remain (the survivor), got {live:?}"
    );
    assert_eq!(live[0].token.as_deref(), Some(other_token.as_str()));
}

/// `revoke_share` must scope its lookup by `object_type` + `object_id`, not just `token`, or a
/// colliding token could revoke another dashboard's embed and bypass the creator-or-admin gate. The
/// collision is planted by direct row write.
#[tokio::test]
async fn revoke_share_cannot_revoke_another_dashboards_embed_via_a_planted_secret_token() {
    let app = skip_if_none!(spawn_app().await);
    // Must be the seeded "default" org (see the first share test).
    let org_id = {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        arcodash_core::entities::organization::Entity::find()
            .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
            .one(&app.db)
            .await
            .unwrap()
            .expect("default org must exist")
            .id
    };
    let perms = &[
        "create_dashboard",
        "edit_dashboard",
        "view_dashboard",
        "create_embed",
        "share_public_anonymous",
    ];
    let (victim, _) = user_with_role(&app.db, org_id, perms).await;
    let (attacker, _) = user_with_role(&app.db, org_id, perms).await;

    let victim_d = dashboard_service::create(&app.db, org_id, &victim, "Victim".to_string())
        .await
        .expect("create failed");
    widget_service::create(
        &app.db,
        org_id,
        &victim,
        victim_d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("widget create failed");
    let victim_d = dashboard_service::get_by_slug(&app.db, org_id, &victim, &victim_d.slug)
        .await
        .unwrap();
    let victim_d = dashboard_service::publish(&app.db, &victim, victim_d)
        .await
        .expect("publish failed");
    let victim_token =
        dashboard_service::share(&app.db, &app.state, org_id, &victim, &victim_d.slug)
            .await
            .expect("share failed");

    // The attacker's dashboard: published, never shared, with a planted `secret_token` matching the
    // victim's.
    let attacker_d = dashboard_service::create(&app.db, org_id, &attacker, "Attacker".to_string())
        .await
        .expect("create failed");
    widget_service::create(
        &app.db,
        org_id,
        &attacker,
        attacker_d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("widget create failed");
    let attacker_d = dashboard_service::get_by_slug(&app.db, org_id, &attacker, &attacker_d.slug)
        .await
        .unwrap();
    let attacker_d = dashboard_service::publish(&app.db, &attacker, attacker_d)
        .await
        .expect("publish failed");
    // Direct row write, bypassing `dashboard_service::update`.
    {
        let mut active: dashboard::ActiveModel = attacker_d.clone().into();
        active.options = Set(serde_json::json!({ "secret_token": victim_token }));
        active.update(&app.db).await.expect("plant failed");
    }

    // Precondition: the planted token landed, so the no-op assertion proves the revoke scoping.
    let attacker_d_after_plant =
        dashboard_service::get_by_slug(&app.db, org_id, &attacker, &attacker_d.slug)
            .await
            .unwrap();
    assert_eq!(
        attacker_d_after_plant.options["secret_token"],
        serde_json::Value::String(victim_token.clone()),
        "the planted secret_token must actually be persisted on the attacker's dashboard"
    );

    dashboard_service::revoke_share(&app.db, org_id, &attacker, &attacker_d.slug)
        .await
        .expect(
            "revoke_share must remain a no-op when the planted token belongs to another dashboard",
        );

    let public_url = format!("{}/api/dashboards/public/{victim_token}", app.address);
    let anon = reqwest::Client::builder().build().unwrap();
    assert_eq!(
        anon.get(public_url).send().await.unwrap().status(),
        200,
        "the victim's embed must survive an attacker planting its token into an unrelated dashboard's options"
    );
}

/// `DELETE .../share` must require creator-or-admin like `DELETE /api/embeds/{id}`, not merely
/// Editor reach.
#[tokio::test]
async fn revoke_share_requires_creator_or_admin_like_the_embeds_route() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;
    let (editor_grantee, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Revoke Policy".to_string())
        .await
        .unwrap();
    let slug = d.slug.clone();
    let dashboard_id = d.id;
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    dashboard_service::publish(&app.db, &owner, d)
        .await
        .unwrap();

    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        dashboard_id,
        Some(editor_grantee.id),
        None,
        "editor",
    )
    .await
    .unwrap();

    dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .unwrap();

    // An Editor grantee passes an edit-only check but is neither creator nor admin.
    let denied = dashboard_service::revoke_share(&app.db, org_id, &editor_grantee, &slug).await;
    assert!(
        denied.is_err(),
        "a non-creator Editor grantee must be denied revoke"
    );

    dashboard_service::revoke_share(&app.db, org_id, &owner, &slug)
        .await
        .expect("the creator must be allowed to revoke");

    dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .unwrap();
    dashboard_service::revoke_share(&app.db, org_id, &admin, &slug)
        .await
        .expect("an admin must be allowed to revoke");

    // Parity: the same grantee is denied by `DELETE /api/embeds/{id}`.
    let token = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .unwrap();
    let embed_row = embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::Token.eq(token))
        .one(&app.db)
        .await
        .unwrap()
        .expect("embed row must exist");
    let groups =
        arcodash_backend::authz::permissions::load_effective_groups(&app.db, &editor_grantee)
            .await
            .unwrap();
    let denied2 =
        embed_service::revoke_embed(&app.db, org_id, &editor_grantee, &groups, embed_row.id).await;
    assert!(
        denied2.is_err(),
        "DELETE /api/embeds/{{id}} must agree: the same non-creator Editor grantee is denied"
    );
}

/// `dashboard_service::share` applies the org's `embed_default_expiry`, like
/// `embed_service::create_embed`.
#[tokio::test]
async fn share_applies_the_org_embed_default_expiry() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Default Expiry".to_string())
        .await
        .unwrap();
    let slug = d.slug.clone();
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    dashboard_service::publish(&app.db, &owner, d)
        .await
        .unwrap();

    let mut state_with_default = app.state.clone();
    state_with_default.embed_default_expiry = Some(std::time::Duration::from_secs(3600));

    let before = chrono::Utc::now();
    let token = dashboard_service::share(&app.db, &state_with_default, org_id, &owner, &slug)
        .await
        .unwrap();

    let embed_row = embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::Token.eq(token))
        .one(&app.db)
        .await
        .unwrap()
        .expect("embed row must exist");
    let expires_at = embed_row
        .expires_at
        .expect("share must set expires_at from the org default")
        .with_timezone(&chrono::Utc);
    let expected = before + chrono::Duration::seconds(3600);
    let drift = (expires_at - expected).num_seconds().abs();
    assert!(
        drift <= 5,
        "expires_at ({expires_at}) must land ~1h out (expected {expected}), drift was {drift}s"
    );
}

/// Listing and revoking embeds doesn't require the dashboard to be published; an owner who
/// unpublishes can still revoke.
#[tokio::test]
async fn list_and_revoke_embeds_do_not_require_the_object_to_be_published() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Unpublish Then Manage".to_string())
        .await
        .unwrap();
    let slug = d.slug.clone();
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    let dashboard_id = d.id;
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .unwrap();

    let groups = arcodash_backend::authz::permissions::load_effective_groups(&app.db, &owner)
        .await
        .unwrap();
    let created = embed_service::create_embed(
        &app.db,
        &app.state,
        org_id,
        &owner,
        &groups,
        embed_service::CreateEmbedInput {
            object_type: embed_service::EmbedObjectType::Dashboard,
            object_id: dashboard_id,
            anonymous: true,
            locked_params: serde_json::json!({}),
            expires_at: None,
            frame_ancestors: None,
        },
    )
    .await
    .unwrap();

    dashboard_service::unpublish(&app.db, &owner, d)
        .await
        .unwrap();

    let listed = embed_service::list_embeds_for_object(
        &app.db,
        org_id,
        &owner,
        &groups,
        embed_service::EmbedObjectType::Dashboard,
        dashboard_id,
    )
    .await
    .expect("listing embeds must not require the dashboard to be published");
    assert!(listed.iter().any(|e| e.id == created.id));

    embed_service::revoke_embed(&app.db, org_id, &owner, &groups, created.id)
        .await
        .expect("revoking an embed must not require the dashboard to be published");
}

/// `dashboard_service::update` strips `secret_token` from a caller `options` patch, so a stale blob
/// can't overwrite it.
#[tokio::test]
async fn update_strips_secret_token_out_of_a_client_supplied_options_patch() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
            "share_public_anonymous",
        ],
    )
    .await;

    let d = dashboard_service::create(&app.db, org_id, &owner, "Options Injection".to_string())
        .await
        .unwrap();
    let slug = d.slug.clone();
    widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("tile".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let d = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    dashboard_service::publish(&app.db, &owner, d)
        .await
        .unwrap();

    let real_token = dashboard_service::share(&app.db, &app.state, org_id, &owner, &slug)
        .await
        .unwrap();

    dashboard_service::update(
        &app.db,
        org_id,
        &owner,
        &slug,
        None,
        None,
        Some(serde_json::json!({ "secret_token": "attacker-supplied", "theme": "dark" })),
        None,
        None,
        None,
    )
    .await
    .expect("update must succeed, just without the system-owned key");

    let reloaded = dashboard_service::get_by_slug(&app.db, org_id, &owner, &slug)
        .await
        .unwrap();
    assert_eq!(
        reloaded
            .options
            .get("secret_token")
            .and_then(|v| v.as_str()),
        Some(real_token.as_str()),
        "a client-supplied options patch must not be able to overwrite secret_token"
    );
    assert_eq!(
        reloaded.options.get("theme").and_then(|v| v.as_str()),
        Some("dark"),
        "every other key in the same patch must still be applied"
    );
}
