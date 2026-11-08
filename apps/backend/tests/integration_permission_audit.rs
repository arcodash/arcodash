//! Integration tests: `permission_service::{grant,revoke,set_visibility}` each leave exactly one
//! `events` row behind.

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::services::{permission_service, query_service};
use arcodash_core::entities::{data_source, event, query, user};
use arcodash_core::test_support::{new_data_source, new_query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
};

const OWNER_PERMS: &[&str] = &["create_query", "edit_query", "view_query"];

/// Insert an unpublished (draft) query owned by `owner` — Owner-tier reach by ownership alone,
/// same fixture `integration_permission_service_uniform_404.rs` uses.
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

/// The single most recent `events` row for `action`, or `None`.
async fn latest_event(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    action: &str,
) -> Option<event::Model> {
    event::Entity::find()
        .filter(event::Column::OrgId.eq(org_id))
        .filter(event::Column::Action.eq(action))
        .order_by_desc(event::Column::Id)
        .one(db)
        .await
        .expect("events query failed")
}

#[tokio::test]
async fn grant_records_permission_granted_event() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;
    let q = draft_query(&app.db, org_id, &owner).await;

    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "query",
        q.id,
        Some(grantee.id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");

    let e = latest_event(&app.db, org_id, "permission.granted")
        .await
        .expect("expected a permission.granted event");
    assert_eq!(e.user_id, Some(owner.id));
    assert_eq!(e.object_type.as_deref(), Some("query"));
    assert_eq!(e.object_id, Some(q.id));
    assert_eq!(
        e.additional_properties["grantee_user_id"].as_i64(),
        Some(grantee.id)
    );
    assert!(e.additional_properties["grantee_group_id"].is_null());
    assert_eq!(e.additional_properties["access_type"], "viewer");
}

#[tokio::test]
async fn revoke_records_permission_revoked_event() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;
    let q = draft_query(&app.db, org_id, &owner).await;

    let perm = permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "query",
        q.id,
        Some(grantee.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");

    permission_service::revoke(&app.db, org_id, &owner, "query", q.id, perm.id)
        .await
        .expect("revoke failed");

    let e = latest_event(&app.db, org_id, "permission.revoked")
        .await
        .expect("expected a permission.revoked event");
    assert_eq!(e.user_id, Some(owner.id));
    assert_eq!(e.object_type.as_deref(), Some("query"));
    assert_eq!(e.object_id, Some(q.id));
    assert_eq!(
        e.additional_properties["permission_id"].as_i64(),
        Some(perm.id)
    );
    assert_eq!(
        e.additional_properties["grantee_user_id"].as_i64(),
        Some(grantee.id)
    );
    assert_eq!(e.additional_properties["access_type"], "editor");
}

#[tokio::test]
async fn set_visibility_records_from_and_to() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(&app.db, org_id, OWNER_PERMS).await;

    let ds_id = data_source::Entity::insert(new_data_source(org_id))
        .exec(&app.db)
        .await
        .unwrap()
        .last_insert_id;
    let mut q = new_query(org_id, ds_id, owner.id);
    q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q.insert(&app.db).await.unwrap();
    let q = query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish failed");
    assert_eq!(q.visibility, "workspace", "fixture assumption drifted");

    permission_service::set_visibility(&app.db, org_id, &owner, "query", q.id, "restricted")
        .await
        .expect("set_visibility failed");

    let e = latest_event(&app.db, org_id, "object.visibility_changed")
        .await
        .expect("expected an object.visibility_changed event");
    assert_eq!(e.user_id, Some(owner.id));
    assert_eq!(e.object_type.as_deref(), Some("query"));
    assert_eq!(e.object_id, Some(q.id));
    assert_eq!(e.additional_properties["from"], "workspace");
    assert_eq!(e.additional_properties["to"], "restricted");
}
