//! Integration tests: per-object ACL endpoints for queries and dashboards (grant/list/revoke, and
//! that a grant makes a query reachable).

mod common;
use common::ok_json;
use common::{authed_client, TestApp};

// ── shared helpers ──

/// Gives `user_id` a fresh role+group carrying exactly `perms`; reads need the `view_<obj>`
/// capability on top of reach.
async fn grant_capability(db: &sea_orm::DatabaseConnection, user_id: i64, perms: &[&str]) {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let u = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        data_source_access_restricted: Set(false),
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

    let mut groups = u.groups.clone();
    groups.push(group_id);
    let mut active: user::ActiveModel = u.into();
    active.groups = Set(groups);
    active.update(db).await.unwrap();
}

/// Create a data source (required before creating a query).
async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "acl-test-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a bare query and return its id.
async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "ACL Test Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a dashboard and return its (id, slug).
async fn create_dashboard(client: &reqwest::Client, base: &str) -> (i64, String) {
    let body = ok_json(
        client
            .post(format!("{base}/api/dashboards"))
            .json(&serde_json::json!({ "name": "ACL Test Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let id = body["id"].as_i64().unwrap();
    let slug = body["slug"].as_str().unwrap().to_string();
    (id, slug)
}

/// Adds a bare text tile (`publish` refuses an empty dashboard).
async fn add_widget(client: &reqwest::Client, base: &str, dashboard_id: i64) {
    let resp = client
        .post(format!("{base}/api/widgets"))
        .json(&serde_json::json!({ "dashboard_id": dashboard_id, "text": "tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "create widget");
}

/// Invite a user (admin only) and return their id.
async fn invite_user(client: &reqwest::Client, base: &str, email: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/users"))
            .json(&serde_json::json!({
                "name": "Grantee User",
                "email": email,
                "groups": []
            }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Reads the invitation token from the DB (it is emailed, never returned), accepts the invitation,
/// logs in, and returns the new user's client.
async fn onboard_and_login(
    app: &TestApp,
    user_id: i64,
    email: &str,
    password: &str,
) -> reqwest::Client {
    use sea_orm::EntityTrait;
    let user = arcodash_core::entities::user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let token = user.details["invitation_token"]
        .as_str()
        .expect("invited user must have details.invitation_token")
        .to_string();

    // invitation/accept is CSRF-exempt, so a bare client is fine.
    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let accept = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Grantee User",
            "password": password
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        accept.status().as_u16(),
        200,
        "accept invitation: {}",
        accept.text().await.unwrap_or_default()
    );

    let (session, csrf) = app.login(email, password).await;
    authed_client(&app.address, &session, &csrf)
}

// ── query ACL tests ──

/// GET /api/queries/:id/acl on owned query → 200, empty array.
#[tokio::test]
async fn list_query_acl_empty_on_new_query() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let resp = client
        .get(format!("{}/api/queries/{query_id}/acl", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "ACL must be empty on a fresh query; got: {body}"
    );
}

/// POST /api/queries/:id/acl → 200 PermissionResponse with grantor_id/grantee_id set.
#[tokio::test]
async fn grant_query_acl_returns_permission_response() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let grantee_id = invite_user(&client, &app.address, "grantee1@test.local").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "modify" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "grant query acl: {body_text}");

    let perm: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(perm["object_type"], "query");
    assert_eq!(perm["object_id"], query_id);
    // "modify" is the legacy wire value; the grant ladder normalizes it to "editor" on store.
    assert_eq!(perm["access_type"], "editor");
    assert_eq!(perm["grantee_id"], grantee_id);
    assert!(!perm["id"].is_null(), "permission must have an id");
    assert!(
        !perm["grantor_id"].is_null(),
        "permission must have a grantor_id"
    );
}

/// After grant, the grantee can GET /api/queries/:id (authz-integrity test).
#[tokio::test]
async fn grantee_can_access_query_after_grant() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let grantee_email = "grantee2@test.local";
    let grantee_id = invite_user(&admin_client, &app.address, grantee_email).await;
    let grantee_client = onboard_and_login(&app, grantee_id, grantee_email, "Gr@ntee1Pass!").await;
    grant_capability(&app.db, grantee_id, &["view_query"]).await;

    // Grantee cannot access query without a grant (no data-source group access).
    let before = grantee_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        before.status().as_u16(),
        404,
        "grantee must not see query before grant"
    );

    let grant_resp = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant_resp.status().as_u16(), 200);

    let after = grantee_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status().as_u16(),
        200,
        "grantee must see query after grant"
    );
}

/// DELETE /api/queries/:id/acl/:perm_id → 204; grantee loses access.
#[tokio::test]
async fn revoke_query_acl_removes_access() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let grantee_email = "grantee3@test.local";
    let grantee_id = invite_user(&admin_client, &app.address, grantee_email).await;
    let grantee_client = onboard_and_login(&app, grantee_id, grantee_email, "Gr@ntee2Pass!").await;
    grant_capability(&app.db, grantee_id, &["view_query"]).await;

    let grant: serde_json::Value = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let perm_id = grant["id"].as_i64().unwrap();

    let before = grantee_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        before.status().as_u16(),
        200,
        "must be accessible before revoke"
    );

    let revoke = admin_client
        .delete(format!(
            "{}/api/queries/{query_id}/acl/{perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204, "revoke must return 204");

    let after = grantee_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status().as_u16(),
        404,
        "grantee must lose access after revoke"
    );
}

/// DELETE /api/queries/:id/acl/:perm_id with nonexistent perm_id → 404.
#[tokio::test]
async fn revoke_nonexistent_query_perm_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let resp = client
        .delete(format!("{}/api/queries/{query_id}/acl/999999", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// grant_query_acl on a query the caller cannot access → 404.
#[tokio::test]
async fn grant_query_acl_on_inaccessible_query_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    // A non-admin caller who does NOT own and cannot see the query.
    let caller_email = "caller@test.local";
    let caller_id = invite_user(&admin_client, &app.address, caller_email).await;
    let caller_client = onboard_and_login(&app, caller_id, caller_email, "C@ller1Pass!").await;

    let grantee_id = invite_user(&admin_client, &app.address, "grantee_x@test.local").await;

    // The handler runs query_service::get_for_user first, which 404s before grant logic.
    let resp = caller_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "grant on inaccessible query must return 404"
    );
}

/// grant with an invalid access_type → 400 (validated against the owner/editor/viewer ladder plus
/// legacy wire values).
#[tokio::test]
async fn grant_query_acl_with_invalid_access_type_returns_400() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let grantee_id = invite_user(&client, &app.address, "badtype@test.local").await;

    let resp = client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "superuser" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(
        status, 400,
        "invalid access_type must be rejected with 400 (got {status}); \
         permission_service::grant must validate access_type"
    );
}

// ── dashboard ACL tests ──

/// GET /api/dashboards/:id/acl → 200, empty array on fresh dashboard.
#[tokio::test]
async fn list_dashboard_acl_empty_on_new_dashboard() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&client, &app.address).await;

    let resp = client
        .get(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "dashboard ACL must be empty on creation; got: {body}"
    );
}

