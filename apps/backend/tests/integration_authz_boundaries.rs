//! Authorization boundary integration tests: non-admin 403s, data-source-scoped query visibility
//! (404), DS access via group assignment, and unauthenticated / CSRF-missing 401/403s. Skipped when
//! `TEST_DATABASE_URL` is unset.

mod common;
use common::{authed_client, spawn_app, ADMIN_EMAIL, ADMIN_PASSWORD};

/// Bare client with no cookies, no CSRF.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Invite a new user via the admin client, accept, and log in; returns `(user_id, authed_client)`.
async fn invite_and_login(
    app: &common::TestApp,
    admin: &reqwest::Client,
    email: &str,
    name: &str,
    password: &str,
    group_ids: Vec<i64>,
) -> (i64, reqwest::Client) {
    let invite_resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": name,
            "email": email,
            "groups": group_ids
        }))
        .send()
        .await
        .unwrap();
    let status = invite_resp.status().as_u16();
    let body_text = invite_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "invite user: {body_text}");
    let invite_body: serde_json::Value = serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("invite body not JSON ({e}): {body_text}"));
    let user_id = invite_body["id"].as_i64().expect("invite must return id");

    let token = {
        use arcodash_core::entities::user;
        use sea_orm::EntityTrait;
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

    let accept_client = bare();
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
        .json(&serde_json::json!({
            "token": token,
            "name": name,
            "password": password
        }))
        .send()
        .await
        .unwrap();
    let status = accept_resp.status().as_u16();
    let body_text = accept_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "accept invitation: {body_text}");

    let (session, csrf_login) = app.login(email, password).await;
    let client = authed_client(&app.address, &session, &csrf_login);

    (user_id, client)
}

/// POST /api/groups → 403 for a non-admin user.
#[tokio::test]
async fn non_admin_cannot_create_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "nonadmin@test.local",
        "Non Admin",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "forbidden-group" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin POST /groups must be 403"
    );
}

/// POST /api/destinations → 403 for a non-admin user.
#[tokio::test]
async fn non_admin_cannot_create_destination() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "nonadmin2@test.local",
        "Non Admin2",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .post(format!("{}/api/destinations", app.address))
        .json(&serde_json::json!({ "name": "dst", "type": "email", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin POST /destinations must be 403"
    );
}

/// GET /api/organization → 403 for a non-admin user.
#[tokio::test]
async fn non_admin_cannot_get_organization() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "nonadmin3@test.local",
        "Non Admin3",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .get(format!("{}/api/organization", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin GET /organization must be 403"
    );
}

/// GET /api/events → 403 for a non-admin user.
#[tokio::test]
async fn non_admin_cannot_list_events() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "nonadmin4@test.local",
        "Non Admin4",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin GET /events must be 403"
    );
}

/// POST /api/users (invite) → 403 for a non-admin user.
#[tokio::test]
async fn non_admin_cannot_invite_users() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "nonadmin5@test.local",
        "Non Admin5",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": "Another User",
            "email": "another@test.local",
            "groups": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin POST /users must be 403"
    );
}

/// Non-admin without DS access: GET /api/queries/:id (owned by admin on an unassigned DS) → 404.
#[tokio::test]
async fn non_admin_without_ds_access_cannot_see_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds_resp = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "private-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(ds_resp.status().as_u16(), 200);
    let ds: serde_json::Value = ds_resp.json().await.unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q_resp = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Admin-Only Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(q_resp.status().as_u16(), 200);
    let q: serde_json::Value = q_resp.json().await.unwrap();
    let query_id = q["id"].as_i64().unwrap();

    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "noaccess@test.local",
        "No Access",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "non-admin without DS access must get 404 (not 403) to avoid enumeration"
    );
}

