//! Integration tests for the grant ceiling (`authz::permissions::require_can_grant` and its call
//! sites): a caller with a `manage_*` or `create_embed`+`share_public_anonymous` capability must
//! not hand out a permission, directly or via a role/group, that they don't hold.

mod common;
use common::{default_org_id, ok_json};
use common::{make_org, set_role_permissions, spawn_app, test_api_key, user_with_role, TestApp};

use arcodash_backend::authz::permissions::{
    load_effective_groups, load_effective_groups_by_ids, permission_keys_for_groups,
    require_can_grant,
};
use arcodash_backend::services::{permission_service, user_service};
use arcodash_core::entities::{
    access_permission, dashboard, data_source, data_source_group, embed, group, group_role, role,
    user,
};
use sea_orm::{ActiveModelTrait, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

async fn default_builtin_group_id(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    name: &str,
) -> i64 {
    group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.eq(name))
        .one(db)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("builtin group {name} must exist"))
        .id
}

/// A group whose vestigial `permissions` column is deliberately wrong (`bogus_column_perms`),
/// proving call sites read role-derived permissions.
async fn group_with_role(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    role_perms: &[&str],
    bogus_column_perms: &[&str],
) -> i64 {
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(role_perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("role insert failed")
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(bogus_column_perms.iter().map(|s| s.to_string()).collect()),
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

fn bare() -> reqwest::Client {
    reqwest::Client::new()
}

fn api_url(app: &TestApp, path: &str, u: &user::Model) -> String {
    format!("{}{path}?api_key={}", app.address, test_api_key(&u.email))
}

// ── permissions.rs helpers ──

#[tokio::test]
async fn require_can_grant_allows_empty_grant_set() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &[]).await;
    assert!(require_can_grant(&app.db, &caller, &[]).await.is_ok());
}

#[tokio::test]
async fn require_can_grant_allows_exact_key_the_caller_holds() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let result = require_can_grant(&app.db, &caller, &["view_query".to_string()]).await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn require_can_grant_allows_granular_key_implied_by_a_held_manage_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["manage_query"]).await;
    let result = require_can_grant(&app.db, &caller, &["execute_query".to_string()]).await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn require_can_grant_refuses_manage_key_when_caller_holds_only_the_granular() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["execute_query"]).await;
    let err = require_can_grant(&app.db, &caller, &["manage_query".to_string()])
        .await
        .expect_err("a granular key must not imply its own manage_* key");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn require_can_grant_admin_caller_passes_every_key_including_admin() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let result = require_can_grant(
        &app.db,
        &caller,
        &[
            "admin".to_string(),
            "manage_users".to_string(),
            "not-a-real-catalog-key".to_string(),
        ],
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn require_can_grant_refuses_an_unknown_key_for_a_non_admin() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let err = require_can_grant(&app.db, &caller, &["bogus_permission_key".to_string()])
        .await
        .expect_err("an unknown key can never be held, so it can never be granted");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn require_can_grant_error_is_403_and_names_every_missing_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &[]).await;
    let err = require_can_grant(
        &app.db,
        &caller,
        &["view_query".to_string(), "manage_users".to_string()],
    )
    .await
    .expect_err("caller holds neither key");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    assert!(
        err.message.contains("view_query") && err.message.contains("manage_users"),
        "error must name every missing key: {}",
        err.message
    );
}

#[tokio::test]
async fn permission_keys_for_groups_returns_the_role_derived_union_not_the_vestigial_column() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let group_id = group_with_role(
        &app.db,
        org_id,
        &["view_query", "create_query"],
        &["admin"], // vestigial column deliberately carries something the role does not
    )
    .await;

    let keys = permission_keys_for_groups(&app.db, org_id, &[group_id])
        .await
        .expect("permission_keys_for_groups failed");
    assert!(keys.contains(&"view_query".to_string()));
    assert!(keys.contains(&"create_query".to_string()));
    assert!(
        !keys.contains(&"admin".to_string()),
        "must not read the vestigial groups.permissions column: {keys:?}"
    );
}

#[tokio::test]
async fn permission_keys_for_groups_is_empty_for_no_group_ids() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let keys = permission_keys_for_groups(&app.db, org_id, &[])
        .await
        .expect("permission_keys_for_groups failed");
    assert!(keys.is_empty());
}

#[tokio::test]
async fn permission_keys_for_groups_ignores_group_ids_from_another_org() {
    let app = skip_if_none!(spawn_app().await);
    let org_a = make_org(&app.db).await;
    let org_b = make_org(&app.db).await;
    let group_in_b = group_with_role(&app.db, org_b, &["admin"], &[]).await;

    let keys = permission_keys_for_groups(&app.db, org_a, &[group_in_b])
        .await
        .expect("permission_keys_for_groups failed");
    assert!(
        keys.is_empty(),
        "a group id scoped to another org must never resolve: {keys:?}"
    );
}

#[tokio::test]
async fn load_effective_groups_by_ids_matches_load_effective_groups_for_a_users_own_groups() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (u, _) = user_with_role(&app.db, org_id, &["view_query", "create_query"]).await;

    let via_user = load_effective_groups(&app.db, &u)
        .await
        .expect("load_effective_groups failed");
    let via_ids = load_effective_groups_by_ids(&app.db, u.org_id, &u.groups)
        .await
        .expect("load_effective_groups_by_ids failed");

    let mut a: Vec<(i64, Vec<String>)> = via_user
        .into_iter()
        .map(|g| (g.id, g.permissions))
        .collect();
    let mut b: Vec<(i64, Vec<String>)> =
        via_ids.into_iter().map(|g| (g.id, g.permissions)).collect();
    a.sort_by_key(|(id, _)| *id);
    b.sort_by_key(|(id, _)| *id);
    assert_eq!(a, b);
}