/// POST /api/dashboards/:id/acl → 200 PermissionResponse.
#[tokio::test]
async fn grant_dashboard_acl_returns_permission_response() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&client, &app.address).await;
    let grantee_id = invite_user(&client, &app.address, "dash-grantee1@test.local").await;

    let resp = client
        .post(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "modify" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "grant dashboard acl: {body_text}");

    let perm: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(perm["object_type"], "dashboard");
    assert_eq!(perm["object_id"], dashboard_id);
    // "modify" is the legacy wire value; the grant ladder normalizes it to "editor" on store.
    assert_eq!(perm["access_type"], "editor");
    assert_eq!(perm["grantee_id"], grantee_id);
    assert!(!perm["id"].is_null());
    assert!(!perm["grantor_id"].is_null());
}

/// After grant, ACL list includes the new entry.
#[tokio::test]
async fn list_dashboard_acl_shows_granted_entry() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&client, &app.address).await;
    let grantee_id = invite_user(&client, &app.address, "dash-grantee2@test.local").await;

    client
        .post(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();

    let list_resp = client
        .get(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list: serde_json::Value = list_resp.json().await.unwrap();
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 1, "ACL must have exactly one entry");
    assert_eq!(entries[0]["grantee_id"], grantee_id);
}

/// DELETE /api/dashboards/:id/acl/:perm_id → 204; ACL empty again.
#[tokio::test]
async fn revoke_dashboard_acl_removes_entry() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&client, &app.address).await;
    let grantee_id = invite_user(&client, &app.address, "dash-grantee3@test.local").await;

    let grant: serde_json::Value = client
        .post(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let perm_id = grant["id"].as_i64().unwrap();

    let revoke = client
        .delete(format!(
            "{}/api/dashboards/{dashboard_id}/acl/{perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204);

    let list: serde_json::Value = client
        .get(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        list.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "ACL must be empty after revoke; got: {list}"
    );
}

/// DELETE /api/dashboards/:id/acl/:perm_id with nonexistent id → 404.
#[tokio::test]
async fn revoke_nonexistent_dashboard_perm_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&client, &app.address).await;

    let resp = client
        .delete(format!(
            "{}/api/dashboards/{dashboard_id}/acl/999999",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// DELETE /api/dashboards/:dashboard_id/acl/:perm_id where `perm_id` belongs to a *query* ACL must
/// 404 (the route's target id is checked against the row) and leave the query grant intact.
#[tokio::test]
async fn revoke_dashboard_acl_rejects_a_perm_id_belonging_to_a_query() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;
    let (dashboard_id, _slug) = create_dashboard(&admin_client, &app.address).await;

    let grantee_email = "cross-object-grantee@test.local";
    let grantee_id = invite_user(&admin_client, &app.address, grantee_email).await;
    grant_capability(&app.db, grantee_id, &["view_query"]).await;

    let grant: serde_json::Value = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_perm_id = grant["id"].as_i64().unwrap();

    let resp = admin_client
        .delete(format!(
            "{}/api/dashboards/{dashboard_id}/acl/{query_perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a perm_id from a different object/object_type must 404, not revoke"
    );

    // The query ACL grant must be untouched.
    let query_acl = ok_json(
        admin_client
            .get(format!("{}/api/queries/{query_id}/acl", app.address))
            .send()
            .await
            .unwrap(),
        "list query acl",
    )
    .await;
    assert_eq!(
        query_acl.as_array().unwrap().len(),
        1,
        "the query's own ACL grant must survive the rejected cross-object revoke"
    );
}

/// Auth boundary: a never-published draft dashboard is invisible to a non-admin non-owner, who gets
/// the uniform 404 rather than a 403 that confirms it exists.
#[tokio::test]
async fn non_owner_with_no_view_reach_cannot_grant_dashboard_acl_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    // Admin owns this dashboard; it is never published, so no one else can see it yet.
    let (dashboard_id, _slug) = create_dashboard(&admin_client, &app.address).await;

    let caller_email = "dash-caller@test.local";
    let caller_id = invite_user(&admin_client, &app.address, caller_email).await;
    let caller_client = onboard_and_login(&app, caller_id, caller_email, "C@ller2Pass!").await;

    let grantee_id = invite_user(&admin_client, &app.address, "dash-grantee-x@test.local").await;

    let resp = caller_client
        .post(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a caller who can't even view the dashboard must not learn it exists via a 403"
    );
}

/// Once published, the same non-admin caller can view the dashboard but isn't owner/admin, so
/// sharing is a 403, not hidden.
#[tokio::test]
async fn non_owner_who_can_view_cannot_grant_dashboard_acl_returns_403() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let (dashboard_id, slug) = create_dashboard(&admin_client, &app.address).await;
    add_widget(&admin_client, &app.address, dashboard_id).await;
    let publish_resp = admin_client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200, "publish dashboard");

    let caller_email = "dash-caller-viewer@test.local";
    let caller_id = invite_user(&admin_client, &app.address, caller_email).await;
    let caller_client = onboard_and_login(&app, caller_id, caller_email, "C@ller3Pass!").await;

    let grantee_id = invite_user(&admin_client, &app.address, "dash-grantee-y@test.local").await;

    let resp = caller_client
        .post(format!("{}/api/dashboards/{dashboard_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "only the owner or an admin can share a dashboard, even one the caller can view"
    );
}

/// grant on a genuinely nonexistent dashboard id → 404 (must stay 404, not become anything else).
#[tokio::test]
async fn grant_dashboard_acl_on_nonexistent_id_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let grantee_id = invite_user(&admin_client, &app.address, "dash-grantee-z@test.local").await;

    let resp = admin_client
        .post(format!("{}/api/dashboards/999999999/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// `PUT /api/dashboards/:id/visibility` with no view reach on a real, same-org dashboard → 404.
#[tokio::test]
async fn non_owner_with_no_view_reach_cannot_set_dashboard_visibility_returns_404() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let (dashboard_id, _slug) = create_dashboard(&admin_client, &app.address).await;

    let caller_email = "dash-vis-caller@test.local";
    let caller_id = invite_user(&admin_client, &app.address, caller_email).await;
    let caller_client = onboard_and_login(&app, caller_id, caller_email, "C@ller4Pass!").await;

    let resp = caller_client
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "public" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a caller who can't even view the dashboard must not learn it exists via a 403"
    );
}

/// `PUT /api/dashboards/:id/visibility` for a caller who can view the published dashboard but isn't
/// owner/admin → 403.
#[tokio::test]
async fn non_owner_who_can_view_cannot_set_dashboard_visibility_returns_403() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let (dashboard_id, slug) = create_dashboard(&admin_client, &app.address).await;
    add_widget(&admin_client, &app.address, dashboard_id).await;
    let publish_resp = admin_client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200, "publish dashboard");

    let caller_email = "dash-vis-caller-viewer@test.local";
    let caller_id = invite_user(&admin_client, &app.address, caller_email).await;
    let caller_client = onboard_and_login(&app, caller_id, caller_email, "C@ller5Pass!").await;

    let resp = caller_client
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "public" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "must be the object owner or an admin to change visibility, even one the caller can view"
    );
}