/// Non-admin granted DS access via group assignment can then list/get queries on that DS.
#[tokio::test]
async fn non_admin_with_ds_access_can_see_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds_resp = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "shared-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(ds_resp.status().as_u16(), 200);
    let ds: serde_json::Value = ds_resp.json().await.unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q_resp = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Shared Query",
            "query": "SELECT 42",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(q_resp.status().as_u16(), 200);
    let q: serde_json::Value = q_resp.json().await.unwrap();
    let query_id = q["id"].as_i64().unwrap();

    // Publish it: a draft is private to its owner, and this test covers data-source visibility.
    let pub_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(pub_resp.status().as_u16(), 200);

    let grp_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "ds-readers" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_resp.status().as_u16(), 200);
    let grp: serde_json::Value = grp_resp.json().await.unwrap();
    let group_id = grp["id"].as_i64().unwrap();

    // Groups default unrestricted; restrict this one so the allowlist assignment is load-bearing.
    let restrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict_resp.status().as_u16(), 200);

    let assign_resp = admin
        .post(format!(
            "{}/api/groups/{group_id}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_resp.status().as_u16(), 200);

    // DS access is only the reach half; the group also needs a role carrying `view_query`.
    let role_resp = admin
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({ "name": "ds-readers-role", "permissions": ["view_query"] }))
        .send()
        .await
        .unwrap();
    let role_status = role_resp.status().as_u16();
    let role_body = role_resp.text().await.unwrap_or_default();
    assert_eq!(role_status, 200, "role create failed: {role_body}");
    let role: serde_json::Value = serde_json::from_str(&role_body).unwrap();
    let role_id = role["id"].as_i64().unwrap();
    let role_assign_resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_assign_resp.status().as_u16(), 200);

    let (uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dsreader@test.local",
        "DS Reader",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    let get_resp = user_client
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    let status = get_resp.status().as_u16();
    let body = get_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "user with DS access must see query: {body}");
    let query_json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(query_json["id"].as_i64(), Some(query_id));

    let list_resp = user_client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list: serde_json::Value = list_resp.json().await.unwrap();
    let results = if list.is_array() {
        list.as_array().unwrap().clone()
    } else {
        list["results"].as_array().unwrap().clone()
    };
    assert!(
        results.iter().any(|q| q["id"].as_i64() == Some(query_id)),
        "query {query_id} must appear in list for user with DS access; list: {list}"
    );

    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_id}/data_sources/{ds_id}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_id}", app.address))
        .send()
        .await;
}

/// Create-block union rule: a user whose every group is restricted cannot create a data source and
/// `can_create` reflects that; un-restricting the group flips both back.
#[tokio::test]
async fn create_blocked_when_every_group_restricted_can_create_reflects_it() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let grp_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "ds-creators" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_resp.status().as_u16(), 200);
    let grp: serde_json::Value = grp_resp.json().await.unwrap();
    let group_id = grp["id"].as_i64().unwrap();

    let role_resp = admin
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({ "name": "ds-creators-role", "permissions": ["create_data_source"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_resp.status().as_u16(), 200);
    let role: serde_json::Value = role_resp.json().await.unwrap();
    let role_id = role["id"].as_i64().unwrap();
    let role_assign_resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_assign_resp.status().as_u16(), 200);

    // Restrict the group up front (empty allowlist) so its lone member has no unrestricted group.
    let restrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict_resp.status().as_u16(), 200);

    let (uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dscreator@test.local",
        "DS Creator",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    let can_create_resp = user_client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(can_create_resp.status().as_u16(), 200);
    let can_create_body: serde_json::Value = can_create_resp.json().await.unwrap();
    assert_eq!(
        can_create_body["can_create"].as_bool(),
        Some(false),
        "can_create must be false when every group is restricted"
    );

    let blocked_resp = user_client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "blocked-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked_resp.status().as_u16(),
        403,
        "create must be blocked when every group is restricted"
    );

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

    let can_create_resp = user_client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    let can_create_body: serde_json::Value = can_create_resp.json().await.unwrap();
    assert_eq!(
        can_create_body["can_create"].as_bool(),
        Some(true),
        "can_create must be true once any group is unrestricted"
    );

    let allowed_resp = user_client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "allowed-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let allowed_status = allowed_resp.status().as_u16();
    let allowed_body = allowed_resp.text().await.unwrap_or_default();
    assert_eq!(
        allowed_status, 200,
        "create must be allowed once any group is unrestricted: {allowed_body}"
    );

    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_id}", app.address))
        .send()
        .await;
}

/// Same-group-tied create rule: `create_data_source` on a restricted group plus a different
/// unrestricted group must NOT let the user create; both must land on the same group.
#[tokio::test]
async fn create_blocked_when_capability_and_unrestricted_are_on_different_groups() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let grp_a_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "ds-capable-restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_a_resp.status().as_u16(), 200);
    let grp_a: serde_json::Value = grp_a_resp.json().await.unwrap();
    let group_a_id = grp_a["id"].as_i64().unwrap();

    let role_resp = admin
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({ "name": "ds-creators-role-2", "permissions": ["create_data_source"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_resp.status().as_u16(), 200);
    let role: serde_json::Value = role_resp.json().await.unwrap();
    let role_id = role["id"].as_i64().unwrap();
    let role_assign_resp = admin
        .post(format!("{}/api/groups/{group_a_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_assign_resp.status().as_u16(), 200);

    let restrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_a_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict_resp.status().as_u16(), 200);

    let grp_b_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "unrestricted-powerless" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_b_resp.status().as_u16(), 200);
    let grp_b: serde_json::Value = grp_b_resp.json().await.unwrap();
    let group_b_id = grp_b["id"].as_i64().unwrap();

    let (uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dsgapuser@test.local",
        "DS Gap User",
        "P@ssw0rd!",
        vec![group_a_id, group_b_id],
    )
    .await;

    let can_create_resp = user_client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(can_create_resp.status().as_u16(), 200);
    let can_create_body: serde_json::Value = can_create_resp.json().await.unwrap();
    assert_eq!(
        can_create_body["can_create"].as_bool(),
        Some(false),
        "can_create must be false when the capability-granting group is restricted, even with \
         an unrestricted-but-powerless second group"
    );

    let blocked_resp = user_client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "should-be-blocked-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked_resp.status().as_u16(),
        403,
        "create must stay blocked: capability and unrestricted posture are on different groups"
    );

    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_a_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_b_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_a_id}", app.address))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_b_id}", app.address))
        .send()
        .await;
}

