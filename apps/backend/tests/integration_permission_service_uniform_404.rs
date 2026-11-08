//! Integration tests: `permission_service::grant`/`set_visibility` uniform-404 discipline.
//!
//! A caller with zero view reach to a real, same-org object must get the same 404 every other
//! mutation path gives — never a 403, which would confirm the object exists. A caller who CAN
//! view the object but isn't Owner-tier still gets 403. Cross-org and genuinely-nonexistent ids
//! stay 404 either way.
//!
//! `integration_permissions_acl.rs` covers the dashboard case end-to-end over HTTP; this file
//! exercises `permission_service` directly for the query and monitor object families plus the
//! cross-org/nonexistent-id controls, without the HTTP/session overhead.

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::services::{monitor_service, permission_service, query_service};
use arcodash_core::entities::{data_source, monitor as monitor_entity, query, user};
use arcodash_core::test_support::{new_data_source, new_query};
use axum::http::StatusCode;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

/// Insert an unpublished (draft) query owned by `owner` — invisible to every other org member
/// per draft privacy, same state a fresh `POST /api/queries` leaves it in.
async fn draft_query(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
) -> query::Model {
    let ds_id = data_source::Entity::insert(new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id;
    let mut q = new_query(org_id, ds_id, owner.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    q.insert(db).await.expect("query insert failed")
}

/// Insert a published, then owner-only-visible monitor: monitors have no draft state (`is_draft`
/// is always `false` for them in `object_role_for`), so the default `workspace` visibility would
/// otherwise hand every org member Viewer reach — force `restricted` to actually hide it.
async fn hidden_monitor(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
) -> monitor_entity::Model {
    let ds_id = data_source::Entity::insert(new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id;
    let mut q = new_query(org_id, ds_id, owner.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q.insert(db).await.expect("query insert failed");
    let published = query_service::publish(db, owner, q, 0)
        .await
        .expect("publish failed");

    let m = monitor_service::create(
        db,
        org_id,
        owner,
        published.id,
        "Hidden Monitor".to_string(),
        serde_json::json!({ "op": ">", "value": 1 }),
        None,
    )
    .await
    .expect("monitor create failed");

    let mut active: monitor_entity::ActiveModel = m.into();
    active.visibility = Set("restricted".to_string());
    active.update(db).await.expect("visibility update failed")
}

/// Full capability set the fixture owner needs across every helper in this file.
const OWNER_PERMS: &[&str] = &[
    "create_query",
    "edit_query",
    "view_query",
    "create_monitor",
    "view_monitor",
    "edit_monitor",
];

// ── no view reach -> 404 ────────────────────────────────

#[tokio::test]
async fn grant_returns_404_for_query_the_caller_cannot_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;

    let q = draft_query(&app.db, org_id, &owner).await;

    let err = permission_service::grant(
        &app.db,
        org_id,
        &stranger,
        "query",
        q.id,
        Some(grantee.id),
        None,
        "viewer",
    )
    .await
    .expect_err("caller with no view reach must not be able to grant");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

#[tokio::test]
async fn set_visibility_returns_404_for_query_the_caller_cannot_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;

    let q = draft_query(&app.db, org_id, &owner).await;

    let err =
        permission_service::set_visibility(&app.db, org_id, &stranger, "query", q.id, "public")
            .await
            .expect_err("caller with no view reach must not be able to change visibility");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

#[tokio::test]
async fn grant_returns_404_for_monitor_the_caller_cannot_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;

    let m = hidden_monitor(&app.db, org_id, &owner).await;

    let err = permission_service::grant(
        &app.db,
        org_id,
        &stranger,
        "monitor",
        m.id,
        Some(grantee.id),
        None,
        "viewer",
    )
    .await
    .expect_err("caller with no view reach must not be able to grant");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

#[tokio::test]
async fn set_visibility_returns_404_for_monitor_the_caller_cannot_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;

    let m = hidden_monitor(&app.db, org_id, &owner).await;

    let err = permission_service::set_visibility(
        &app.db,
        org_id,
        &stranger,
        "monitor",
        m.id,
        "workspace",
    )
    .await
    .expect_err("caller with no view reach must not be able to change visibility");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

// ── caller CAN view but isn't owner -> still 403 ────────────

#[tokio::test]
async fn grant_returns_403_for_query_the_caller_can_view_but_does_not_own() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (viewer, _) = user_with_role(&app.db, org_id, &[]).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;

    let ds_id = data_source::Entity::insert(new_data_source(org_id))
        .exec(&app.db)
        .await
        .unwrap()
        .last_insert_id;
    let mut q = new_query(org_id, ds_id, owner.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q.insert(&app.db).await.unwrap();
    // Published + default `workspace` visibility -> any org member gets Viewer reach via the
    // org-open default (object_role_for has no data-source gate of its own).
    let published = query_service::publish(&app.db, &owner, q, 0).await.unwrap();

    let err = permission_service::grant(
        &app.db,
        org_id,
        &viewer,
        "query",
        published.id,
        Some(grantee.id),
        None,
        "viewer",
    )
    .await
    .expect_err("a viewer who isn't Owner-tier must not be able to share");
    assert_eq!(err.status, StatusCode::FORBIDDEN, "{}", err.message);
}

#[tokio::test]
async fn set_visibility_returns_403_for_monitor_the_caller_can_view_but_does_not_own() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (viewer, _) = user_with_role(&app.db, org_id, &[]).await;

    let ds_id = data_source::Entity::insert(new_data_source(org_id))
        .exec(&app.db)
        .await
        .unwrap()
        .last_insert_id;
    let mut q = new_query(org_id, ds_id, owner.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q.insert(&app.db).await.unwrap();
    let published = query_service::publish(&app.db, &owner, q, 0).await.unwrap();
    // Default `workspace` visibility, no draft concept for monitors -> immediately viewable.
    let m = monitor_service::create(
        &app.db,
        org_id,
        &owner,
        published.id,
        "Visible Monitor".to_string(),
        serde_json::json!({ "op": ">", "value": 1 }),
        None,
    )
    .await
    .unwrap();

    let err =
        permission_service::set_visibility(&app.db, org_id, &viewer, "monitor", m.id, "restricted")
            .await
            .expect_err("a viewer who isn't Owner-tier must not be able to change visibility");
    assert_eq!(err.status, StatusCode::FORBIDDEN, "{}", err.message);
}

// ── cross-org and genuinely nonexistent ids stay 404 ────────

#[tokio::test]
async fn grant_returns_404_for_cross_org_query_id() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_a = make_org(&app.db).await;
    let org_b = make_org(&app.db).await;
    let (owner_a, _) = user_with_role(&app.db, org_a, OWNER_PERMS).await;
    let (caller_b, _) = user_with_role(&app.db, org_b, &[]).await;
    let (grantee_b, _) = user_with_role(&app.db, org_b, &[]).await;

    // Published + workspace-visible within org_a -- must still be invisible to org_b.
    let ds_id = data_source::Entity::insert(new_data_source(org_a))
        .exec(&app.db)
        .await
        .unwrap()
        .last_insert_id;
    let mut q = new_query(org_a, ds_id, owner_a.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q.insert(&app.db).await.unwrap();
    let published = query_service::publish(&app.db, &owner_a, q, 0)
        .await
        .unwrap();

    let err = permission_service::grant(
        &app.db,
        org_b,
        &caller_b,
        "query",
        published.id,
        Some(grantee_b.id),
        None,
        "viewer",
    )
    .await
    .expect_err("a different org's real, published object must still 404");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

#[tokio::test]
async fn grant_returns_404_for_nonexistent_query_id() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;

    let err = permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "query",
        999_999_999,
        Some(grantee.id),
        None,
        "viewer",
    )
    .await
    .expect_err("a nonexistent id must 404");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}

#[tokio::test]
async fn set_visibility_returns_404_for_nonexistent_monitor_id() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;

    let err = permission_service::set_visibility(
        &app.db,
        org_id,
        &owner,
        "monitor",
        999_999_999,
        "workspace",
    )
    .await
    .expect_err("a nonexistent id must 404");
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);
}
