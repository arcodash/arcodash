//! Integration tests: group CRUD, member management, data-source assignment.

mod common;
use common::{api_key_client, default_org_id, ok_json};
use common::{spawn_app, test_api_key, user_with_role};

/// POST /api/groups → 200, group created with expected fields.
#[tokio::test]
async fn create_group_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let body = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "analysts" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;

    assert_eq!(body["name"], "analysts");
    assert!(!body["id"].is_null());
}

/// POST /api/groups/:id renaming a builtin group → 400 and the name is untouched (readers key on
/// builtin names).
#[tokio::test]
async fn rename_builtin_group_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;

    let resp = client
        .post(format!("{}/api/groups/{admin_group_id}", app.address))
        .json(&serde_json::json!({ "name": "not-admin-anymore" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "renaming a builtin group must be rejected"
    );

    use arcodash_core::entities::group;
    use sea_orm::EntityTrait;
    let reloaded = group::Entity::find_by_id(admin_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.name, "admin",
        "a rejected rename must leave the builtin group's name untouched"
    );
}

/// POST /api/groups with an existing name → 409 (mapped from the `idx_groups_org_id_name` unique
/// violation), not 500.
#[tokio::test]
async fn create_group_with_duplicate_name_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let name = format!("dup-group-{}", uuid::Uuid::new_v4());

    let first = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);

    let second = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        409,
        "a duplicate group name in the same org must be rejected, not silently duplicated"
    );
}