// Owner-grant downgrade guard: only an admin, or the owner grant's own holder demoting themselves,
// may downgrade an existing "owner" grant (an owner-tier grantee can otherwise share the object).

/// A non-admin owner-tier grantee (C) downgrading a co-owner's (B's) grant → 403; B's grant is
/// unchanged.
#[tokio::test]
async fn non_admin_owner_cannot_downgrade_another_owner_grant() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let b_email = "owner-b@test.local";
    let b_id = invite_user(&admin_client, &app.address, b_email).await;
    grant_capability(&app.db, b_id, &["view_query"]).await;
    onboard_and_login(&app, b_id, b_email, "OwnerB1Pass!").await;

    let c_email = "owner-c@test.local";
    let c_id = invite_user(&admin_client, &app.address, c_email).await;
    // edit_query too, so C reaches the downgrade guard this test targets.
    grant_capability(&app.db, c_id, &["view_query", "edit_query"]).await;
    let c_client = onboard_and_login(&app, c_id, c_email, "OwnerC1Pass!").await;

    for grantee in [b_id, c_id] {
        let resp = admin_client
            .post(format!("{}/api/queries/{query_id}/acl", app.address))
            .json(&serde_json::json!({ "user_id": grantee, "access_type": "owner" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "admin owner grant must succeed"
        );
    }

    let resp = c_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": b_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a non-admin owner-tier grantee must not be able to downgrade another owner grant"
    );

    let list: serde_json::Value = admin_client
        .get(format!("{}/api/queries/{query_id}/acl", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let b_entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["grantee_id"] == b_id)
        .expect("B's grant must still exist");
    assert_eq!(
        b_entry["access_type"], "owner",
        "B's grant must be unchanged after the rejected downgrade attempt"
    );
}

/// An admin CAN downgrade another user's owner-tier grant.
#[tokio::test]
async fn admin_can_downgrade_owner_grant() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let b_email = "owner-admin-b@test.local";
    let b_id = invite_user(&admin_client, &app.address, b_email).await;
    grant_capability(&app.db, b_id, &["view_query"]).await;
    onboard_and_login(&app, b_id, b_email, "OwnerAdmB1Pass!").await;

    let grant_resp = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": b_id, "access_type": "owner" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant_resp.status().as_u16(), 200);

    let downgrade = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": b_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    let status = downgrade.status().as_u16();
    let body_text = downgrade.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "an admin must be able to downgrade an owner grant: {body_text}"
    );
    let perm: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(perm["access_type"], "viewer");
}