// ── roles create/update (HTTP, default org) ──

#[tokio::test]
async fn manage_roles_holder_cannot_create_a_role_carrying_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let name = format!("escalation-attempt-{}", uuid::Uuid::new_v4().simple());
    let resp = bare()
        .post(api_url(&app, "/api/roles", &holder))
        .json(&serde_json::json!({ "name": name, "permissions": ["admin"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let created = role::Entity::find()
        .filter(role::Column::OrgId.eq(org_id))
        .filter(role::Column::Name.eq(&name))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        created.is_none(),
        "refused role create must not persist a row"
    );
}

#[tokio::test]
async fn manage_roles_holder_cannot_create_a_role_with_a_permission_they_lack() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let resp = bare()
        .post(api_url(&app, "/api/roles", &holder))
        .json(&serde_json::json!({ "name": "should-fail", "permissions": ["view_query"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn manage_roles_holder_cannot_escalate_an_existing_role_via_update() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let target_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("target-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/roles/{target_role_id}"),
            &holder,
        ))
        .json(&serde_json::json!({ "permissions": ["admin"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let refetched = role::Entity::find_by_id(target_role_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.permissions.is_empty(),
        "refused update must leave the role's permissions untouched: {:?}",
        refetched.permissions
    );
}

#[tokio::test]
async fn admin_can_create_a_role_carrying_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let created = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "Full Admin Clone", "permissions": ["admin"] }))
            .send()
            .await
            .unwrap(),
        "admin create admin-carrying role",
    )
    .await;
    assert_eq!(created["permissions"], serde_json::json!(["admin"]));
}

#[tokio::test]
async fn admin_can_update_a_custom_role_to_carry_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let created = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "Upgrade Target", "permissions": [] }))
            .send()
            .await
            .unwrap(),
        "admin create role",
    )
    .await;
    let role_id = created["id"].as_i64().unwrap();

    let updated = ok_json(
        admin
            .post(format!("{}/api/roles/{role_id}", app.address))
            .json(&serde_json::json!({ "permissions": ["admin"] }))
            .send()
            .await
            .unwrap(),
        "admin escalate role to admin",
    )
    .await;
    assert_eq!(updated["permissions"], serde_json::json!(["admin"]));
}

