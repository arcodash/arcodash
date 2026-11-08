//! Integration tests: seeding/backfilling roles must never
//! change any user's effective permission set.

mod common;
use common::ok_json;
use common::spawn_app;

use arcodash_core::entities::{group, group_role, organization, role, user};
use arcodash_core::org_seed::{
    ADMIN_GROUP_PERMISSIONS, DEFAULT_GROUP_PERMISSIONS, VIEWER_ROLE_PERMISSIONS,
};
use sea_orm::{ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};
use std::collections::HashSet;

/// Permission set of the single role linked to `group_id`, via `group_roles -> roles`. Panics if
/// the group has no linked role.
async fn role_permissions_for_group(
    db: &sea_orm::DatabaseConnection,
    group_id: i64,
) -> Vec<String> {
    let link = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .one(db)
        .await
        .expect("group_role query failed")
        .unwrap_or_else(|| panic!("group {group_id} has no linked role"));

    let r = role::Entity::find_by_id(link.role_id)
        .one(db)
        .await
        .expect("role query failed")
        .unwrap_or_else(|| panic!("role {} not found", link.role_id));

    r.permissions
}

fn as_set(v: &[String]) -> HashSet<&str> {
    v.iter().map(|s| s.as_str()).collect()
}
fn as_set_static<'a>(v: &'a [&'a str]) -> HashSet<&'a str> {
    v.iter().copied().collect()
}

/// INVARIANT: after the roles migration/seed, every builtin group's linked role carries the group's
/// exact permission set, and the backfilled "default" org matches a brand-new org (seeded via
/// `seed_builtin_groups`).
#[tokio::test]
async fn migration_preserves_effective_permissions() {
    let app = skip_if_none!(spawn_app().await);

    // Part 1: the migration-seeded "default" org (backfill path): both groups must end up linked to
    // a role with their exact original permissions.
    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist after migrations");

    let admin_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("admin"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("admin group must exist");
    let default_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("default group must exist");

    let admin_role_perms = role_permissions_for_group(&app.db, admin_group.id).await;
    let default_role_perms = role_permissions_for_group(&app.db, default_group.id).await;

    assert_eq!(
        as_set(&admin_role_perms),
        as_set_static(ADMIN_GROUP_PERMISSIONS),
        "admin group's role must carry exactly {ADMIN_GROUP_PERMISSIONS:?} (admin implies all)"
    );
    assert_eq!(
        as_set(&default_role_perms),
        as_set_static(DEFAULT_GROUP_PERMISSIONS),
        "default group's role must carry exactly DEFAULT_GROUP_PERMISSIONS"
    );

    let viewer_role = role::Entity::find()
        .filter(role::Column::OrgId.eq(default_org.id))
        .filter(role::Column::Name.eq("Viewer"))
        .one(&app.db)
        .await
        .expect("viewer role query failed")
        .expect("Viewer builtin role must be seeded per org");
    let viewers_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("viewers group must exist");
    let viewers_role_perms = role_permissions_for_group(&app.db, viewers_group.id).await;
    assert_eq!(
        as_set(&viewers_role_perms),
        as_set_static(VIEWER_ROLE_PERMISSIONS),
        "viewers group's role must carry exactly VIEWER_ROLE_PERMISSIONS"
    );
    let viewers_links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(viewers_group.id))
        .all(&app.db)
        .await
        .expect("group_role query failed");
    assert_eq!(viewers_links.len(), 1);
    assert_eq!(viewers_links[0].role_id, viewer_role.id);

    // Part 2: a brand-new org (inline seed_builtin_groups path), using a random-suffixed slug since
    // cleanup() doesn't truncate `organizations`.
    let new_org = organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set("Roles Invariant Org".to_string()),
        slug: Set(format!("roles-invariant-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("new org insert failed")
    .last_insert_id;

    let seeded_groups = arcodash_core::org_seed::seed_builtin_groups(&app.db, new_org)
        .await
        .expect("seed_builtin_groups failed");

    let new_admin_perms = role_permissions_for_group(&app.db, seeded_groups.admin_group_id).await;
    let new_default_perms =
        role_permissions_for_group(&app.db, seeded_groups.default_group_id).await;

    assert_eq!(
        as_set(&new_admin_perms),
        as_set_static(ADMIN_GROUP_PERMISSIONS),
        "freshly-seeded org: admin group's role must carry exactly ADMIN_GROUP_PERMISSIONS"
    );
    assert_eq!(
        as_set(&new_default_perms),
        as_set_static(DEFAULT_GROUP_PERMISSIONS),
        "freshly-seeded org: default group's role must carry exactly DEFAULT_GROUP_PERMISSIONS"
    );

    assert_eq!(as_set(&admin_role_perms), as_set(&new_admin_perms));
    assert_eq!(as_set(&default_role_perms), as_set(&new_default_perms));

    let new_viewer_perms = role_permissions_for_group(&app.db, seeded_groups.viewer_group_id).await;
    assert_eq!(
        as_set(&new_viewer_perms),
        as_set_static(VIEWER_ROLE_PERMISSIONS),
        "freshly-seeded org: viewers group's role must carry exactly VIEWER_ROLE_PERMISSIONS"
    );
}

/// `load_effective_groups` must derive permissions from a group's linked role(s), not its vestigial
/// `permissions` column: an empty-column group linked to a `["create_query"]` role is granted
/// `create_query`.
#[tokio::test]
async fn effective_permissions_come_from_roles() {
    let app = skip_if_none!(spawn_app().await);

    let org_id = organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set("Effective Perms Org".to_string()),
        slug: Set(format!("effective-perms-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("org insert failed")
    .last_insert_id;

    let role_id = role::Entity::insert(role::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Query Author".to_string()),
        permissions: Set(vec!["create_query".to_string()]),
        r#type: Set("custom".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("role insert failed")
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("query-authors".to_string()),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
        data_source_access_restricted: Set(false),
    })
    .exec(&app.db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        id: NotSet,
        group_id: Set(group_id),
        role_id: Set(role_id),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("group_role insert failed");

    let user_id = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Effective Perms User".to_string()),
        email: Set(format!(
            "effective-perms-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        password_hash: Set(None),
        groups: Set(vec![group_id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("user insert failed")
    .last_insert_id;

    let user = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("user must exist after insert");

    let groups = arcodash_backend::authz::permissions::load_effective_groups(&app.db, &user)
        .await
        .expect("load_effective_groups failed");

    let g = groups
        .iter()
        .find(|g| g.id == group_id)
        .expect("user's group must be present in load_effective_groups's result");
    assert!(
        g.permissions.iter().any(|p| p == "create_query"),
        "group's effective permissions must come from its linked role, not its own (empty) column"
    );
    assert!(arcodash_backend::authz::permissions::user_has_permission(
        &user,
        &groups,
        "create_query"
    ));
}

/// POST /api/roles with an unknown permission key is rejected before reaching the DB
/// (`permissions_catalog.rs` is the source of truth).
#[tokio::test]
async fn create_role_validates_permissions_against_catalog() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({
            "name": "Analyst",
            "permissions": ["create_query", "bogus"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// POST /api/roles → 200, then the created role shows up in GET /api/roles as type="custom".
#[tokio::test]
async fn create_and_list_role_roundtrips() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": "Report Viewer",
                "permissions": ["view_query", "view_dashboard"]
            }))
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    assert_eq!(created["name"], "Report Viewer");
    assert_eq!(created["type"], "custom");
    assert_eq!(created["group_count"], 0);
    let role_id = created["id"].as_i64().unwrap();

    let list = ok_json(
        client
            .get(format!("{}/api/roles", app.address))
            .send()
            .await
            .unwrap(),
        "list roles",
    )
    .await;
    let items = list.as_array().unwrap();
    let found = items
        .iter()
        .find(|r| r["id"].as_i64() == Some(role_id))
        .unwrap_or_else(|| panic!("created role id={role_id} must appear in list; got {list}"));
    assert_eq!(found["type"], "custom");
}

/// The batched `GET /api/roles` list's `group_count` must match a per-role `COUNT` for
/// zero/one/many attached groups.
#[tokio::test]
async fn list_roles_group_count_matches_actual_attachments() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let make_role = |name: &str| {
        let name = name.to_string();
        let client = client.clone();
        let address = app.address.clone();
        async move {
            ok_json(
                client
                    .post(format!("{address}/api/roles"))
                    .json(&serde_json::json!({ "name": name, "permissions": [] }))
                    .send()
                    .await
                    .unwrap(),
                "create role",
            )
            .await["id"]
                .as_i64()
                .unwrap()
        }
    };

    let no_groups_role = make_role("Count Test: No Groups").await;
    let one_group_role = make_role("Count Test: One Group").await;
    let many_groups_role = make_role("Count Test: Many Groups").await;

    let make_group = |name: &str, role_id: i64| {
        let name = name.to_string();
        let client = client.clone();
        let address = app.address.clone();
        async move {
            let group_id = ok_json(
                client
                    .post(format!("{address}/api/groups"))
                    .json(&serde_json::json!({ "name": name }))
                    .send()
                    .await
                    .unwrap(),
                "create group",
            )
            .await["id"]
                .as_i64()
                .unwrap();
            let attach = client
                .post(format!("{address}/api/groups/{group_id}/roles"))
                .json(&serde_json::json!({ "role_id": role_id }))
                .send()
                .await
                .unwrap();
            assert_eq!(attach.status().as_u16(), 200);
            group_id
        }
    };

    make_group("Count Test Group 1", one_group_role).await;
    for i in 0..3 {
        make_group(&format!("Count Test Group Many {i}"), many_groups_role).await;
    }

    let list = ok_json(
        client
            .get(format!("{}/api/roles", app.address))
            .send()
            .await
            .unwrap(),
        "list roles",
    )
    .await;
    let by_id = |id: i64| -> serde_json::Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_i64() == Some(id))
            .unwrap_or_else(|| panic!("role id={id} must appear in list; got {list}"))
            .clone()
    };

    assert_eq!(by_id(no_groups_role)["group_count"], 0);
    assert_eq!(by_id(one_group_role)["group_count"], 1);
    assert_eq!(by_id(many_groups_role)["group_count"], 3);
}

/// Builtin roles (seeded per org, e.g. "Admin") can never be edited or deleted via the API.
#[tokio::test]
async fn builtin_role_cannot_be_edited_or_deleted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");
    let admin_role = role::Entity::find()
        .filter(role::Column::OrgId.eq(default_org.id))
        .filter(role::Column::Name.eq("Admin"))
        .one(&app.db)
        .await
        .expect("role query failed")
        .expect("builtin Admin role must exist");

    let update_resp = client
        .post(format!("{}/api/roles/{}", app.address, admin_role.id))
        .json(&serde_json::json!({ "name": "Unauthorized Rename" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 403);

    let delete_resp = client
        .delete(format!("{}/api/roles/{}", app.address, admin_role.id))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 403);
}

/// A custom role still linked to a group cannot be deleted (409), so a group's permission grant
/// isn't silently orphaned.
#[tokio::test]
async fn deleting_role_linked_to_a_group_is_blocked() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "Linked Role", "permissions": [] }))
            .send()
            .await
            .unwrap(),
        "create role for link test",
    )
    .await;
    let role_id = created["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "linked-role-group" }))
            .send()
            .await
            .unwrap(),
        "create group for link test",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let now = chrono::Utc::now().into();
    group_role::Entity::insert(group_role::ActiveModel {
        id: NotSet,
        group_id: Set(group_id),
        role_id: Set(role_id),
        created_at: Set(now),
        updated_at: Set(now),
    })
    .exec(&app.db)
    .await
    .expect("group_role insert failed");

    let delete_resp = client
        .delete(format!("{}/api/roles/{}", app.address, role_id))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 409);
}