/// An owner-tier grantee CAN downgrade their OWN owner grant (self-demotion).
#[tokio::test]
async fn owner_can_self_demote_own_owner_grant() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let b_email = "owner-self-b@test.local";
    let b_id = invite_user(&admin_client, &app.address, b_email).await;
    // edit_query too: sharing needs edit capability on top of owner-tier reach.
    grant_capability(&app.db, b_id, &["view_query", "edit_query"]).await;
    let b_client = onboard_and_login(&app, b_id, b_email, "OwnerSelfB1Pass!").await;

    let grant_resp = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": b_id, "access_type": "owner" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant_resp.status().as_u16(), 200);

    let self_demote = b_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": b_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    let status = self_demote.status().as_u16();
    let body_text = self_demote.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "self-demotion of one's own owner grant must succeed: {body_text}"
    );
    let perm: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(perm["access_type"], "viewer");
}

/// An ordinary viewer→editor upgrade by an owner-tier grantee still works; the guard only fires on
/// downgrading an "owner" grant.
#[tokio::test]
async fn owner_can_upgrade_viewer_grant_to_editor() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    let c_email = "owner-upgrade-c@test.local";
    let c_id = invite_user(&admin_client, &app.address, c_email).await;
    // edit_query too: sharing needs edit capability.
    grant_capability(&app.db, c_id, &["view_query", "edit_query"]).await;
    let c_client = onboard_and_login(&app, c_id, c_email, "OwnerUpgC1Pass!").await;

    let d_email = "owner-upgrade-d@test.local";
    let d_id = invite_user(&admin_client, &app.address, d_email).await;
    grant_capability(&app.db, d_id, &["view_query"]).await;
    onboard_and_login(&app, d_id, d_email, "OwnerUpgD1Pass!").await;

    // C is owner-tier; D starts as a plain viewer.
    let c_grant = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": c_id, "access_type": "owner" }))
        .send()
        .await
        .unwrap();
    assert_eq!(c_grant.status().as_u16(), 200);
    let d_grant = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": d_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(d_grant.status().as_u16(), 200);

    let upgrade = c_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": d_id, "access_type": "editor" }))
        .send()
        .await
        .unwrap();
    let status = upgrade.status().as_u16();
    let body_text = upgrade.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "a normal viewer->editor upgrade by an owner must still work: {body_text}"
    );
    let perm: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(perm["access_type"], "editor");
}