#[tokio::test]
async fn manage_roles_holder_can_create_a_role_with_only_permissions_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    // manage_roles implies view_roles — in-ceiling for this holder.
    let resp = bare()
        .post(api_url(&app, "/api/roles", &holder))
        .json(&serde_json::json!({ "name": "in-ceiling-role", "permissions": ["view_roles"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn manage_roles_holder_can_create_a_role_with_a_key_implied_by_one_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles", "manage_query"]).await;

    let resp = bare()
        .post(api_url(&app, "/api/roles", &holder))
        .json(&serde_json::json!({ "name": "implied-key-role", "permissions": ["execute_query"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn role_update_with_only_a_name_change_is_not_gated() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let target_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("rename-target-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["admin".to_string()]), // holder does NOT hold admin
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/roles/{target_role_id}"),
            &holder,
        ))
        .json(&serde_json::json!({ "name": "renamed-only" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a name-only update must not touch the grant-ceiling gate: {}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn role_create_with_an_unknown_permission_still_returns_400_not_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let resp = bare()
        .post(api_url(&app, "/api/roles", &holder))
        .json(&serde_json::json!({ "name": "bogus-key-role", "permissions": ["not_a_real_key"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "catalog validation must run before the grant-ceiling gate"
    );
}

#[tokio::test]
async fn roles_delete_stays_ungated_for_a_manage_roles_holder() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let target_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("delete-target-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let resp = bare()
        .delete(api_url(
            &app,
            &format!("/api/roles/{target_role_id}"),
            &holder,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

// ── groups add_role / add_member (HTTP, default org) ──

#[tokio::test]
async fn manage_groups_holder_cannot_attach_an_admin_carrying_role_to_a_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;

    let admin_role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "admin-carrier", "permissions": ["admin"] }))
            .send()
            .await
            .unwrap(),
        "admin create admin-carrying role",
    )
    .await;
    let admin_role_id = admin_role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(
                &serde_json::json!({ "name": format!("target-{}", uuid::Uuid::new_v4().simple()) }),
            )
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{group_id}/roles"),
            &holder,
        ))
        .json(&serde_json::json!({ "role_id": admin_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .all(&app.db)
        .await
        .unwrap();
    assert!(links.is_empty(), "refused attach must not persist a link");
}

#[tokio::test]
async fn manage_groups_holder_cannot_self_add_to_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{admin_group_id}/members"),
            &holder,
        ))
        .json(&serde_json::json!({ "user_id": holder.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let refetched = user::Entity::find_by_id(holder.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !refetched.groups.contains(&admin_group_id),
        "a refused self-add must not have written the admin group to the caller's groups"
    );
}

#[tokio::test]
async fn manage_groups_holder_cannot_add_another_user_to_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{admin_group_id}/members"),
            &holder,
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let refetched = user::Entity::find_by_id(victim.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !refetched.groups.contains(&admin_group_id),
        "a refused add must not have written the admin group to the victim's groups"
    );
}

#[tokio::test]
async fn admin_can_attach_an_admin_carrying_role_to_a_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let admin_role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "admin-carrier-2", "permissions": ["admin"] }))
            .send()
            .await
            .unwrap(),
        "admin create admin-carrying role",
    )
    .await;
    let admin_role_id = admin_role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("admin-target-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": admin_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn admin_can_add_a_member_to_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let admin = app.admin_client().await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let resp = admin
        .post(format!(
            "{}/api/groups/{admin_group_id}/members",
            app.address
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn manage_groups_holder_can_attach_a_role_whose_permissions_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups", "view_query"]).await;

    let role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(
                &serde_json::json!({ "name": "in-ceiling-attach", "permissions": ["view_query"] }),
            )
            .send()
            .await
            .unwrap(),
        "admin create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("holder-target-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{group_id}/roles"),
            &holder,
        ))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn manage_groups_holder_can_add_a_member_to_a_group_conferring_only_permissions_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups", "view_query"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let target_group_id = group_with_role(&app.db, org_id, &["view_query"], &[]).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group_id}/members"),
            &holder,
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

/// An idempotent re-add of a member already in a privileged group must not become 403.
#[tokio::test]
async fn re_adding_an_existing_member_of_a_privileged_group_is_still_a_no_op_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let add_resp = admin
        .post(format!(
            "{}/api/groups/{admin_group_id}/members",
            app.address
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_resp.status().as_u16(), 200);

    // Re-add by a caller who could never have performed the *original* add themselves.
    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{admin_group_id}/members"),
            &holder,
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "idempotent re-add of an existing member must stay a no-op 200: {}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn add_member_to_a_group_with_no_linked_roles_is_allowed_for_a_manage_groups_holder() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("no-role-group-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{group_id}/members"),
            &holder,
        ))
        .json(&serde_json::json!({ "user_id": victim.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn add_role_with_an_out_of_org_role_id_still_returns_404_not_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;

    let other_org_id = make_org(&app.db).await;
    let other_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(other_org_id),
        name: Set("other-org-role".to_string()),
        permissions: Set(vec!["admin".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("cross-org-target-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{group_id}/roles"),
            &holder,
        ))
        .json(&serde_json::json!({ "role_id": other_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "the org-scoped role lookup must 404 before the grant-ceiling gate ever runs"
    );
}

// ── require_can_grant_groups: object-ACL and data-source-scope axes, plus assign_data_source / set_data_source_access guards ──

/// Like `user_with_role`, but the caller's group is pinned restricted (no unrestricted data-source
/// reach), so the data-source-scope axis is actually tested.
async fn restricted_caller_with_role(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    perms: &[&str],
) -> user::Model {
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        data_source_access_restricted: Set(true),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();

    let email = format!(
        "gc-restricted-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );
    let user_id = user::Entity::insert(user::ActiveModel {
        org_id: Set(org_id),
        name: Set("Grant Ceiling Restricted Caller".to_string()),
        email: Set(email.clone()),
        password_hash: Set(None),
        groups: Set(vec![group_id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&test_api_key(&email))),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn private_dashboard(db: &sea_orm::DatabaseConnection, org_id: i64, owner_id: i64) -> i64 {
    let now = chrono::Utc::now();
    dashboard::Entity::insert(dashboard::ActiveModel {
        org_id: Set(org_id),
        slug: Set(format!("gc-acl-{}", uuid::Uuid::new_v4().simple())),
        name: Set("grant-ceiling ACL fixture".to_string()),
        user_id: Set(owner_id),
        layout: Set(serde_json::json!([])),
        dashboard_filters_enabled: Set(false),
        is_archived: Set(false),
        visibility: Set("private".to_string()),
        tags: Set(None),
        options: Set(serde_json::json!({})),
        published_snapshot: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id
}

async fn grant_dashboard(
    db: &sea_orm::DatabaseConnection,
    grantor_id: i64,
    dashboard_id: i64,
    grantee_id: Option<i64>,
    grantee_group_id: Option<i64>,
    access_type: &str,
) {
    let now = chrono::Utc::now();
    access_permission::ActiveModel {
        object_type: Set("dashboard".to_string()),
        object_id: Set(dashboard_id),
        access_type: Set(access_type.to_string()),
        grantor_id: Set(grantor_id),
        grantee_id: Set(grantee_id),
        grantee_group_id: Set(grantee_group_id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn insert_data_source(db: &sea_orm::DatabaseConnection, org_id: i64) -> i64 {
    let now = chrono::Utc::now();
    data_source::Entity::insert(data_source::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("gc-ds-{}", uuid::Uuid::new_v4().simple())),
        r#type: Set("pg".to_string()),
        health_state: Set("unknown".to_string()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id
}

/// `add_member` is blocked by the object-ACL axis: the target group's grant on an object exceeds
/// the caller's reach.
#[tokio::test]
async fn add_member_blocked_by_an_object_grant_the_caller_cannot_match() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (owner, _) = user_with_role(&app.db, org_id, &[]).await;
    let dashboard_id = private_dashboard(&app.db, org_id, owner.id).await;

    let target_group = group_with_role(&app.db, org_id, &[], &[]).await;
    grant_dashboard(
        &app.db,
        owner.id,
        dashboard_id,
        None,
        Some(target_group),
        "editor",
    )
    .await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group}/members"),
            &caller,
        ))
        .json(&serde_json::json!({ "user_id": caller.id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 403, "add_member: {body}");
    assert!(
        body.contains("cannot grant access to an object you cannot reach at that level"),
        "{body}"
    );
}

/// The object-ACL axis doesn't over-block: a caller with an equal-or-higher grant on the same
/// object may `add_member`.
#[tokio::test]
async fn add_member_allowed_when_caller_holds_an_equal_or_higher_object_grant() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (owner, _) = user_with_role(&app.db, org_id, &[]).await;
    let dashboard_id = private_dashboard(&app.db, org_id, owner.id).await;

    let target_group = group_with_role(&app.db, org_id, &[], &[]).await;
    grant_dashboard(
        &app.db,
        owner.id,
        dashboard_id,
        None,
        Some(target_group),
        "editor",
    )
    .await;
    grant_dashboard(
        &app.db,
        owner.id,
        dashboard_id,
        Some(caller.id),
        None,
        "editor",
    )
    .await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group}/members"),
            &caller,
        ))
        .json(&serde_json::json!({ "user_id": caller.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

/// `add_member` is blocked by the data-source-scope axis: the target group is unrestricted but the
/// caller has no unrestricted-reach group.
#[tokio::test]
async fn add_member_blocked_by_unrestricted_data_source_access_the_caller_lacks() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let caller = restricted_caller_with_role(&app.db, org_id, &["manage_groups"]).await;

    let target_group = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("gc-unrestricted-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        data_source_access_restricted: Set(false),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group}/members"),
            &caller,
        ))
        .json(&serde_json::json!({ "user_id": caller.id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 403, "add_member: {body}");
    assert!(
        body.contains("cannot grant unrestricted data-source access you do not hold"),
        "{body}"
    );
}

/// `add_member` is blocked by the other data-source-scope half: the target group's allowlist names
/// a data source the caller can't reach.
#[tokio::test]
async fn add_member_blocked_by_a_restricted_groups_allowlist_the_caller_cannot_reach() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let caller = restricted_caller_with_role(&app.db, org_id, &["manage_groups"]).await;
    let ds_id = insert_data_source(&app.db, org_id).await;

    let target_group = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "gc-restricted-allowlist-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        data_source_access_restricted: Set(true),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;
    data_source_group::ActiveModel {
        data_source_id: Set(ds_id),
        group_id: Set(target_group),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("data_source_group insert failed");

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group}/members"),
            &caller,
        ))
        .json(&serde_json::json!({ "user_id": caller.id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 403, "add_member: {body}");
    assert!(
        body.contains("\"message\":\"cannot grant access to a data source you cannot reach\""),
        "must hit permissions.rs's restricted-allowlist branch with its exact string, not the \
         assign_data_source handler's \"you cannot grant...\" wrapper: {body}"
    );
}

/// `POST /api/groups/{id}/data_sources` refuses to attach a data source the caller's groups can't
/// reach.
#[tokio::test]
async fn assign_data_source_blocked_for_a_data_source_the_caller_cannot_reach() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let caller = restricted_caller_with_role(&app.db, org_id, &["manage_groups"]).await;
    let ds_id = insert_data_source(&app.db, org_id).await;
    let target_group = group_with_role(&app.db, org_id, &[], &[]).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/groups/{target_group}/data_sources"),
            &caller,
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 403, "assign_data_source: {body}");
    assert!(
        body.contains("you cannot grant access to a data source you cannot reach"),
        "{body}"
    );
}

/// `PUT /api/groups/{id}/data_source_access {"restricted": false}` is refused for a
/// `manage_groups`-only caller without unrestricted reach, and allowed for an org admin.
#[tokio::test]
async fn set_data_source_access_widen_blocked_for_a_manage_groups_only_caller_but_allowed_for_admin(
) {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let holder = restricted_caller_with_role(&app.db, org_id, &["manage_groups"]).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let target_group = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("gc-widen-target-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        data_source_access_restricted: Set(true),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let blocked = bare()
        .put(api_url(
            &app,
            &format!("/api/groups/{target_group}/data_source_access"),
            &holder,
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    let status = blocked.status().as_u16();
    let body = blocked.text().await.unwrap_or_default();
    assert_eq!(status, 403, "set_data_source_access: {body}");
    assert!(
        body.contains("cannot grant unrestricted data-source access you do not hold"),
        "{body}"
    );

    let allowed = bare()
        .put(api_url(
            &app,
            &format!("/api/groups/{target_group}/data_source_access"),
            &admin,
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        allowed.status().as_u16(),
        200,
        "an org admin must be able to widen: {}",
        allowed.text().await.unwrap_or_default()
    );
}

// ── user_service update / invite / regenerate_api_key ──

/// Builds a custom group+role carrying `perms` and returns its id, standing in for the admin group
/// so tests don't need seeded builtins.
async fn privileged_group_id(db: &sea_orm::DatabaseConnection, org_id: i64, perms: &[&str]) -> i64 {
    let (u, _) = user_with_role(db, org_id, perms).await;
    u.groups[0]
}

#[tokio::test]
async fn manage_users_holder_cannot_add_themselves_to_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;

    let mut groups = holder.groups.clone();
    groups.push(admin_group_id);
    let err = user_service::update(
        &app.db,
        org_id,
        &holder,
        holder.id,
        None,
        Some(groups),
        None,
    )
    .await
    .expect_err("must not self-escalate into an admin-carrying group");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn manage_users_holder_cannot_add_another_user_to_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;

    let mut groups = victim.groups.clone();
    groups.push(admin_group_id);
    let err = user_service::update(
        &app.db,
        org_id,
        &holder,
        victim.id,
        None,
        Some(groups),
        None,
    )
    .await
    .expect_err("must not escalate someone else into an admin-carrying group");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn http_post_users_id_with_the_admin_group_is_403_for_a_manage_users_holder() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let mut groups = victim.groups.clone();
    groups.push(admin_group_id);
    let resp = bare()
        .post(api_url(&app, &format!("/api/users/{}", victim.id), &holder))
        .json(&serde_json::json!({ "groups": groups }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn admin_can_move_a_user_into_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;

    let mut groups = victim.groups.clone();
    groups.push(admin_group_id);
    let result =
        user_service::update(&app.db, org_id, &admin, victim.id, None, Some(groups), None).await;
    assert!(result.is_ok(), "{:?}", result.err());
}

/// Mirror of the add case: a non-admin `manage_users` holder must not strip `admin` by *removing* a
/// user from an admin-conferring group.
#[tokio::test]
async fn manage_users_without_admin_cannot_remove_a_user_from_an_admin_conferring_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    // A standing admin outside the group, so only the grant ceiling (not the last-admin invariant)
    // can block.
    let (_other_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;

    let err = user_service::update(
        &app.db,
        org_id,
        &holder,
        victim.id,
        None,
        Some(vec![]),
        None,
    )
    .await
    .expect_err(
        "must not be able to strip admin by removing the user from an admin-conferring group",
    );
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    let reloaded = user_service::get(&app.db, org_id, victim.id).await.unwrap();
    assert_eq!(
        reloaded.groups, victim.groups,
        "victim's groups must be unchanged after the forbidden update"
    );
}

/// Positive control: an admin can remove a user from an admin-conferring group.
#[tokio::test]
async fn admin_can_remove_a_user_from_an_admin_conferring_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let result =
        user_service::update(&app.db, org_id, &admin, victim.id, None, Some(vec![]), None).await;
    assert!(result.is_ok(), "{:?}", result.err());
    assert!(result.unwrap().groups.is_empty());
}

#[tokio::test]
async fn manage_users_holder_can_add_a_user_to_a_group_conferring_only_permissions_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users", "view_query"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;
    let target_group_id = privileged_group_id(&app.db, org_id, &["view_query"]).await;

    let mut groups = victim.groups.clone();
    groups.push(target_group_id);
    let result = user_service::update(
        &app.db,
        org_id,
        &holder,
        victim.id,
        None,
        Some(groups),
        None,
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
}

/// The ceiling covers the `removed` side of the group diff too: a non-holder can't strip a
/// permission by removing someone from the group that confers it.
#[tokio::test]
async fn removing_a_permission_conferring_group_is_gated_by_the_same_ceiling_as_adding() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    // Not `admin`, which would also trip the last-admin guard; this isolates the grant ceiling.
    let (victim, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let err = user_service::update(
        &app.db,
        org_id,
        &holder,
        victim.id,
        None,
        Some(vec![]),
        None,
    )
    .await
    .expect_err("removal of a group conferring a permission the caller doesn't hold must be gated");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

/// A non-admin `manage_users` holder stripping the org's sole admin is rejected by the ceiling,
/// before the last-admin guard.
#[tokio::test]
async fn manage_users_without_admin_cannot_strip_the_orgs_sole_admin_via_group_removal() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (manager, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (sole_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let err = user_service::update(
        &app.db,
        org_id,
        &manager,
        sole_admin.id,
        None,
        Some(vec![]),
        None,
    )
    .await
    .expect_err("stripping the org's only admin must still be rejected");
    assert_eq!(
        err.status,
        axum::http::StatusCode::FORBIDDEN,
        "the removal-side grant ceiling must reject this before the last-admin guard is reached"
    );
}

/// The last-admin guard is still the backstop: an admin removing their own only admin-conferring
/// group must be rejected.
#[tokio::test]
async fn admin_removing_their_own_sole_admin_group_still_hits_the_last_admin_guard() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (sole_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let err = user_service::update(
        &app.db,
        org_id,
        &sole_admin,
        sole_admin.id,
        None,
        Some(vec![]),
        None,
    )
    .await
    .expect_err("stripping the org's only admin must still be rejected");
    assert_eq!(
        err.status,
        axum::http::StatusCode::BAD_REQUEST,
        "the ceiling passes (self holds admin); the last-admin guard must still catch it"
    );
}

#[tokio::test]
async fn no_op_group_update_with_the_identical_group_list_is_not_gated() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let same_groups = victim.groups.clone();
    let result = user_service::update(
        &app.db,
        org_id,
        &holder,
        victim.id,
        None,
        Some(same_groups),
        None,
    )
    .await;
    assert!(
        result.is_ok(),
        "resubmitting the identical group list (added=[]) must not be gated: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn manage_users_holder_cannot_invite_a_new_user_into_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;
    let email = format!(
        "invite-escalate-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );

    let err = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "Escalation Attempt".to_string(),
        email.clone(),
        vec![admin_group_id],
    )
    .await
    .expect_err("must not be able to invite straight into an admin-carrying group");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    let created = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .one(&app.db)
        .await
        .unwrap();
    assert!(created.is_none(), "a refused invite must not create a row");
}

#[tokio::test]
async fn http_post_api_users_invite_into_the_admin_group_is_403_for_a_manage_users_holder() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = default_builtin_group_id(&app.db, org_id, "admin").await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;

    let resp = bare()
        .post(api_url(&app, "/api/users", &holder))
        .json(&serde_json::json!({
            "name": "HTTP Escalation",
            "email": format!("http-escalate-{}@test.local", uuid::Uuid::new_v4().simple()),
            "groups": [admin_group_id]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn admin_can_invite_directly_into_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;
    let email = format!(
        "admin-invited-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );

    let result = user_service::invite(
        &app.db,
        org_id,
        &admin,
        "Admin Invited".to_string(),
        email,
        vec![admin_group_id],
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
}

#[tokio::test]
async fn manage_users_holder_can_invite_into_a_group_conferring_only_permissions_they_hold() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users", "view_query"]).await;
    let target_group_id = privileged_group_id(&app.db, org_id, &["view_query"]).await;
    let email = format!(
        "in-ceiling-invite-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );

    let result = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "In Ceiling".to_string(),
        email,
        vec![target_group_id],
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
}

#[tokio::test]
async fn invite_with_an_empty_group_list_still_succeeds_for_a_manage_users_holder() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let email = format!("empty-groups-{}@example.com", uuid::Uuid::new_v4().simple());

    let result = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "Empty Groups".to_string(),
        email,
        vec![],
    )
    .await;
    assert!(result.is_ok(), "{:?}", result.err());
}

#[tokio::test]
async fn invite_colliding_with_an_existing_active_email_errors_before_the_ceiling_check() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;
    // An accepted (non-pending) user.
    let (existing_active, _) = user_with_role(&app.db, org_id, &[]).await;

    let err = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "Collision Attempt".to_string(),
        existing_active.email.clone(),
        vec![admin_group_id],
    )
    .await
    .expect_err("colliding email must be rejected");
    assert_eq!(
        err.status,
        axum::http::StatusCode::BAD_REQUEST,
        "the email-collision check must win over the (would-be-403) ceiling check"
    );
}

/// Security-critical: the pending-user resend branch must NOT update `groups`, or an attacker could
/// re-POST a pending invite with escalated groups.
#[tokio::test]
async fn re_inviting_a_pending_user_into_a_privileged_group_takes_the_token_regenerate_branch() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let admin_group_id = privileged_group_id(&app.db, org_id, &["admin"]).await;
    let email = format!(
        "pending-resync-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );

    let (first, _first_token) = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "Pending User".to_string(),
        email.clone(),
        vec![],
    )
    .await
    .expect("first invite (empty groups) must succeed");
    assert!(first.is_invitation_pending);
    assert!(first.groups.is_empty());

    let result = user_service::invite(
        &app.db,
        org_id,
        &holder,
        "Pending User".to_string(),
        email.clone(),
        vec![admin_group_id],
    )
    .await;

    let refetched = user::Entity::find_by_id(first.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    if refetched.groups.contains(&admin_group_id) {
        panic!(
            "SECURITY REGRESSION: re-inviting a pending user changed users.groups to include a \
             group the caller could not grant ({admin_group_id}); resend result: {result:?}"
        );
    }
    assert_eq!(
        refetched.groups,
        Vec::<i64>::new(),
        "the resend branch must leave groups exactly as first-created, not adopt the resend's \
         requested group list"
    );
}

#[tokio::test]
async fn manage_users_holder_cannot_regenerate_an_admins_api_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let before = victim.api_key_hash.clone();

    let err = user_service::regenerate_api_key(&app.db, org_id, &holder, victim.id)
        .await
        .expect_err("must not be able to regenerate an admin's key");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    let refetched = user::Entity::find_by_id(victim.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        refetched.api_key_hash, before,
        "a refused regenerate must not have rotated the key first"
    );
}

#[tokio::test]
async fn admin_can_regenerate_any_users_api_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    let before = victim.api_key_hash.clone();

    let result = user_service::regenerate_api_key(&app.db, org_id, &admin, victim.id).await;
    assert!(result.is_ok(), "{:?}", result.err());

    let refetched = user::Entity::find_by_id(victim.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(refetched.api_key_hash, before);
}

#[tokio::test]
async fn manage_users_holder_can_regenerate_a_strictly_less_privileged_users_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (victim, _) = user_with_role(&app.db, org_id, &[]).await;

    let result = user_service::regenerate_api_key(&app.db, org_id, &holder, victim.id).await;
    assert!(result.is_ok(), "{:?}", result.err());
}

#[tokio::test]
async fn any_user_can_regenerate_their_own_api_key_regardless_of_privilege() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (plain, _) = user_with_role(&app.db, org_id, &[]).await;

    let result = user_service::regenerate_api_key(&app.db, org_id, &plain, plain.id).await;
    assert!(result.is_ok(), "{:?}", result.err());
}

#[tokio::test]
async fn an_admin_can_regenerate_their_own_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let result = user_service::regenerate_api_key(&app.db, org_id, &admin, admin.id).await;
    assert!(result.is_ok(), "{:?}", result.err());
}

/// Characterization: two holders of the same `manage_users` key satisfy each other's ceiling, so
/// either can regenerate the other's API key (the ceiling is key-based, not ownership-based);
/// pinned so tightening is deliberate.
#[tokio::test]
async fn manage_users_holder_can_still_regenerate_another_equally_privileged_users_key() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (holder_a, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (holder_b, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;

    let result = user_service::regenerate_api_key(&app.db, org_id, &holder_a, holder_b.id).await;
    assert!(result.is_ok(), "{:?}", result.err());
}

// ── dashboard_service::share / rotate_share (HTTP, default org) ──

async fn create_dashboard_as(app: &TestApp, u: &user::Model, name: &str) -> (i64, String) {
    let body = ok_json(
        bare()
            .post(api_url(app, "/api/dashboards", u))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    (
        body["id"].as_i64().unwrap(),
        body["slug"].as_str().unwrap().to_string(),
    )
}

/// Attaches a throwaway data source/query/visualization as admin so the dashboard is publishable
/// (publish requires a widget).
async fn attach_minimal_widget(app: &TestApp, dashboard_id: i64) {
    let admin = app.admin_client().await;
    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": format!("gc-ds-{}", uuid::Uuid::new_v4().simple()), "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for widget",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({ "data_source_id": ds_id, "name": format!("gc-q-{}", uuid::Uuid::new_v4().simple()), "query": "select 1" }))
            .send()
            .await
            .unwrap(),
        "create query for widget",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query for widget",
    )
    .await;
    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({ "query_id": query_id, "type": "chart", "name": "gc-viz", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create viz for widget",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();
    let widget_resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dashboard_id, "visualization_id": viz_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        widget_resp.status().as_u16(),
        200,
        "create widget: {}",
        widget_resp.text().await.unwrap_or_default()
    );
}

async fn publish_dashboard_as(app: &TestApp, u: &user::Model, dashboard_id: i64, slug: &str) {
    attach_minimal_widget(app, dashboard_id).await;
    let resp = bare()
        .post(api_url(app, &format!("/api/dashboards/{slug}/publish"), u))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "publish must succeed: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn dashboard_options(app: &TestApp, dashboard_id: i64) -> serde_json::Value {
    use arcodash_core::entities::dashboard;
    dashboard::Entity::find_by_id(dashboard_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .options
}

async fn embed_row_count(app: &TestApp, dashboard_id: i64) -> u64 {
    use sea_orm::PaginatorTrait;
    embed::Entity::find()
        .filter(embed::Column::ObjectType.eq("dashboard"))
        .filter(embed::Column::ObjectId.eq(dashboard_id))
        .count(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn editor_without_share_public_anonymous_cannot_mint_a_legacy_share_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
        ],
    )
    .await;

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "No Anon Perm").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    assert!(dashboard_options(&app, dashboard_id)
        .await
        .get("secret_token")
        .is_none());
    assert_eq!(embed_row_count(&app, dashboard_id).await, 0);
}

#[tokio::test]
async fn editor_without_create_embed_cannot_mint_a_legacy_share_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "share_public_anonymous",
        ],
    )
    .await;

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "No Create Embed").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(embed_row_count(&app, dashboard_id).await, 0);
}

#[tokio::test]
async fn non_owner_non_admin_with_both_share_permissions_cannot_share_someone_elses_dashboard() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;
    let (sharer, _) = user_with_role(
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

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Not Yours").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    // Object-level Editor reach satisfies authorize(Share)'s reach half but must not suffice
    // without owner-or-admin.
    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        dashboard_id,
        Some(sharer.id),
        None,
        "editor",
    )
    .await
    .expect("owner must be able to grant editor ACL access");

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &sharer,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn admin_can_still_mint_and_rotate_a_legacy_share_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (admin, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "admin",
        ],
    )
    .await;

    let (dashboard_id, slug) = create_dashboard_as(&app, &admin, "Admin Share").await;
    publish_dashboard_as(&app, &admin, dashboard_id, &slug).await;

    let minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &admin,
            ))
            .send()
            .await
            .unwrap(),
        "admin mint share",
    )
    .await;
    let token1 = minted["secret_token"].as_str().unwrap().to_string();

    let rotated = ok_json(
        bare()
            .put(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &admin,
            ))
            .send()
            .await
            .unwrap(),
        "admin rotate share",
    )
    .await;
    let token2 = rotated["secret_token"].as_str().unwrap().to_string();
    assert_ne!(token1, token2);
}

#[tokio::test]
async fn dashboard_owner_with_create_embed_and_share_public_anonymous_can_mint_a_legacy_share_token(
) {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
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

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Owner Can Share").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "owner mint share",
    )
    .await;
    let token = minted["secret_token"].as_str().unwrap().to_string();

    let public = bare()
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        public.status().as_u16(),
        200,
        "the minted token must resolve anonymously"
    );
}

#[tokio::test]
async fn the_same_owner_can_rotate_their_legacy_share_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
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

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Owner Rotate").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "owner mint share",
    )
    .await;
    let old_token = minted["secret_token"].as_str().unwrap().to_string();

    let rotated = ok_json(
        bare()
            .put(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "owner rotate share",
    )
    .await;
    let new_token = rotated["secret_token"].as_str().unwrap().to_string();
    assert_ne!(old_token, new_token);

    let old_resolve = bare()
        .get(format!("{}/api/dashboards/public/{old_token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        old_resolve.status().as_u16(),
        404,
        "the rotated-away token must stop resolving"
    );
}

/// A refused rotate must never destroy the live link first.
#[tokio::test]
async fn rotate_share_refuses_before_destroying_an_existing_link() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, owner_role_id) = user_with_role(
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
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Refuse Before Destroy").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    // Admin mints on the owner's behalf (owner-or-admin both satisfy require_can_share_object).
    let minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &admin,
            ))
            .send()
            .await
            .unwrap(),
        "admin mint share for owner's dashboard",
    )
    .await;
    let token = minted["secret_token"].as_str().unwrap().to_string();

    set_role_permissions(
        &app.db,
        owner_role_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
        ],
    )
    .await;

    let rotate_resp = bare()
        .put(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rotate_resp.status().as_u16(), 403);

    let still_resolves = bare()
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        still_resolves.status().as_u16(),
        200,
        "a refused rotate must leave the original live link resolving"
    );

    let opts = dashboard_options(&app, dashboard_id).await;
    assert_eq!(
        opts.get("secret_token").and_then(|v| v.as_str()),
        Some(token.as_str()),
        "a refused rotate must not have touched the stored secret_token"
    );
}