/// POST assigns a role to a group (idempotently), GET lists it, a user in the group gets the role's
/// permissions, and DELETE unassigns it.
#[tokio::test]
async fn assign_and_unassign_role_to_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let role = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(
                &serde_json::json!({ "name": "Assignment Role", "permissions": ["create_query"] }),
            )
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "role-assignment-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    for _ in 0..2 {
        let resp = client
            .post(format!("{}/api/groups/{}/roles", app.address, group_id))
            .json(&serde_json::json!({ "role_id": role_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    let list = ok_json(
        client
            .get(format!("{}/api/groups/{}/roles", app.address, group_id))
            .send()
            .await
            .unwrap(),
        "list group roles",
    )
    .await;
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 1, "assignment must be idempotent: {list}");
    assert_eq!(items[0]["id"], role_id);
    assert_eq!(items[0]["name"], "Assignment Role");

    let links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .filter(group_role::Column::RoleId.eq(role_id))
        .all(&app.db)
        .await
        .expect("group_role query failed");
    assert_eq!(links.len(), 1, "no duplicate group_role rows");

    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");
    let user_id = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(default_org.id),
        name: Set("Assignment Member".to_string()),
        email: Set(format!(
            "role-assignment-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        password_hash: Set(None),
        groups: Set(vec![group_id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("user insert failed")
    .last_insert_id;
    let member = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("user must exist");

    let groups = arcodash_backend::authz::permissions::load_effective_groups(&app.db, &member)
        .await
        .expect("load_effective_groups failed");
    assert!(arcodash_backend::authz::permissions::user_has_permission(
        &member,
        &groups,
        "create_query"
    ));

    let delete_resp = client
        .delete(format!(
            "{}/api/groups/{}/roles/{}",
            app.address, group_id, role_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 200);

    let list_after = ok_json(
        client
            .get(format!("{}/api/groups/{}/roles", app.address, group_id))
            .send()
            .await
            .unwrap(),
        "list group roles after delete",
    )
    .await;
    assert_eq!(list_after.as_array().unwrap().len(), 0);

    let groups_after =
        arcodash_backend::authz::permissions::load_effective_groups(&app.db, &member)
            .await
            .expect("load_effective_groups failed");
    assert!(!arcodash_backend::authz::permissions::user_has_permission(
        &member,
        &groups_after,
        "create_query"
    ));
}

/// A group can't be assigned a role from a different org: the role lookup is scoped by `org_id`,
/// else an admin could grant another org's permissions.
#[tokio::test]
async fn assigning_role_from_another_org_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "cross-org-role-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let other_org_id = organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set("Other Org".to_string()),
        slug: Set(format!("other-org-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("other org insert failed")
    .last_insert_id;

    let now = chrono::Utc::now().into();
    let other_role_id = role::Entity::insert(role::ActiveModel {
        id: NotSet,
        org_id: Set(other_org_id),
        name: Set("Other Org Role".to_string()),
        permissions: Set(vec!["create_query".to_string()]),
        r#type: Set("custom".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
    })
    .exec(&app.db)
    .await
    .expect("other org role insert failed")
    .last_insert_id;

    let resp = client
        .post(format!("{}/api/groups/{}/roles", app.address, group_id))
        .json(&serde_json::json!({ "role_id": other_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .all(&app.db)
        .await
        .expect("group_role query failed");
    assert!(links.is_empty(), "no link must be created on rejection");
}

/// SECURITY: object-ACL sharing gates "owner or admin" on role-derived permissions: an admin only
/// via a linked role may share, a non-owner without admin is refused.
#[tokio::test]
async fn acl_grant_authz_reads_role_derived_admin() {
    use arcodash_core::entities::{data_source, query};
    let app = skip_if_none!(spawn_app().await);

    let org_id = organization::Entity::insert(organization::ActiveModel {
        name: Set("ACL Role Admin Org".to_string()),
        slug: Set(format!("acl-role-admin-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("org insert failed")
    .last_insert_id;

    let owner_id = user::Entity::insert(user::ActiveModel {
        org_id: Set(org_id),
        name: Set("Owner".to_string()),
        email: Set(format!(
            "owner-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        groups: Set(vec![]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("owner insert failed")
    .last_insert_id;

    let ds_id = data_source::Entity::insert(data_source::ActiveModel {
        org_id: Set(org_id),
        name: Set("ds".to_string()),
        r#type: Set("postgres".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("data_source insert failed")
    .last_insert_id;

    let query_id = query::Entity::insert(query::ActiveModel {
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        name: Set("q".to_string()),
        query: Set("select 1".to_string()),
        query_hash: Set(uuid::Uuid::new_v4().simple().to_string()),
        api_key: Set(uuid::Uuid::new_v4().simple().to_string()),
        user_id: Set(owner_id),
        options: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("query insert failed")
    .last_insert_id;

    async fn user_via_role(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        perms: Vec<String>,
    ) -> user::Model {
        let role_id = role::Entity::insert(role::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
            permissions: Set(perms),
            r#type: Set("custom".to_string()),
            ..Default::default()
        })
        .exec(db)
        .await
        .expect("role insert failed")
        .last_insert_id;
        let group_id = group::Entity::insert(group::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("grp-{}", uuid::Uuid::new_v4().simple())),
            permissions: Set(vec![]),
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
        .expect("link insert failed");
        let uid = user::Entity::insert(user::ActiveModel {
            org_id: Set(org_id),
            name: Set("sharer".to_string()),
            email: Set(format!(
                "sharer-{}@example.com",
                uuid::Uuid::new_v4().simple()
            )),
            groups: Set(vec![group_id]),
            api_key_hash: Set(arcodash_core::hash::hash_api_key(
                &uuid::Uuid::new_v4().simple().to_string(),
            )),
            details: Set(serde_json::json!({})),
            digest_enabled: Set(false),
            ..Default::default()
        })
        .exec(db)
        .await
        .expect("user insert failed")
        .last_insert_id;
        user::Entity::find_by_id(uid)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    let admin_via_role = user_via_role(&app.db, org_id, vec!["admin".to_string()]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &admin_via_role,
        "query",
        query_id,
        Some(owner_id),
        None,
        "view",
    )
    .await
    .expect("admin-via-role must be allowed to share");

    // Role lacking admin, not owner, no grant: zero view-reach, so the uniform 404 refuses it
    // rather than a 403 that would confirm the query exists.
    let non_admin = user_via_role(&app.db, org_id, vec!["view_query".to_string()]).await;
    let err = arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &non_admin,
        "query",
        query_id,
        Some(owner_id),
        None,
        "view",
    )
    .await
    .expect_err("non-admin non-owner must be refused");
    assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
}

/// `GET /api/users/{id}/effective_permissions` returns the flat permission set derived from the
/// user's groups' linked roles (admin-only endpoint).
#[tokio::test]
async fn effective_permissions_endpoint() {
    let app = skip_if_none!(spawn_app().await);
    let client = app.admin_client().await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");

    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org.id),
        name: Set(format!("viewer-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["view_query".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("role insert failed")
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org.id),
        name: Set(format!("viewers-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("link insert failed");

    let user_id = user::Entity::insert(user::ActiveModel {
        org_id: Set(org.id),
        name: Set("Viewer User".to_string()),
        email: Set(format!(
            "viewer-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        groups: Set(vec![group_id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("user insert failed")
    .last_insert_id;

    let body = ok_json(
        client
            .get(format!(
                "{}/api/users/{}/effective_permissions",
                app.address, user_id
            ))
            .send()
            .await
            .unwrap(),
        "effective permissions",
    )
    .await;

    let perms: Vec<String> = body["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(perms, vec!["view_query".to_string()]);
    assert_eq!(body["is_admin"], false);
}

/// Invite a user via HTTP, accept the invitation, and log in as them. Duplicated locally since each
/// test binary compiles separately.
async fn invite_and_login_scoped(
    app: &common::TestApp,
    admin: &reqwest::Client,
    email: &str,
    name: &str,
    group_ids: Vec<i64>,
) -> reqwest::Client {
    let password = "P@ssw0rd!";
    let invite_resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": name, "email": email, "groups": group_ids }))
        .send()
        .await
        .unwrap();
    let status = invite_resp.status().as_u16();
    let body_text = invite_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "invite user: {body_text}");
    let invite_body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let user_id = invite_body["id"].as_i64().expect("invite must return id");

    let token = {
        let u = user::Entity::find_by_id(user_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("invited user must exist");
        u.details["invitation_token"]
            .as_str()
            .expect("invitation_token must be in details")
            .to_string()
    };

    let accept_client = reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let sess = accept_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();
    let accept_resp = accept_client
        .post(format!("{}/api/users/invitation/accept", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "token": token, "name": name, "password": password }))
        .send()
        .await
        .unwrap();
    let status = accept_resp.status().as_u16();
    let body_text = accept_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "accept invitation: {body_text}");

    let (session, csrf_login) = app.login(email, password).await;
    common::authed_client(&app.address, &session, &csrf_login)
}

/// Create a role with `perms`, a group linked to it, and return the group id, via the admin HTTP
/// API so the assignment path is exercised.
async fn role_scoped_group(admin: &reqwest::Client, app: &common::TestApp, perms: &[&str]) -> i64 {
    let role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("role-{}", uuid::Uuid::new_v4().simple()),
                "permissions": perms
            }))
            .send()
            .await
            .unwrap(),
        "create scoped role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(
                &serde_json::json!({ "name": format!("group-{}", uuid::Uuid::new_v4().simple()) }),
            )
            .send()
            .await
            .unwrap(),
        "create scoped group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let assign_resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_resp.status().as_u16(), 200);

    // New groups default to restricted; keep this one unrestricted so callers don't trip the widen
    // guard.
    let unrestrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict_resp.status().as_u16(), 200);

    group_id
}

/// A `manage_groups`-only user can create a group and read `/api/roles` (the OR-with-manage_groups
/// carve-out so a groups-manager can attach roles), but 403s on role create.
#[tokio::test]
async fn manage_groups_only_can_create_group_and_read_roles_but_not_create_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let group_id = role_scoped_group(&admin, &app, &["manage_groups"]).await;
    let user = invite_and_login_scoped(
        &app,
        &admin,
        "groups-mgr@test.local",
        "Groups Mgr",
        vec![group_id],
    )
    .await;

    let create_group = user
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "created-by-groups-mgr" }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_group.status().as_u16(), 200);

    let read_roles = user
        .get(format!("{}/api/roles", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(read_roles.status().as_u16(), 200);

    let create_role = user
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({ "name": "should-fail", "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_role.status().as_u16(), 403);
}

/// A `manage_roles`-only user can CRUD roles but 403s on group create.
#[tokio::test]
async fn manage_roles_only_can_crud_roles_but_not_create_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let group_id = role_scoped_group(&admin, &app, &["manage_roles"]).await;
    let user = invite_and_login_scoped(
        &app,
        &admin,
        "roles-mgr@test.local",
        "Roles Mgr",
        vec![group_id],
    )
    .await;

    // `manage_roles` alone doesn't imply `view_query`: the grant-ceiling gate refuses roles
    // carrying permissions the caller lacks, so use an in-ceiling set.
    let created = ok_json(
        user.post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "roles-mgr-made-this", "permissions": [] }))
            .send()
            .await
            .unwrap(),
        "roles-mgr create role",
    )
    .await;
    let role_id = created["id"].as_i64().unwrap();

    let update_resp = user
        .post(format!("{}/api/roles/{role_id}", app.address))
        .json(&serde_json::json!({ "name": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);

    let delete_resp = user
        .delete(format!("{}/api/roles/{role_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 200);

    let create_group = user
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "should-fail" }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_group.status().as_u16(), 403);
}

/// A `create_data_source`-only user can create but not edit a data source (separate keys). Denied
/// as 404, not 403: creating grants no `data_source_groups` reach, and every object-bound
/// `authorize()` denial maps to not-found.
#[tokio::test]
async fn create_data_source_only_can_create_but_not_edit() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let group_id = role_scoped_group(&admin, &app, &["create_data_source"]).await;
    let user = invite_and_login_scoped(
        &app,
        &admin,
        "ds-creator@test.local",
        "DS Creator",
        vec![group_id],
    )
    .await;

    let created = ok_json(
        user.post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "creator-made-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create_data_source-only create",
    )
    .await;
    let ds_id = created["id"].as_i64().unwrap();

    let edit_resp = user
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({ "name": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(edit_resp.status().as_u16(), 404);
}

/// The builtin Editor/Viewer roles retain equivalent access under their granular keys —
/// an Editor can still create a dashboard; a Viewer still cannot.
#[tokio::test]
async fn editor_and_viewer_builtins_retain_equivalent_access_post_migration() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");
    let default_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("builtin default (Editor) group must exist");

    let editor = invite_and_login_scoped(
        &app,
        &admin,
        "editor-builtin@test.local",
        "Editor Builtin",
        vec![default_group.id],
    )
    .await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "editor-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "admin create ds for editor test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let create_dashboard = editor
        .post(format!("{}/api/dashboards", app.address))
        .json(&serde_json::json!({ "name": "editor dashboard" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        create_dashboard.status().as_u16(),
        200,
        "Editor builtin must retain dashboard creation post-migration"
    );

    let create_ds = editor
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "editor-cant", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_ds.status().as_u16(), 403);
    let _ = ds_id;
}

/// An admin's session payload reports `is_admin: true` and a non-empty `permissions` array.
#[tokio::test]
async fn admin_session_reports_is_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let body = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;

    assert_eq!(body["is_admin"], true);
    assert!(!body["user"].is_null());
    let perms: Vec<String> = body["permissions"]
        .as_array()
        .expect("permissions must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !perms.is_empty(),
        "admin session must report non-empty permissions"
    );
}

/// A `manage_query`-only user's session EXPANDS `manage_query` into every granular query key via
/// `expand_permissions`, with `is_admin: false`.
#[tokio::test]
async fn manage_query_only_session_reports_expanded_query_permissions() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let group_id = role_scoped_group(&admin, &app, &["manage_query"]).await;
    let user = invite_and_login_scoped(
        &app,
        &admin,
        "query-mgr-session@test.local",
        "Query Mgr Session",
        vec![group_id],
    )
    .await;

    let body = ok_json(
        user.get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "manage_query-only session",
    )
    .await;

    assert_eq!(body["is_admin"], false);
    let perms: HashSet<String> = body["permissions"]
        .as_array()
        .expect("permissions must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for key in [
        "view_query",
        "create_query",
        "edit_query",
        "execute_query",
        "schedule_query",
        "manage_query",
    ] {
        assert!(
            perms.contains(key),
            "expanded permissions must include {key}: {perms:?}"
        );
    }
}

/// A `manage_users`-only session EXPANDS `manage_users` into `view_users` and `view_groups` (the
/// invite dialog needs group visibility).
#[tokio::test]
async fn manage_users_only_session_reports_expanded_view_groups() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let group_id = role_scoped_group(&admin, &app, &["manage_users"]).await;
    let user = invite_and_login_scoped(
        &app,
        &admin,
        "user-mgr-session@test.local",
        "User Mgr Session",
        vec![group_id],
    )
    .await;

    let body = ok_json(
        user.get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "manage_users-only session",
    )
    .await;

    assert_eq!(body["is_admin"], false);
    let perms: HashSet<String> = body["permissions"]
        .as_array()
        .expect("permissions must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for key in ["view_users", "view_groups", "manage_users"] {
        assert!(
            perms.contains(key),
            "expanded permissions must include {key}: {perms:?}"
        );
    }
}

/// SECURITY: a user in the builtin "viewers" group is read-only over HTTP: denied every
/// write/execute path and the user directory, but can read dashboards and queries.
#[tokio::test]
async fn viewers_group_member_is_read_only_over_http() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");
    let viewers_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("builtin viewers group must exist");

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "viewer-fixture-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "admin create ds for viewer test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let existing_query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "viewer-fixture-query",
                "query": "select 1"
            }))
            .send()
            .await
            .unwrap(),
        "admin create query for viewer test",
    )
    .await;
    let query_id = existing_query["id"].as_i64().unwrap();

    let viewer = invite_and_login_scoped(
        &app,
        &admin,
        "readonly-viewer@test.local",
        "Readonly Viewer",
        vec![viewers_group.id],
    )
    .await;

    let create_query = viewer
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "viewer-attempt",
            "query": "select 1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        create_query.status().as_u16(),
        403,
        "viewer must not create queries"
    );

    let execute_query = viewer
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    // Executing a saved query without reach gets the opaque 404 that hides existence; 403 is
    // reserved for callers with an explicit grant.
    assert_eq!(
        execute_query.status().as_u16(),
        404,
        "viewer must not execute queries"
    );

    let list_users = viewer
        .get(format!("{}/api/users", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_users.status().as_u16(),
        403,
        "viewer must not read the user directory"
    );

    let list_dashboards = viewer
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_dashboards.status().as_u16(),
        200,
        "viewer must be able to read dashboards"
    );

    let list_queries = viewer
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_queries.status().as_u16(),
        200,
        "viewer must be able to read queries"
    );
}

/// A logged-out session reports no permissions and `is_admin: false`.
#[tokio::test]
async fn logged_out_session_has_no_permissions() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let body = ok_json(
        bare.get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "logged-out session",
    )
    .await;

    assert!(body["user"].is_null());
    assert_eq!(body["is_admin"], false);
    assert_eq!(body["permissions"].as_array().unwrap().len(), 0);
}

/// Creating a second role with a name already used in the same org must be a 409 with a
/// readable message, not a raw DB error string or a 500.
#[tokio::test]
async fn create_role_duplicate_name_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": "dup-name-role",
                "permissions": ["view_query"]
            }))
            .send()
            .await
            .unwrap(),
        "create first role",
    )
    .await;

    let resp = client
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({
            "name": "dup-name-role",
            "permissions": ["view_query"]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 409, "duplicate role name: {body}");
    assert!(
        body.contains("already exists"),
        "must be a readable message, not a raw DB error; got: {body}"
    );
}

/// Renaming a role onto an existing name in the same org must be a 409, and the rejected
/// rename must not partially apply — the role being renamed keeps its original name.
#[tokio::test]
async fn rename_role_onto_existing_name_returns_409_and_does_not_apply() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "X", "permissions": ["view_query"] }))
            .send()
            .await
            .unwrap(),
        "create role X",
    )
    .await;

    let role_y = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "Y", "permissions": ["view_query"] }))
            .send()
            .await
            .unwrap(),
        "create role Y",
    )
    .await;
    let role_y_id = role_y["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/roles/{role_y_id}", app.address))
        .json(&serde_json::json!({ "name": "X" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 409, "rename onto existing name: {body}");

    let after = ok_json(
        client
            .get(format!("{}/api/roles/{role_y_id}", app.address))
            .send()
            .await
            .unwrap(),
        "fetch role Y after rejected rename",
    )
    .await;
    assert_eq!(
        after["name"], "Y",
        "rejected rename must not partially apply; got {after}"
    );
}