// Owner-tier reach alone must not authorize `grant`/`revoke`/`set_visibility`; all three also
// require edit capability.

/// An owner-tier grantee without `edit_query` is denied grant, revoke and visibility change.
#[tokio::test]
async fn owner_grant_without_edit_capability_denied_on_grant_revoke_and_visibility() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;
    let query_id = create_query(&admin_client, &app.address, ds_id).await;

    // Owner-tier ACL grant, but the role carries only view_query — no edit_query.
    let grantee_email = "owner-no-cap@test.local";
    let grantee_id = invite_user(&admin_client, &app.address, grantee_email).await;
    grant_capability(&app.db, grantee_id, &["view_query"]).await;
    let grantee_client =
        onboard_and_login(&app, grantee_id, grantee_email, "OwnerNoCap1Pass!").await;

    let owner_grant = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": grantee_id, "access_type": "owner" }))
        .send()
        .await
        .unwrap();
    assert_eq!(owner_grant.status().as_u16(), 200);

    let target_id = invite_user(
        &admin_client,
        &app.address,
        "owner-no-cap-target@test.local",
    )
    .await;

    let grant_attempt = grantee_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": target_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        grant_attempt.status().as_u16(),
        403,
        "owner-tier reach without edit_query capability must not be able to grant"
    );

    // DELETE /acl/:perm_id -> 403 (revoking a grant they didn't make).
    let target_grant: serde_json::Value = admin_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": target_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let target_perm_id = target_grant["id"].as_i64().unwrap();

    let revoke_attempt = grantee_client
        .delete(format!(
            "{}/api/queries/{query_id}/acl/{target_perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke_attempt.status().as_u16(),
        403,
        "owner-tier reach without edit_query capability must not be able to revoke"
    );

    let visibility_attempt = grantee_client
        .put(format!("{}/api/queries/{query_id}/visibility", app.address))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        visibility_attempt.status().as_u16(),
        403,
        "owner-tier reach without edit_query capability must not be able to change visibility"
    );
}