#[tokio::test]
async fn share_refuses_an_underprivileged_caller_even_when_a_token_already_exists() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, owner_role_id) = user_with_role(
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

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Downgrade After Share").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "owner mint share",
    )
    .await;
    let token = minted["secret_token"].as_str().unwrap().to_string();

    set_role_permissions(
        &app.db,
        owner_role_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_embed",
        ],
    )
    .await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body_text = resp.text().await.unwrap_or_default();
    assert!(
        !body_text.contains(&token),
        "a 403 body must never leak an already-existing token: {body_text}"
    );
}

#[tokio::test]
async fn sharing_an_unpublished_dashboard_still_returns_400_not_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
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

    let (_dashboard_id, slug) = create_dashboard_as(&app, &owner, "Never Published").await;

    let resp = bare()
        .post(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an unpublished dashboard must fail require_published before the grant-ceiling gate"
    );
}

#[tokio::test]
async fn revoke_share_stays_at_the_old_authorization_bar() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, owner_role_id) = user_with_role(
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

    let (dashboard_id, slug) = create_dashboard_as(&app, &owner, "Revoke Stays Open").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let _minted = ok_json(
        bare()
            .post(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "owner mint share",
    )
    .await;

    // Owner can no longer mint (would fail require_can_share_publicly)...
    set_role_permissions(
        &app.db,
        owner_role_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    // ...but revoke_share never calls require_can_share_publicly, only authorize(RevokeShare).
    let resp = bare()
        .delete(api_url(
            &app,
            &format!("/api/dashboards/{slug}/share"),
            &owner,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn rotate_share_on_a_dashboard_with_no_existing_token_behaves_as_a_plain_share() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
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

    let (dashboard_id, slug) =
        create_dashboard_as(&app, &owner, "Rotate With No Prior Token").await;
    publish_dashboard_as(&app, &owner, dashboard_id, &slug).await;

    let rotated = ok_json(
        bare()
            .put(api_url(
                &app,
                &format!("/api/dashboards/{slug}/share"),
                &owner,
            ))
            .send()
            .await
            .unwrap(),
        "rotate with no prior token",
    )
    .await;
    assert!(rotated["secret_token"].as_str().is_some());
}

// ── Asymmetric grant ceiling ──
// `require_can_grant` only bounds ADDING a permission; `manage_roles`/`manage_groups` holders without `admin`
// must be equally bounded on REMOVE/detach (`require_can_revoke`).

#[tokio::test]
async fn manage_roles_without_admin_cannot_strip_admin_from_a_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles"]).await;

    let target = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("demote-target-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["admin"],
            }))
            .send()
            .await
            .unwrap(),
        "admin create admin-carrying role",
    )
    .await;
    let role_id = target["id"].as_i64().unwrap();

    // An empty list passes `require_can_grant` trivially; only `require_can_revoke` catches the
    // implied removal of `admin`.
    let resp = bare()
        .post(api_url(&app, &format!("/api/roles/{role_id}"), &holder))
        .json(&serde_json::json!({ "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let refetched = role::Entity::find_by_id(role_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.permissions.iter().any(|p| p == "admin"),
        "a refused demotion must leave the role's admin permission intact: {:?}",
        refetched.permissions
    );
}

#[tokio::test]
async fn manage_roles_without_admin_can_still_edit_a_non_admin_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_roles", "view_query"]).await;

    let target = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("editable-target-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["view_query"],
            }))
            .send()
            .await
            .unwrap(),
        "admin create role",
    )
    .await;
    let role_id = target["id"].as_i64().unwrap();

    // Drops a key the holder does hold; the revoke ceiling must not block an ordinary edit.
    let resp = bare()
        .post(api_url(&app, &format!("/api/roles/{role_id}"), &holder))
        .json(&serde_json::json!({ "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn admin_can_strip_admin_from_a_role_while_another_admin_remains() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;

    // A second admin keeps the last-admin guard out of this revoke-ceiling test.
    let second_admin_role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("second-admin-role-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["admin"],
            }))
            .send()
            .await
            .unwrap(),
        "admin create second admin-carrying role",
    )
    .await;
    let second_admin_role_id = second_admin_role["id"].as_i64().unwrap();
    let second_admin_group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("second-admin-group-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create second admin group",
    )
    .await;
    let second_admin_group_id = second_admin_group["id"].as_i64().unwrap();
    let attach = admin
        .post(format!(
            "{}/api/groups/{second_admin_group_id}/roles",
            app.address
        ))
        .json(&serde_json::json!({ "role_id": second_admin_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(attach.status().as_u16(), 200);
    let (second_admin_user, _) = user_with_role(&app.db, org_id, &[]).await;
    let add_member = admin
        .post(format!(
            "{}/api/groups/{second_admin_group_id}/members",
            app.address
        ))
        .json(&serde_json::json!({ "user_id": second_admin_user.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_member.status().as_u16(), 200);

    let target = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("demotable-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["admin"],
            }))
            .send()
            .await
            .unwrap(),
        "admin create demotable admin-carrying role",
    )
    .await;
    let role_id = target["id"].as_i64().unwrap();

    let resp = admin
        .post(format!("{}/api/roles/{role_id}", app.address))
        .json(&serde_json::json!({ "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "an admin caller must not be blocked by the revoke ceiling: {}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn manage_groups_without_admin_cannot_detach_an_admin_conferring_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;
    let (holder, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;

    let admin_role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("detach-target-role-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["admin"],
            }))
            .send()
            .await
            .unwrap(),
        "admin create admin-carrying role",
    )
    .await;
    let admin_role_id = admin_role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("detach-target-group-{}", uuid::Uuid::new_v4().simple()) }))
            .send()
            .await
            .unwrap(),
        "admin create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let attach = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": admin_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(attach.status().as_u16(), 200);

    let resp = bare()
        .delete(api_url(
            &app,
            &format!("/api/groups/{group_id}/roles/{admin_role_id}"),
            &holder,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let link = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .filter(group_role::Column::RoleId.eq(admin_role_id))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        link.is_some(),
        "a refused detach must leave the group-role link in place"
    );
}