/// Renaming a custom group onto an existing name → 409, same mapping as create.
#[tokio::test]
async fn rename_group_to_a_duplicate_name_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let taken_name = format!("taken-{}", uuid::Uuid::new_v4());
    let renamed = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": taken_name }))
            .send()
            .await
            .unwrap(),
        "create first group",
    )
    .await;
    let other = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": format!("other-{}", uuid::Uuid::new_v4()) }))
            .send()
            .await
            .unwrap(),
        "create second group",
    )
    .await;
    let other_id = other["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/groups/{other_id}", app.address))
        .json(&serde_json::json!({ "name": renamed["name"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
}

/// POST /api/groups/:id/members → member appears in GET /api/groups/:id/members.
#[tokio::test]
async fn add_member_appears_in_member_list() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    // admin_client uses static Cookie + X-CSRF-Token headers, so csrf cookie rotation on GET
    // /session is ignored.
    let (session_cookie, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;
    let client = common::authed_client(&app.address, &session_cookie, &csrf);

    let user_resp: serde_json::Value = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_id = user_resp["user"]["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "member-test-group" }))
            .send()
            .await
            .unwrap(),
        "create group for member test",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let add_resp = client
        .post(format!("{}/api/groups/{group_id}/members", app.address))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
        .unwrap();
    let add_status = add_resp.status().as_u16();
    let add_text = add_resp.text().await.unwrap_or_default();
    assert_eq!(add_status, 200, "add member: {add_text}");

    // A user in some OTHER group must not show up: catches a predicate that doesn't filter on `id`.
    let org_id = default_org_id(&app.db).await;
    let (other_user, _other_group_id) = user_with_role(&app.db, org_id, &["view_groups"]).await;

    let members_resp = client
        .get(format!("{}/api/groups/{group_id}/members", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(members_resp.status().as_u16(), 200);
    let members: serde_json::Value = members_resp.json().await.unwrap();
    let members_arr = members.as_array().unwrap();
    let found = members_arr
        .iter()
        .any(|m| m["id"].as_i64() == Some(user_id));
    assert!(
        found,
        "user id={user_id} must appear in group members; got: {members}"
    );
    let leaked = members_arr
        .iter()
        .any(|m| m["id"].as_i64() == Some(other_user.id));
    assert!(
        !leaked,
        "a user in a different group must not appear in this group's member list; got: {members}"
    );
}

/// POST /api/groups/:id/data_sources → data source appears in GET /api/groups/:id/data_sources.
#[tokio::test]
async fn assign_data_source_appears_in_group_data_sources() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "group-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for group test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "ds-access-group" }))
            .send()
            .await
            .unwrap(),
        "create group for ds assignment test",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let assign_resp = client
        .post(format!(
            "{}/api/groups/{group_id}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    let assign_status = assign_resp.status().as_u16();
    let assign_text = assign_resp.text().await.unwrap_or_default();
    assert_eq!(assign_status, 200, "assign data source: {assign_text}");

    let list_resp = client
        .get(format!(
            "{}/api/groups/{group_id}/data_sources",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list: serde_json::Value = list_resp.json().await.unwrap();
    let items = list["entries"]
        .as_array()
        .expect("response must be the { restricted, entries } envelope");
    let found = items
        .iter()
        .any(|d| d["id"].as_i64() == Some(ds_id) || d["data_source_id"].as_i64() == Some(ds_id));
    assert!(
        found,
        "data source id={ds_id} must appear in group data sources; got: {list}"
    );
}

/// PUT /api/groups/:id/data_source_access toggles `restricted`; a group id from another org 404s.
#[tokio::test]
async fn set_data_source_access_toggles_restricted_and_guards_cross_org() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "restriction-test-group" }))
            .send()
            .await
            .unwrap(),
        "create group for restriction test",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    // A brand-new group defaults to restricted (closes the create-then-widen escalation).
    let initial = ok_json(
        client
            .get(format!(
                "{}/api/groups/{group_id}/data_sources",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get initial data source access",
    )
    .await;
    assert_eq!(initial["restricted"].as_bool(), Some(true));

    // Widen: an admin bypasses the caller-reach check on the restricted -> unrestricted move.
    let widen_resp = client
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(widen_resp.status().as_u16(), 200);

    let widened = ok_json(
        client
            .get(format!(
                "{}/api/groups/{group_id}/data_sources",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get data source access after widen",
    )
    .await;
    assert_eq!(widened["restricted"].as_bool(), Some(false));

    let set_resp = client
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_resp.status().as_u16(), 200);

    let after = ok_json(
        client
            .get(format!(
                "{}/api/groups/{group_id}/data_sources",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get data source access after set",
    )
    .await;
    assert_eq!(after["restricted"].as_bool(), Some(true));

    // Cross-org guard: a group id from a different org must 404, not leak/update it.
    use arcodash_core::test_support::new_organization;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    let mut other_org = new_organization();
    other_org.slug = Set(format!(
        "groups-cross-org-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    other_org.name = Set("Groups Cross-Org Test".to_string());
    let other_org = other_org.insert(&app.db).await.unwrap();
    let other_group =
        arcodash_core::test_support::new_group(other_org.id, "other-org-group", vec![])
            .insert(&app.db)
            .await
            .unwrap();

    let cross_org_resp = client
        .put(format!(
            "{}/api/groups/{}/data_source_access",
            app.address, other_group.id
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_org_resp.status().as_u16(), 404);
}

/// Re-attaching an attached role is a no-op: no duplicate `group.role_attached` event, still
/// success.
#[tokio::test]
async fn add_role_twice_records_one_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "role-idempotency-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let role = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": "Idempotency Test Role",
                "permissions": ["view_query"]
            }))
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let first = client
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200, "first attach must succeed");

    let second = client
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        200,
        "re-attaching an already-attached role must still return success"
    );

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let events = arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::Action.eq("group.role_attached"))
        .filter(arcodash_core::entities::event::Column::ObjectId.eq(group_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "idempotent re-attach must not record a duplicate audit event, got {events:?}"
    );
}

/// A stray `permissions` field on create is ignored: 200, and the response has no `permissions`
/// key.
#[tokio::test]
async fn create_group_ignores_stray_permissions_field() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "stray-perms-group", "permissions": ["admin"] }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "stray permissions field must not break create: {body_text}"
    );
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert!(
        body.get("permissions").is_none(),
        "group response must not have a permissions field: {body}"
    );
    let group_id = body["id"].as_i64().unwrap();

    // A member with no role attached must not get elevated access from the stray create-time
    // "permissions".
    let user_resp: serde_json::Value = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_user_id = user_resp["user"]["id"].as_i64().unwrap();
    let invited = ok_json(
        client
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({
                "name": "Stray Perms Member",
                "email": format!("stray-perms-{}@test.local", uuid::Uuid::new_v4().simple()),
                "groups": [group_id]
            }))
            .send()
            .await
            .unwrap(),
        "invite member of stray-perms group",
    )
    .await;
    let member_id = invited["id"].as_i64().unwrap();
    assert_ne!(member_id, admin_user_id);

    let effective = ok_json(
        client
            .get(format!(
                "{}/api/users/{member_id}/effective_permissions",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get effective permissions for stray-perms group member",
    )
    .await;
    assert_eq!(
        effective["is_admin"].as_bool(),
        Some(false),
        "membership alone (no role) must never confer admin, even with a stray create-time \
         permissions field: {effective}"
    );
    assert!(
        effective["permissions"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "a group with no role attached must grant no effective permissions: {effective}"
    );
}

/// GET /api/groups/:id never echoes a `permissions` field.
#[tokio::test]
async fn get_group_response_has_no_permissions_field() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "get-no-perms-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = created["id"].as_i64().unwrap();

    let got = ok_json(
        client
            .get(format!("{}/api/groups/{group_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get group",
    )
    .await;
    assert!(
        got.get("permissions").is_none(),
        "GET group response must not have a permissions field: {got}"
    );
}

/// Attaching a real role still flows through `load_effective_groups`.
#[tokio::test]
async fn group_with_real_role_still_grants_effective_permissions() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "real-role-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let role = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": "Real Role For Group",
                "permissions": ["view_query", "create_query"]
            }))
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let attach = client
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(attach.status().as_u16(), 200);

    let invited = ok_json(
        client
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({
                "name": "Real Role Member",
                "email": format!("real-role-{}@test.local", uuid::Uuid::new_v4().simple()),
                "groups": [group_id]
            }))
            .send()
            .await
            .unwrap(),
        "invite member of real-role group",
    )
    .await;
    let member_id = invited["id"].as_i64().unwrap();

    let effective = ok_json(
        client
            .get(format!(
                "{}/api/users/{member_id}/effective_permissions",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get effective permissions for real-role group member",
    )
    .await;
    let perms = effective["permissions"]
        .as_array()
        .expect("permissions must be an array");
    let has = |k: &str| perms.iter().any(|p| p.as_str() == Some(k));
    assert!(
        has("view_query"),
        "role permissions must flow through: {effective}"
    );
    assert!(
        has("create_query"),
        "role permissions must flow through: {effective}"
    );
}

/// `assign_data_source` twice for the same pair: both 200, one `data_source_groups` row, one audit
/// event.
#[tokio::test]
async fn assign_data_source_twice_records_one_row_and_one_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "dup-assign-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "dup-assign-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    for _ in 0..2 {
        let resp = client
            .post(format!(
                "{}/api/groups/{group_id}/data_sources",
                app.address
            ))
            .json(&serde_json::json!({ "data_source_id": ds_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "each assign call must return 200"
        );
    }

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let rows = arcodash_core::entities::data_source_group::Entity::find()
        .filter(arcodash_core::entities::data_source_group::Column::GroupId.eq(group_id))
        .filter(arcodash_core::entities::data_source_group::Column::DataSourceId.eq(ds_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "double-assign must not duplicate the row, got {rows:?}"
    );

    let events = arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::Action.eq("group.data_source_assigned"))
        .filter(arcodash_core::entities::event::Column::ObjectId.eq(group_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "double-assign must not record a duplicate audit event, got {events:?}"
    );
}

/// Best-effort: ~8 concurrent `assign_data_source` calls for a fresh pair all return 2xx and leave
/// one row.
#[tokio::test]
async fn assign_data_source_concurrent_calls_never_500_and_leave_one_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(
                &serde_json::json!({ "name": "concurrent-assign-ds", "type": "pg", "options": {} }),
            )
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "concurrent-assign-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let address = app.address.clone();
        tasks.push(tokio::spawn(async move {
            client
                .post(format!("{address}/api/groups/{group_id}/data_sources"))
                .json(&serde_json::json!({ "data_source_id": ds_id }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    let mut statuses: Vec<u16> = Vec::with_capacity(tasks.len());
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    assert!(
        statuses.iter().all(|s| (200..300).contains(s)),
        "every concurrent assign must return 2xx (zero 500s from a losing race): {statuses:?}"
    );

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let rows = arcodash_core::entities::data_source_group::Entity::find()
        .filter(arcodash_core::entities::data_source_group::Column::GroupId.eq(group_id))
        .filter(arcodash_core::entities::data_source_group::Column::DataSourceId.eq(ds_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "concurrent double-assign must not duplicate the row, got {rows:?}"
    );
}

/// Two concurrent membership edits on the SAME user, each adding a different group, must both
/// compose (no whole-array clobber).
#[tokio::test]
async fn concurrent_add_member_to_two_groups_both_survive() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let user_id = admin_user_id(&app.db, org_id).await;

    let group_a = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "concurrent-add-a" }))
            .send()
            .await
            .unwrap(),
        "create group a",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    let group_b = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "concurrent-add-b" }))
            .send()
            .await
            .unwrap(),
        "create group b",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let add_a = client
        .post(format!("{}/api/groups/{group_a}/members", app.address))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send();
    let add_b = client
        .post(format!("{}/api/groups/{group_b}/members", app.address))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send();
    let (resp_a, resp_b) = tokio::join!(add_a, add_b);
    assert_eq!(resp_a.unwrap().status().as_u16(), 200, "add to group a");
    assert_eq!(resp_b.unwrap().status().as_u16(), 200, "add to group b");

    for (group_id, label) in [(group_a, "a"), (group_b, "b")] {
        let members: serde_json::Value = client
            .get(format!("{}/api/groups/{group_id}/members", app.address))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let found = members
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_i64() == Some(user_id));
        assert!(
            found,
            "user must be a member of group {label} after the concurrent add; members: {members}"
        );
    }
}

/// Best-effort: ~8 concurrent `add_role` calls for a fresh pair all return 2xx and leave one
/// `group_roles` row.
#[tokio::test]
async fn add_role_concurrent_calls_never_500_and_leave_one_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let role = ok_json(
        client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": "Concurrent Attach Role",
                "permissions": ["view_query"]
            }))
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
            .json(&serde_json::json!({ "name": "concurrent-attach-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let address = app.address.clone();
        tasks.push(tokio::spawn(async move {
            client
                .post(format!("{address}/api/groups/{group_id}/roles"))
                .json(&serde_json::json!({ "role_id": role_id }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    let mut statuses: Vec<u16> = Vec::with_capacity(tasks.len());
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    assert!(
        statuses.iter().all(|s| (200..300).contains(s)),
        "every concurrent attach must return 2xx (zero 500s from a losing race): {statuses:?}"
    );

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let rows = arcodash_core::entities::group_role::Entity::find()
        .filter(arcodash_core::entities::group_role::Column::GroupId.eq(group_id))
        .filter(arcodash_core::entities::group_role::Column::RoleId.eq(role_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "concurrent double-attach must not duplicate the row, got {rows:?}"
    );
}

// Last-admin guard: `require_admin_remains` must block a mutation that would leave zero enabled
// admins (detaching the admin role, removing the sole admin, deleting the group). These use the
// shared "default" org, the only one that can authenticate over HTTP in the OSS binary;
// `app.cleanup()` leaves it with exactly one admin.

async fn builtin_group_id(db: &sea_orm::DatabaseConnection, org_id: i64, name: &str) -> i64 {
    use arcodash_core::entities::group;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.eq(name))
        .one(db)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("builtin group {name} must exist"))
        .id
}

async fn role_id_for_group(db: &sea_orm::DatabaseConnection, group_id: i64) -> i64 {
    use arcodash_core::entities::group_role;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .one(db)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("group {group_id} must carry at least one role"))
        .role_id
}

async fn admin_user_id(db: &sea_orm::DatabaseConnection, org_id: i64) -> i64 {
    use arcodash_core::entities::user;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .id
}

/// Creates a second independent admin-conferring group + role + enabled user, to prove the guard
/// only fires on the last admin.
async fn insert_second_admin(db: &sea_orm::DatabaseConnection, org_id: i64) {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveValue::*, EntityTrait};

    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "second-admin-role-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec!["admin".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "second-admin-group-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
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

    let email = format!("second-admin-{}@example.com", uuid::Uuid::new_v4().simple());
    user::Entity::insert(user::ActiveModel {
        org_id: Set(org_id),
        name: Set("Second Admin".to_string()),
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
    .unwrap();
}

/// Restores a `group_roles(group_id, role_id)` link on drop if missing, so a panicking test can't
/// leave a migration-seeded group without its role. Runs on a dedicated thread/runtime with a fresh
/// connection because `Drop` can't `.await` and a pooled sqlx connection is bound to its original
/// runtime's reactor.
struct RestoreGroupRoleOnDrop {
    group_id: i64,
    role_id: i64,
}

impl Drop for RestoreGroupRoleOnDrop {
    fn drop(&mut self) {
        use arcodash_core::entities::group_role;
        use sea_orm::{ActiveValue::*, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
        let group_id = self.group_id;
        let role_id = self.role_id;
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .expect("build runtime to restore group_roles row")
                .block_on(async move {
                    let db_url = std::env::var("TEST_DATABASE_URL")
                        .expect("TEST_DATABASE_URL must still be set to restore group_roles");
                    let db = arcodash_core::db::connect(&db_url)
                        .await
                        .expect("connect to restore group_roles row");
                    let still_linked = group_role::Entity::find()
                        .filter(group_role::Column::GroupId.eq(group_id))
                        .filter(group_role::Column::RoleId.eq(role_id))
                        .count(&db)
                        .await
                        .unwrap_or(0);
                    if still_linked == 0 {
                        let _ = group_role::Entity::insert(group_role::ActiveModel {
                            group_id: Set(group_id),
                            role_id: Set(role_id),
                            ..Default::default()
                        })
                        .exec(&db)
                        .await;
                    }
                });
        })
        .join()
        .expect("restore-group-role thread panicked");
    }
}

/// `DELETE /api/groups/{id}/roles/{role_id}` is rejected when `id` is the org's only
/// admin-conferring group.
#[tokio::test]
async fn remove_role_blocked_when_it_is_the_orgs_only_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_role_id = role_id_for_group(&app.db, admin_group_id).await;

    let resp = client
        .delete(format!(
            "{}/api/groups/{admin_group_id}/roles/{admin_role_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "remove_role: {body}");
    assert!(
        body.contains("cannot remove the last admin"),
        "unexpected error body: {body}"
    );

    use arcodash_core::entities::group_role;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let still_linked = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(admin_group_id))
        .filter(group_role::Column::RoleId.eq(admin_role_id))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        still_linked, 1,
        "the role-group link must survive the rejected removal"
    );
}

/// The same `remove_role` call succeeds once a second, independent admin-conferring group exists.
#[tokio::test]
async fn remove_role_allowed_with_a_second_admin_present() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_role_id = role_id_for_group(&app.db, admin_group_id).await;
    insert_second_admin(&app.db, org_id).await;

    // This detach succeeds, so guard the builtin group's role link: `cleanup` doesn't reattach
    // roles to migration-seeded groups.
    let _restore_admin_role = RestoreGroupRoleOnDrop {
        group_id: admin_group_id,
        role_id: admin_role_id,
    };

    let resp = client
        .delete(format!(
            "{}/api/groups/{admin_group_id}/roles/{admin_role_id}",
            app.address
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

/// Concurrent `remove_role` on two admin-conferring groups by a caller with `manage_groups` but not
/// `admin`: the grant ceiling rejects both and both links survive.
#[tokio::test]
async fn concurrent_remove_role_by_a_non_admin_never_detaches_either_admin_role() {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    // As in the sibling test: strip the seeded admin's membership so the two purpose-built groups
    // are the only path to admin.
    let seeded_admin_id = admin_user_id(&app.db, org_id).await;
    let seeded = user::Entity::find_by_id(seeded_admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: user::ActiveModel = seeded.into();
    active.groups = Set(vec![]);
    active.update(&app.db).await.unwrap();

    async fn admin_group_with_role(db: &sea_orm::DatabaseConnection, org_id: i64) -> (i64, i64) {
        let role_id = role::Entity::insert(role::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!(
                "role-detach-role-{}",
                uuid::Uuid::new_v4().simple()
            )),
            permissions: Set(vec!["admin".to_string()]),
            r#type: Set("custom".to_string()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id;
        let group_id = group::Entity::insert(group::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!(
                "role-detach-group-{}",
                uuid::Uuid::new_v4().simple()
            )),
            permissions: Set(vec![]),
            r#type: Set("regular".to_string()),
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
            "role-detach-admin-{}@example.com",
            uuid::Uuid::new_v4().simple()
        );
        user::Entity::insert(user::ActiveModel {
            org_id: Set(org_id),
            name: Set("Role Detach Admin".to_string()),
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
        .unwrap();
        (group_id, role_id)
    }

    let (group_a, role_a) = admin_group_with_role(&app.db, org_id).await;
    let (group_b, role_b) = admin_group_with_role(&app.db, org_id).await;
    let (manager, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let client = api_key_client(&test_api_key(&manager.email));

    let remove_a = client.delete(format!(
        "{}/api/groups/{group_a}/roles/{role_a}",
        app.address
    ));
    let remove_b = client.delete(format!(
        "{}/api/groups/{group_b}/roles/{role_b}",
        app.address
    ));
    let (resp_a, resp_b) = tokio::join!(remove_a.send(), remove_b.send());
    let status_a = resp_a.unwrap().status().as_u16();
    let status_b = resp_b.unwrap().status().as_u16();

    let remaining_links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.is_in([group_a, group_b]))
        .filter(group_role::Column::RoleId.is_in([role_a, role_b]))
        .all(&app.db)
        .await
        .unwrap()
        .len();
    assert_eq!(
        remaining_links, 2,
        "a non-admin manage_groups holder must not be able to detach either admin-granting role"
    );

    let mut statuses = [status_a, status_b];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [403, 403],
        "both detaches must be rejected by the grant ceiling, not the last-admin check; got {status_a} and {status_b}"
    );
}

/// The admin-caller counterpart: the caller passes the grant ceiling, so this exercises the
/// last-admin race directly, with two concurrent detaches (one on the caller's own group). Exactly
/// one must win.
#[tokio::test]
async fn concurrent_remove_role_by_an_admin_never_leaves_zero_admins() {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    // As in the non-admin variant: strip the seeded admin's membership.
    let seeded_admin_id = admin_user_id(&app.db, org_id).await;
    let seeded = user::Entity::find_by_id(seeded_admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: user::ActiveModel = seeded.into();
    active.groups = Set(vec![]);
    active.update(&app.db).await.unwrap();

    async fn admin_group_with_role(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
    ) -> (i64, i64, String) {
        let role_id = role::Entity::insert(role::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!(
                "role-detach-admin-role-{}",
                uuid::Uuid::new_v4().simple()
            )),
            permissions: Set(vec!["admin".to_string()]),
            r#type: Set("custom".to_string()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id;
        let group_id = group::Entity::insert(group::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!(
                "role-detach-admin-group-{}",
                uuid::Uuid::new_v4().simple()
            )),
            permissions: Set(vec![]),
            r#type: Set("regular".to_string()),
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
            "role-detach-admin-caller-{}@example.com",
            uuid::Uuid::new_v4().simple()
        );
        user::Entity::insert(user::ActiveModel {
            org_id: Set(org_id),
            name: Set("Role Detach Admin".to_string()),
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
        .unwrap();
        (group_id, role_id, email)
    }

    let (group_a, role_a, email_a) = admin_group_with_role(&app.db, org_id).await;
    let (group_b, role_b, _email_b) = admin_group_with_role(&app.db, org_id).await;
    // The caller is `group_a`'s own admin, so both detaches usually pass the grant ceiling and race
    // on the advisory-lock guard.
    let client = api_key_client(&test_api_key(&email_a));

    let remove_a = client.delete(format!(
        "{}/api/groups/{group_a}/roles/{role_a}",
        app.address
    ));
    let remove_b = client.delete(format!(
        "{}/api/groups/{group_b}/roles/{role_b}",
        app.address
    ));
    let (resp_a, resp_b) = tokio::join!(remove_a.send(), remove_b.send());
    let status_a = resp_a.unwrap().status().as_u16();
    let status_b = resp_b.unwrap().status().as_u16();

    let remaining_links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.is_in([group_a, group_b]))
        .filter(group_role::Column::RoleId.is_in([role_a, role_b]))
        .all(&app.db)
        .await
        .unwrap()
        .len();
    assert_eq!(
        remaining_links, 1,
        "exactly one of the two detaches must be stopped"
    );

    // If the caller's detach commits first, the other request fails the grant ceiling (403) instead
    // of the last-admin guard (400).
    assert!(
        matches!((status_a, status_b), (200, 400) | (400, 200) | (200, 403)),
        "exactly one detach must be rejected; got {status_a} and {status_b}"
    );
}

/// `DELETE /api/groups/{id}/members/{user_id}` is rejected when it would remove the org's sole
/// admin from their only admin-conferring group.
#[tokio::test]
async fn remove_member_blocked_when_target_is_the_orgs_sole_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_id = admin_user_id(&app.db, org_id).await;

    let resp = client
        .delete(format!(
            "{}/api/groups/{admin_group_id}/members/{admin_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "remove_member: {body}");
    assert!(
        body.contains("cannot remove the last admin"),
        "unexpected error body: {body}"
    );

    use arcodash_core::entities::user;
    use sea_orm::EntityTrait;
    let refetched = user::Entity::find_by_id(admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.groups.contains(&admin_group_id),
        "the rejected removal must not have touched the user's groups"
    );
}

/// Removing the sole admin from a group that does NOT confer admin must succeed: the guard must
/// know which group a removal targets so it only fires when admin is actually lost.
#[tokio::test]
async fn remove_member_from_a_non_admin_group_succeeds_even_as_the_orgs_sole_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_id = admin_user_id(&app.db, org_id).await;

    let plain_group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "no-admin-here" }))
            .send()
            .await
            .unwrap(),
        "create non-admin group",
    )
    .await;
    let plain_group_id = plain_group["id"].as_i64().unwrap();

    let add_resp = client
        .post(format!(
            "{}/api/groups/{plain_group_id}/members",
            app.address
        ))
        .json(&serde_json::json!({ "user_id": admin_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        add_resp.status().as_u16(),
        200,
        "{}",
        add_resp.text().await.unwrap_or_default()
    );

    let resp = client
        .delete(format!(
            "{}/api/groups/{plain_group_id}/members/{admin_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "remove from non-admin group: {body}");

    use arcodash_core::entities::user;
    use sea_orm::EntityTrait;
    let refetched = user::Entity::find_by_id(admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !refetched.groups.contains(&plain_group_id),
        "removal must actually take effect"
    );
    assert!(
        refetched.groups.contains(&admin_group_id),
        "the user's separate admin membership must be untouched"
    );
}

/// The same `remove_member` call succeeds once a second, independent admin exists.
#[tokio::test]
async fn remove_member_allowed_with_a_second_admin_present() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_id = admin_user_id(&app.db, org_id).await;
    insert_second_admin(&app.db, org_id).await;

    let resp = client
        .delete(format!(
            "{}/api/groups/{admin_group_id}/members/{admin_id}",
            app.address
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

/// Two admins, each sole holder of a separate admin-conferring group, removed concurrently by a
/// `manage_groups` non-admin: each is safe alone, so a naive guard could let both land (write
/// skew). Exactly one must win.
#[tokio::test]
async fn concurrent_removal_of_two_separate_sole_admins_never_leaves_zero_admins() {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    // API-key auth resolves to the seeded "default" org over HTTP, so strip the seeded admin's
    // membership, leaving the two purpose-built admins as the only path to admin.
    let seeded_admin_id = admin_user_id(&app.db, org_id).await;
    {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let seeded = user::Entity::find_by_id(seeded_admin_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        let mut active: user::ActiveModel = seeded.into();
        active.groups = Set(vec![]);
        active.update(&app.db).await.unwrap();
    }

    async fn admin_in_own_group(db: &sea_orm::DatabaseConnection, org_id: i64) -> (i64, i64) {
        let role_id = role::Entity::insert(role::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("admin-role-{}", uuid::Uuid::new_v4().simple())),
            permissions: Set(vec!["admin".to_string()]),
            r#type: Set("custom".to_string()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id;
        let group_id = group::Entity::insert(group::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("admin-group-{}", uuid::Uuid::new_v4().simple())),
            permissions: Set(vec![]),
            r#type: Set("regular".to_string()),
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
        let email = format!("solo-admin-{}@example.com", uuid::Uuid::new_v4().simple());
        let user_id = user::Entity::insert(user::ActiveModel {
            org_id: Set(org_id),
            name: Set("Solo Admin".to_string()),
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
        (user_id, group_id)
    }

    let (admin_a, group_a) = admin_in_own_group(&app.db, org_id).await;
    let (admin_b, group_b) = admin_in_own_group(&app.db, org_id).await;
    let (manager, _) = user_with_role(&app.db, org_id, &["manage_groups"]).await;
    let client = api_key_client(&test_api_key(&manager.email));

    let remove_a = client.delete(format!(
        "{}/api/groups/{group_a}/members/{admin_a}",
        app.address
    ));
    let remove_b = client.delete(format!(
        "{}/api/groups/{group_b}/members/{admin_b}",
        app.address
    ));
    let (resp_a, resp_b) = tokio::join!(remove_a.send(), remove_b.send());
    let status_a = resp_a.unwrap().status().as_u16();
    let status_b = resp_b.unwrap().status().as_u16();

    // Checked before the status codes: the remaining-admin count is a more sensitive detector of
    // the lock regressing.
    let remaining_admins = user::Entity::find()
        .filter(user::Column::Id.is_in([admin_a, admin_b]))
        .filter(user::Column::DisabledAt.is_null())
        .all(&app.db)
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.groups.contains(&group_a) || u.groups.contains(&group_b))
        .count();
    assert_eq!(
        remaining_admins, 1,
        "the org must end with exactly one admin, never zero and never both"
    );

    let mut statuses = [status_a, status_b];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [200, 400],
        "exactly one removal must be rejected as the last admin; got {status_a} and {status_b}"
    );
}

/// `DELETE /api/groups/{id}` on a custom group is rejected when it is the org's only
/// admin-conferring group (the builtin group is stripped of its role first).
#[tokio::test]
async fn delete_blocked_when_custom_group_is_the_orgs_only_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let builtin_admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_id = admin_user_id(&app.db, org_id).await;

    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    // Strip the builtin group's role link directly (fixture setup); the guard restores it on every
    // exit path since `cleanup` doesn't reseed it.
    let original_role_id = role_id_for_group(&app.db, builtin_admin_group_id).await;
    let _restore_builtin_role = RestoreGroupRoleOnDrop {
        group_id: builtin_admin_group_id,
        role_id: original_role_id,
    };
    group_role::Entity::delete_many()
        .filter(group_role::Column::GroupId.eq(builtin_admin_group_id))
        .exec(&app.db)
        .await
        .unwrap();

    let custom_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("only-admin-role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["admin".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let custom_group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "only-admin-group-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(custom_group_id),
        role_id: Set(custom_role_id),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap();

    let admin_row = user::Entity::find_by_id(admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: user::ActiveModel = admin_row.into();
    active.groups = Set(vec![custom_group_id]);
    active.update(&app.db).await.unwrap();

    let resp = client
        .delete(format!("{}/api/groups/{custom_group_id}", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "delete: {body}");
    assert!(
        body.contains("cannot remove the last admin"),
        "unexpected error body: {body}"
    );

    assert!(
        group::Entity::find_by_id(custom_group_id)
            .one(&app.db)
            .await
            .unwrap()
            .is_some(),
        "the rejected delete must not have removed the group"
    );
}

/// `POST /api/roles/{id}` stripping `admin` from a custom role is rejected when it's the org's only
/// path to an admin-conferring group.
#[tokio::test]
async fn role_update_stripping_admin_blocked_when_it_is_the_orgs_only_admin_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let builtin_admin_group_id = builtin_group_id(&app.db, org_id, "admin").await;
    let admin_id = admin_user_id(&app.db, org_id).await;

    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    let original_role_id = role_id_for_group(&app.db, builtin_admin_group_id).await;
    let _restore_builtin_role = RestoreGroupRoleOnDrop {
        group_id: builtin_admin_group_id,
        role_id: original_role_id,
    };
    group_role::Entity::delete_many()
        .filter(group_role::Column::GroupId.eq(builtin_admin_group_id))
        .exec(&app.db)
        .await
        .unwrap();

    let custom_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!("only-admin-role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["admin".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let custom_group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "only-admin-group-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(custom_group_id),
        role_id: Set(custom_role_id),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap();

    let admin_row = user::Entity::find_by_id(admin_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: user::ActiveModel = admin_row.into();
    active.groups = Set(vec![custom_group_id]);
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/roles/{custom_role_id}", app.address))
        .json(&serde_json::json!({ "permissions": [] }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "role update: {body}");
    assert!(
        body.contains("cannot remove the last admin"),
        "unexpected error body: {body}"
    );

    let reloaded = role::Entity::find_by_id(custom_role_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.permissions,
        vec!["admin".to_string()],
        "the rejected update must not have changed the role's permissions"
    );
}

/// The same role update succeeds when the builtin `admin` group stays untouched and the stripped
/// role belongs to an unrelated group.
#[tokio::test]
async fn role_update_stripping_admin_allowed_when_not_the_orgs_only_admin_role() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    use arcodash_core::entities::{group, group_role, role};
    use sea_orm::{ActiveValue::*, EntityTrait};

    let custom_role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "removable-admin-role-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec!["admin".to_string()]),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let custom_group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(format!(
            "removable-admin-group-{}",
            uuid::Uuid::new_v4().simple()
        )),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(custom_group_id),
        role_id: Set(custom_role_id),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap();

    let resp = client
        .post(format!("{}/api/roles/{custom_role_id}", app.address))
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

    let reloaded = role::Entity::find_by_id(custom_role_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reloaded.permissions.is_empty(),
        "the allowed update must have cleared the role's permissions"
    );
}

/// A duplicate id in `groups` resolves to one row, so the supplied count must be deduped before
/// comparing, else a valid id sent twice reads as unknown.
#[tokio::test]
async fn invite_with_duplicate_group_id_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let group = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "dedup-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let email = format!("dedup-invite-{}@example.com", uuid::Uuid::new_v4().simple());
    let resp = client
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": "Dedup Invitee",
            "email": email,
            "groups": [group_id, group_id]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "invite with duplicate group id: {body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();

    let groups: Vec<i64> = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert!(
        !groups.is_empty(),
        "created user must not end up group-less; got {body}"
    );
    let unique: std::collections::HashSet<i64> = groups.iter().copied().collect();
    assert_eq!(
        unique,
        std::collections::HashSet::from([group_id]),
        "created user's group membership must resolve to exactly the one requested group; got {groups:?}"
    );
}

/// An id naming no row is still rejected and named in the error; deduping only removes the false
/// positive from duplicates.
#[tokio::test]
async fn invite_with_unknown_group_id_returns_400_naming_it() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let email = format!(
        "unknown-group-invite-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );
    let resp = client
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": "Unknown Group Invitee",
            "email": email,
            "groups": [999999999]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "invite with unknown group id: {body}");
    assert!(
        body.contains("999999999"),
        "error body must name the unknown id; got: {body}"
    );
}

/// A group id from another organization is rejected like an unknown id (lookups are scoped by
/// `org_id`), and the user isn't created.
#[tokio::test]
async fn invite_with_cross_org_group_id_returns_400_and_does_not_create_user() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let other_org_id = common::make_org(&app.db).await;
    use arcodash_core::entities::group;
    use sea_orm::{ActiveValue::Set, EntityTrait};
    let other_org_group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(other_org_id),
        name: Set(format!("cross-org-group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap()
    .last_insert_id;

    let email = format!(
        "cross-org-invite-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );
    let resp = client
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": "Cross Org Invitee",
            "email": email,
            "groups": [other_org_group_id]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "invite with cross-org group id: {body}");

    use arcodash_core::entities::user;
    use sea_orm::{ColumnTrait, QueryFilter};
    let created = user::Entity::find()
        .filter(user::Column::Email.eq(email.clone()))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        created.is_none(),
        "rejected invite must not create the user row; found: {created:?}"
    );
}

/// Two distinct, real, in-org group ids must both attach — deduping duplicate ids must not
/// affect the all-valid, no-duplicates case.
#[tokio::test]
async fn invite_with_two_valid_group_ids_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let group_a = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "valid-group-a" }))
            .send()
            .await
            .unwrap(),
        "create group a",
    )
    .await;
    let group_a_id = group_a["id"].as_i64().unwrap();

    let group_b = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "valid-group-b" }))
            .send()
            .await
            .unwrap(),
        "create group b",
    )
    .await;
    let group_b_id = group_b["id"].as_i64().unwrap();

    let email = format!(
        "two-groups-invite-{}@example.com",
        uuid::Uuid::new_v4().simple()
    );
    let body = ok_json(
        client
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({
                "name": "Two Groups Invitee",
                "email": email,
                "groups": [group_a_id, group_b_id]
            }))
            .send()
            .await
            .unwrap(),
        "invite with two valid group ids",
    )
    .await;

    let groups: std::collections::HashSet<i64> = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(
        groups,
        std::collections::HashSet::from([group_a_id, group_b_id]),
        "created user must hold both requested groups; got {body}"
    );
}

/// Creating a second group with a name already used in the same org must be a 409 with a
/// readable message, not a raw DB error string or a 500.
#[tokio::test]
async fn create_group_duplicate_name_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "dup-name-group" }))
            .send()
            .await
            .unwrap(),
        "create first group",
    )
    .await;

    let resp = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "dup-name-group" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 409, "duplicate group name: {body}");
    assert!(
        body.contains("already exists"),
        "must be a readable message, not a raw DB error; got: {body}"
    );
}

/// Renaming a group onto an existing name in the same org must be a 409, and the rejected
/// rename must not partially apply — the group being renamed keeps its original name.
#[tokio::test]
async fn rename_group_onto_existing_name_returns_409_and_does_not_apply() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "X" }))
            .send()
            .await
            .unwrap(),
        "create group X",
    )
    .await;

    let group_y = ok_json(
        client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "Y" }))
            .send()
            .await
            .unwrap(),
        "create group Y",
    )
    .await;
    let group_y_id = group_y["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/groups/{group_y_id}", app.address))
        .json(&serde_json::json!({ "name": "X" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 409, "rename onto existing name: {body}");

    let after = ok_json(
        client
            .get(format!("{}/api/groups/{group_y_id}", app.address))
            .send()
            .await
            .unwrap(),
        "fetch group Y after rejected rename",
    )
    .await;
    assert_eq!(
        after["name"], "Y",
        "rejected rename must not partially apply; got {after}"
    );
}