/// A creator holding `edit_query`, and an admin, still succeed on all three actions.
#[tokio::test]
async fn owner_with_edit_capability_and_admin_still_succeed_on_grant_revoke_and_visibility() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let ds_id = create_ds(&admin_client, &app.address).await;

    // A non-admin creator with create_query + edit_query owns the query and clears both gates.
    let creator_email = "owner-with-cap-creator@test.local";
    let creator_id = invite_user(&admin_client, &app.address, creator_email).await;
    grant_capability(
        &app.db,
        creator_id,
        &["create_query", "edit_query", "view_query"],
    )
    .await;
    let creator_client = onboard_and_login(&app, creator_id, creator_email, "OwnerCap1Pass!").await;

    let query_id = create_query(&creator_client, &app.address, ds_id).await;
    let target_id = invite_user(
        &admin_client,
        &app.address,
        "owner-with-cap-target@test.local",
    )
    .await;

    let grant_resp = creator_client
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": target_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        grant_resp.status().as_u16(),
        200,
        "creator with edit_query must still be able to grant"
    );
    let perm_id = grant_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let visibility_resp = creator_client
        .put(format!("{}/api/queries/{query_id}/visibility", app.address))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        visibility_resp.status().as_u16(),
        204,
        "creator with edit_query must still be able to change visibility"
    );

    let revoke_resp = creator_client
        .delete(format!(
            "{}/api/queries/{query_id}/acl/{perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke_resp.status().as_u16(),
        204,
        "creator with edit_query must still be able to revoke"
    );

    // Admin control: same three actions succeed on a fresh query it neither owns nor was granted.
    let query_id_2 = create_query(&admin_client, &app.address, ds_id).await;
    let target_id_2 = invite_user(
        &admin_client,
        &app.address,
        "owner-with-cap-target2@test.local",
    )
    .await;

    let admin_grant = admin_client
        .post(format!("{}/api/queries/{query_id_2}/acl", app.address))
        .json(&serde_json::json!({ "user_id": target_id_2, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_grant.status().as_u16(),
        200,
        "admin must still be able to grant"
    );
    let admin_perm_id = admin_grant.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let admin_visibility = admin_client
        .put(format!(
            "{}/api/queries/{query_id_2}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_visibility.status().as_u16(),
        204,
        "admin must still be able to change visibility"
    );

    let admin_revoke = admin_client
        .delete(format!(
            "{}/api/queries/{query_id_2}/acl/{admin_perm_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_revoke.status().as_u16(),
        204,
        "admin must still be able to revoke"
    );
}