/// Unauthenticated (no cookie) POST /api/queries → 401 or 403 (no session = no auth).
#[tokio::test]
async fn unauthenticated_post_queries_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = bare();
    let sess = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();

    let resp = client
        .post(format!("{}/api/queries", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "data_source_id": 1,
            "name": "Unauthorized Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "unauthenticated POST /queries must be 401 or 403, got {status}"
    );
}

/// Unauthenticated POST /api/data_sources → 401 or 403.
#[tokio::test]
async fn unauthenticated_post_data_sources_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = bare();
    let sess = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "name": "forbidden-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "unauthenticated POST /data_sources must be 401 or 403, got {status}"
    );
}

/// POST /api/groups with a valid session but MISSING the X-CSRF-Token header → 403.
#[tokio::test]
async fn post_without_csrf_header_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (session, csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::COOKIE,
        reqwest::header::HeaderValue::from_str(&format!(
            "arcodash_session={session}; csrf_token={csrf}"
        ))
        .unwrap(),
    );
    let client = reqwest::Client::builder()
        .cookie_store(false)
        .default_headers(headers)
        .build()
        .unwrap();

    let resp = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "csrf-test-group" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "POST without X-CSRF-Token must be 403 (CSRF double-submit check)"
    );
}

/// Unauthenticated POST /api/users (invite) → 401 or 403.
#[tokio::test]
async fn unauthenticated_post_users_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = bare();
    let sess = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();

    let resp = client
        .post(format!("{}/api/users", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "name": "Ghost",
            "email": "ghost@test.local",
            "groups": []
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "unauthenticated POST /users must be 401 or 403, got {status}"
    );
}

/// Unauthenticated POST /api/destinations → 401 or 403.
#[tokio::test]
async fn unauthenticated_post_destinations_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = bare();
    let sess = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();

    let resp = client
        .post(format!("{}/api/destinations", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "name": "x", "type": "email", "options": {} }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "unauthenticated POST /destinations must be 401 or 403, got {status}"
    );
}

/// Postgres bumps `xmin` on every `UPDATE`, which proves a row was written exactly once (an
/// `updated_at` snapshot can't).
async fn row_xmin(db: &sea_orm::DatabaseConnection, user_id: i64) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    let stmt = Statement::from_sql_and_values(
        db.get_database_backend(),
        r#"SELECT xmin::text::bigint AS xmin FROM users WHERE id = $1"#,
        [user_id.into()],
    );
    let row = db
        .query_one_raw(stmt)
        .await
        .expect("xmin query failed")
        .expect("user row must exist");
    row.try_get::<i64>("", "xmin").unwrap()
}

/// Under `REMOTE_USER_HEADER` auth, one request to a `limit_by_user` route must not write the
/// caller's row more than once (both extractions resolve the same user); an unchanged repeat writes
/// zero times, so `xmin` advancing by more than 1 is what's ruled out.
#[tokio::test]
async fn remote_user_header_resolves_caller_once_per_request() {
    let app = skip_if_none!(
        common::spawn_app_with(|c| {
            c.remote_user_header = Some("X-Remote-User".to_string());
        })
        .await
    );
    app.cleanup().await;

    let email = format!("remote-user-{}@example.com", uuid::Uuid::new_v4().simple());
    let client = reqwest::Client::new();

    let provision = client
        .get(format!(
            "{}/api/queries/999999/parameter_values",
            app.address
        ))
        .header("X-Remote-User", &email)
        .send()
        .await
        .unwrap();
    assert_ne!(
        provision.status().as_u16(),
        401,
        "the remote-user header must auto-provision and authenticate a user"
    );

    use arcodash_core::entities::user;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let user_row = user::Entity::find()
        .filter(user::Column::Email.eq(email.clone()))
        .one(&app.db)
        .await
        .unwrap()
        .expect("remote-user header must have provisioned a user row");

    let xmin_before = row_xmin(&app.db, user_row.id).await;

    // A GET on a `limit_by_user` route resolves the caller twice (middleware and handler); nothing
    // changed since provisioning, so neither should write.
    let _ = client
        .get(format!(
            "{}/api/queries/999999/parameter_values",
            app.address
        ))
        .header("X-Remote-User", &email)
        .send()
        .await
        .unwrap();

    let xmin_after = row_xmin(&app.db, user_row.id).await;
    assert!(
        xmin_after - xmin_before <= 1,
        "two resolutions of the same unchanged caller must never both write: xmin advanced by {}",
        xmin_after - xmin_before
    );
}
